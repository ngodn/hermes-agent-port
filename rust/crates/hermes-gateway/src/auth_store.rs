//! Auth-store reads and profile pool shadowing from hermes_cli/auth.py.
//! Reading rows does not select, refresh or lease a usable credential.
#![allow(dead_code)]
use serde_json::{json, Value};
use std::{collections::HashSet, io, path::Path, sync::Mutex};

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

pub(crate) struct AuthFileLock(std::fs::File);

impl AuthFileLock {
    fn acquire(auth_path: &Path) -> io::Result<Self> {
        Self::acquire_for(auth_path, std::time::Duration::from_secs(15))
    }

    pub(crate) fn acquire_for(auth_path: &Path, timeout: std::time::Duration) -> io::Result<Self> {
        let lock_path = auth_path.with_extension("lock");
        if let Some(parent) = lock_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(lock_path)?;
        let started = std::time::Instant::now();
        loop {
            match file.try_lock() {
                Ok(()) => return Ok(Self(file)),
                Err(std::fs::TryLockError::Error(error))
                    if error.kind() != io::ErrorKind::Interrupted =>
                {
                    return Err(error)
                }
                Err(std::fs::TryLockError::Error(_) | std::fs::TryLockError::WouldBlock) => {}
            }
            if started.elapsed() >= timeout {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "timed out waiting for auth store lock",
                ));
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }
}

impl Drop for AuthFileLock {
    fn drop(&mut self) {
        let _ = std::fs::File::unlock(&self.0);
    }
}

static WRITE_LOCK: Mutex<()> = Mutex::new(());

/// A provider-state read/modify/write lease whose file locks stay held across
/// an OAuth refresh. The active profile is always locked before a distinct
/// root fallback, matching Python's lock ordering for single-use grants.
pub(crate) struct ProviderStateTransaction {
    _locks: Vec<AuthFileLock>,
    source_path: std::path::PathBuf,
    store: Value,
}

impl ProviderStateTransaction {
    pub(crate) fn store(&self) -> &Value {
        &self.store
    }

    pub(crate) fn store_mut(&mut self) -> &mut Value {
        &mut self.store
    }

    pub(crate) fn source_path(&self) -> &Path {
        &self.source_path
    }

    /// Persist the whole source store while the corresponding lease is held.
    pub(crate) fn commit(&mut self) -> io::Result<()> {
        let root = self.store.as_object_mut().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "auth store is not an object")
        })?;
        root.entry("providers").or_insert_with(|| json!({}));
        root.insert("version".into(), json!(1));
        root.insert(
            "updated_at".into(),
            Value::String(chrono::Utc::now().to_rfc3339()),
        );
        let mut bytes = serde_json::to_vec_pretty(&self.store)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        bytes.push(b'\n');
        crate::atomic_file::write_private_preserving_symlink(&self.source_path, &bytes)
    }
}

fn provider_state_in<'a>(
    store: &'a Value,
    provider: &str,
) -> Option<&'a serde_json::Map<String, Value>> {
    store.get("providers")?.get(provider)?.as_object()
}

/// Lock and re-read the auth store that owns `providers.<provider>`.
///
/// A named profile may borrow provider state from the root auth store. The
/// returned transaction writes back to the exact source it read, so rotating a
/// single-use refresh token can never strand sibling profiles on the old pair.
pub(crate) fn lock_provider_state(
    profile_path: &Path,
    root_path: Option<&Path>,
    provider: &str,
    timeout: std::time::Duration,
) -> io::Result<Option<ProviderStateTransaction>> {
    let profile_lock = AuthFileLock::acquire_for(profile_path, timeout)?;
    let profile_store = load(profile_path)?;
    if provider_state_in(&profile_store, provider).is_some() {
        return Ok(Some(ProviderStateTransaction {
            _locks: vec![profile_lock],
            source_path: profile_path.to_path_buf(),
            store: profile_store,
        }));
    }

    let Some(root_path) = root_path.filter(|root| *root != profile_path) else {
        return Ok(None);
    };
    let root_lock = AuthFileLock::acquire_for(root_path, timeout)?;
    let root_store = load(root_path)?;
    if provider_state_in(&root_store, provider).is_none() {
        return Ok(None);
    }
    Ok(Some(ProviderStateTransaction {
        _locks: vec![profile_lock, root_lock],
        source_path: root_path.to_path_buf(),
        store: root_store,
    }))
}

fn merge_newer_disk_status(
    mut incoming: serde_json::Map<String, Value>,
    disk: Option<&serde_json::Map<String, Value>>,
    provider: &str,
    now: f64,
) -> serde_json::Map<String, Value> {
    const STATUS_FIELDS: &[&str] = &[
        "last_status",
        "last_status_at",
        "last_error_code",
        "last_error_reason",
        "last_error_message",
        "last_error_reset_at",
    ];
    let Some(disk) = disk else {
        return incoming;
    };
    if !matches!(
        disk.get("last_status").and_then(Value::as_str),
        Some("dead" | "exhausted")
    ) {
        return incoming;
    }
    let incoming_key = incoming
        .get("access_token")
        .and_then(Value::as_str)
        .unwrap_or("");
    let disk_key = disk
        .get("access_token")
        .and_then(Value::as_str)
        .unwrap_or("");
    if !incoming_key.is_empty() && !disk_key.is_empty() && incoming_key != disk_key {
        return incoming;
    }
    let disk_at = disk
        .get("last_status_at")
        .and_then(crate::credential_pool::absolute_timestamp)
        .unwrap_or(0.0);
    let incoming_at = incoming
        .get("last_status_at")
        .and_then(crate::credential_pool::absolute_timestamp)
        .unwrap_or(0.0);
    if disk_at <= incoming_at {
        return incoming;
    }
    if disk.get("last_status").and_then(Value::as_str) == Some("exhausted") {
        let disk_value = Value::Object(disk.clone());
        let Ok(entry) = crate::credential_pool::PooledCredential::from_dict(provider, &disk_value)
        else {
            return incoming;
        };
        if entry.cooldown_until(false).is_none_or(|until| until <= now) {
            return incoming;
        }
    }
    for field in STATUS_FIELDS {
        incoming.insert(
            (*field).into(),
            disk.get(*field).cloned().unwrap_or(Value::Null),
        );
    }
    incoming
}

/// Persist one provider pool with Python's concurrent-add and newer-cooldown
/// merge. The profile store is the write authority for API-key pools.
pub fn write_pool(
    auth_path: &Path,
    provider: &str,
    entries: Vec<Value>,
    removed_ids: Vec<String>,
) -> io::Result<()> {
    let _process_guard = WRITE_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    let _file_guard = AuthFileLock::acquire(auth_path)?;
    let mut store = load(auth_path)?;
    let root = store
        .as_object_mut()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "auth store is not an object"))?;
    root.entry("providers").or_insert_with(|| json!({}));
    let pool = root
        .entry("credential_pool")
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "credential_pool is not an object",
            )
        })?;
    let existing = pool
        .get(provider)
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let existing_by_id: std::collections::HashMap<String, serde_json::Map<String, Value>> =
        existing
            .iter()
            .filter_map(Value::as_object)
            .filter_map(|entry| {
                entry
                    .get("id")
                    .and_then(Value::as_str)
                    .filter(|id| !id.is_empty())
                    .map(|id| (id.into(), entry.clone()))
            })
            .collect();
    let removed: HashSet<String> = removed_ids
        .into_iter()
        .filter(|id| !id.is_empty())
        .collect();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0.0, |duration| duration.as_secs_f64());
    let mut merged = Vec::with_capacity(entries.len() + existing.len());
    let mut incoming_ids = HashSet::new();
    for entry in entries {
        let Some(entry) = entry.as_object() else {
            merged.push(entry);
            continue;
        };
        let mut sanitized = crate::credential_persistence::sanitize(entry, provider);
        let id = sanitized
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned();
        if !id.is_empty() {
            incoming_ids.insert(id.clone());
        }
        sanitized = merge_newer_disk_status(sanitized, existing_by_id.get(&id), provider, now);
        merged.push(Value::Object(sanitized));
    }
    for disk in existing {
        let Some(id) = disk.get("id").and_then(Value::as_str) else {
            continue;
        };
        if incoming_ids.contains(id) || removed.contains(id) {
            continue;
        }
        let value = disk
            .as_object()
            .map(|entry| Value::Object(crate::credential_persistence::sanitize(entry, provider)))
            .unwrap_or(disk);
        merged.push(value);
    }
    pool.insert(provider.into(), Value::Array(merged));
    root.insert("version".into(), json!(1));
    root.insert(
        "updated_at".into(),
        Value::String(chrono::Utc::now().to_rfc3339()),
    );
    let mut bytes = serde_json::to_vec_pretty(&store)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    bytes.push(b'\n');
    crate::atomic_file::write_private_preserving_symlink(auth_path, &bytes)
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

    #[test]
    fn pool_writes_merge_concurrent_rows_and_newer_cooldowns() {
        let dir = std::env::temp_dir().join(format!(
            "hermes-auth-write-{}-{}",
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
        let path = dir.join("auth.json");
        let future = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs_f64()
            + 600.0;
        std::fs::write(
            &path,
            serde_json::to_vec(&serde_json::json!({
                "credential_pool":{"openrouter":[
                    {"id":"one","auth_type":"api_key","source":"manual","priority":0,"access_token":"key-one","last_status":"exhausted","last_status_at":future,"last_error_code":429},
                    {"id":"concurrent","auth_type":"api_key","source":"manual","priority":1,"access_token":"key-two"}
                ]}
            }))
            .unwrap(),
        )
        .unwrap();
        write_pool(
            &path,
            "openrouter",
            vec![serde_json::json!({
                "id":"one","auth_type":"api_key","source":"manual","priority":0,
                "access_token":"key-one","last_status":"ok","last_status_at":1.0
            })],
            Vec::new(),
        )
        .unwrap();
        let rows = load(&path).unwrap()["credential_pool"]["openrouter"]
            .as_array()
            .unwrap()
            .clone();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0]["last_status"], "exhausted");
        assert_eq!(rows[0]["last_error_code"], 429);
        assert_eq!(rows[1]["id"], "concurrent");

        write_pool(
            &path,
            "openrouter",
            vec![serde_json::json!({
                "id":"one","auth_type":"api_key","source":"manual","priority":0,
                "access_token":"rotated-key","last_status":"ok"
            })],
            vec!["concurrent".into()],
        )
        .unwrap();
        let rows = load(&path).unwrap()["credential_pool"]["openrouter"]
            .as_array()
            .unwrap()
            .clone();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["access_token"], "rotated-key");
        assert_eq!(rows[0]["last_status"], "ok");
    }

    #[cfg(unix)]
    #[test]
    fn pool_write_is_private_and_preserves_auth_symlink() {
        use std::os::unix::fs::{symlink, MetadataExt, PermissionsExt};

        let dir = std::env::temp_dir().join(format!(
            "hermes-auth-link-{}-{}",
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
        let target = dir.join("owned.json");
        let link = dir.join("auth.json");
        std::fs::write(&target, b"{}").unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o644)).unwrap();
        symlink(&target, &link).unwrap();
        let link_inode = std::fs::symlink_metadata(&link).unwrap().ino();

        write_pool(
            &link,
            "openrouter",
            vec![serde_json::json!({
                "id":"one","auth_type":"api_key","source":"manual","priority":0,
                "access_token":"secret"
            })],
            Vec::new(),
        )
        .unwrap();
        assert!(std::fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(std::fs::symlink_metadata(&link).unwrap().ino(), link_inode);
        assert_eq!(
            std::fs::metadata(&target).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            load(&link).unwrap()["credential_pool"]["openrouter"][0]["id"],
            "one"
        );
    }
}
