//! Auth-store reads and profile pool shadowing from hermes_cli/auth.py.
//! Reading rows does not select, refresh or lease a usable credential.
#![allow(dead_code)]
use serde_json::{json, Value};
use std::{io, path::Path};

fn empty_store() -> Value {
    json!({"version":1,"providers":{}})
}

/// Keep I/O failure distinct from corrupt content. A caller performing a later
/// read/modify/write must never mistake an unreadable store for an empty one.
pub fn load(path: &Path) -> io::Result<Value> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(empty_store()),
        Err(error) => return Err(error),
    };
    let bytes = bytes.strip_prefix(b"\xef\xbb\xbf").unwrap_or(&bytes);
    let raw = match serde_json::from_slice::<Value>(bytes) {
        Ok(raw) => raw,
        Err(_) => {
            // Preserve the original bytes before allowing recovery. Failure to
            // copy never changes the original file and does not conceal it.
            let preserved = std::fs::copy(path, path.with_extension("json.corrupt")).is_ok();
            tracing::warn!(path = %path.display(), preserved, "auth store could not be parsed; returning empty state without changing the original");
            return Ok(empty_store());
        }
    };
    normalize(raw)
}

fn normalize(mut raw: Value) -> io::Result<Value> {
    if raw["providers"].is_object() || raw["credential_pool"].is_object() {
        let object = raw.as_object_mut().expect("object with auth fields");
        object.entry("providers").or_insert_with(|| json!({}));
        if let Some(nous) = object
            .get_mut("providers")
            .and_then(|v| v.get_mut("nous"))
            .and_then(Value::as_object_mut)
        {
            let value = nous.get("portal_base_url").unwrap_or(&Value::Null);
            if crate::python_value::truthy(value) {
                let url = value.as_str().ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        "Nous portal_base_url must be a string",
                    )
                })?;
                if crate::local_probe::urlparse_hostname(
                    url.trim_matches(crate::python_value::python_whitespace),
                ) == "api.nousresearch.com"
                {
                    nous.insert(
                        "portal_base_url".into(),
                        json!("https://portal.nousresearch.com"),
                    );
                }
            }
        }
        return Ok(raw);
    }
    if let Some(systems) = raw["systems"].as_object() {
        let providers = match systems.get("nous_portal") {
            Some(nous) => json!({"nous":nous}),
            None => json!({}),
        };
        let active = if systems.contains_key("nous_portal") {
            json!("nous")
        } else {
            Value::Null
        };
        return Ok(json!({"version":1,"providers":providers,"active_provider":active}));
    }
    Ok(empty_store())
}

/// Profile rows shadow root rows per provider. Empty or malformed profile
/// slices permit root fallback. Global read failures cannot break a profile.
pub fn read_pool(
    profile_path: &Path,
    root_path: Option<&Path>,
    provider: Option<&str>,
) -> io::Result<Value> {
    let profile = load(profile_path)?;
    let root = root_path
        .and_then(|path| load(path).ok())
        .unwrap_or(Value::Null);
    Ok(merge_pool(&profile, &root, provider))
}

fn merge_pool(profile: &Value, root: &Value, provider: Option<&str>) -> Value {
    let profile = &profile["credential_pool"];
    let root = &root["credential_pool"];
    if let Some(provider) = provider {
        if let Some(rows) = profile[provider].as_array().filter(|rows| !rows.is_empty()) {
            return Value::Array(rows.clone());
        }
        return Value::Array(root[provider].as_array().cloned().unwrap_or_default());
    }
    let mut merged = profile.as_object().cloned().unwrap_or_default();
    if let Some(root) = root.as_object() {
        for (provider, rows) in root {
            if rows.as_array().is_some_and(|rows| !rows.is_empty())
                && merged
                    .get(provider)
                    .and_then(Value::as_array)
                    .is_none_or(|rows| rows.is_empty())
            {
                merged.insert(provider.clone(), rows.clone());
            }
        }
    }
    Value::Object(merged)
}

/// Suppression is profile-local and never borrows markers from the root store.
/// Mirror the reference's best-effort read and Python membership behavior.
pub fn source_suppressed(path: &Path, provider: &str, source: &str) -> bool {
    let Ok(store) = load(path) else {
        return false;
    };
    suppressed_in(&store, provider, source)
}

pub(crate) fn suppressed_in(store: &Value, provider: &str, source: &str) -> bool {
    match &store["suppressed_sources"][provider] {
        Value::Array(values) => values.iter().any(|value| value.as_str() == Some(source)),
        Value::Object(values) => values.contains_key(source),
        Value::String(value) => value.contains(source),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pool_shadowing_matches_python() {
        let rows: Value =
            serde_json::from_str(include_str!("../../../tools/auth-pool-read-goldens.json"))
                .unwrap();
        for row in rows.as_array().unwrap() {
            assert_eq!(
                merge_pool(&row["profile"], &row["root"], row["provider"].as_str()),
                row["result"],
                "{row}"
            );
        }
    }

    #[test]
    fn file_reads_preserve_credentials_and_surface_io_failure() {
        use base64::Engine;
        let dir = std::env::temp_dir().join(format!(
            "hermes-auth-read-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&dir).unwrap();
        struct Cleanup(std::path::PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let _cleanup = Cleanup(dir.clone());
        let profile = dir.join("profile.json");
        let root = dir.join("root.json");
        assert_eq!(load(&profile).unwrap(), empty_store());
        let rows: Value =
            serde_json::from_str(include_str!("../../../tools/auth-store-read-goldens.json"))
                .unwrap();
        for row in rows.as_array().unwrap() {
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(row["bytes"].as_str().unwrap())
                .unwrap();
            std::fs::write(&profile, &bytes).unwrap();
            let result = load(&profile);
            assert_eq!(result.is_err(), row["error"].as_bool().unwrap(), "{row}");
            if let Ok(result) = result {
                assert_eq!(result, row["result"], "{row}");
            }
            assert_eq!(std::fs::read(&profile).unwrap(), bytes);
        }
        std::fs::remove_file(&profile).unwrap();
        std::fs::write(
            &root,
            br#"{"credential_pool":{"openai-api":[{"access_token":"root-fixture"}]}}"#,
        )
        .unwrap();
        assert_eq!(
            read_pool(&profile, Some(&root), Some("openai-api")).unwrap()[0]["access_token"],
            "root-fixture"
        );
        let bytes = b"\xef\xbb\xbf{\"credential_pool\":{\"openai-api\":[{\"access_token\":\"profile-fixture\"}]}}";
        std::fs::write(&profile, bytes).unwrap();
        assert_eq!(
            read_pool(&profile, Some(&root), Some("openai-api")).unwrap()[0]["access_token"],
            "profile-fixture"
        );
        assert_eq!(std::fs::read(&profile).unwrap(), bytes);
        assert_eq!(
            read_pool(&profile, Some(&dir), Some("openai-api")).unwrap()[0]["access_token"],
            "profile-fixture"
        );
        assert!(read_pool(&dir, Some(&root), None).is_err());
        for bytes in [b"{broken".as_slice(), b"\xff\xfe"] {
            std::fs::write(&profile, bytes).unwrap();
            assert_eq!(load(&profile).unwrap(), empty_store());
            assert_eq!(
                std::fs::read(profile.with_extension("json.corrupt")).unwrap(),
                bytes
            );
            assert_eq!(std::fs::read(&profile).unwrap(), bytes);
        }
    }
}
