//! Environment-source discovery for the credential pool.
#![allow(dead_code)]
use crate::credential_pool::{upsert_entry, PooledCredential};
use anyhow::Result;
use serde_json::json;
use std::{collections::HashSet, path::Path};

pub struct EnvProvider {
    pub auth_type: String,
    pub base_url: String,
    pub base_url_env: Option<String>,
    pub key_vars: Vec<String>,
}

fn config_text(value: &serde_json::Value) -> String {
    if !crate::python_value::truthy(value) {
        return String::new();
    }
    value
        .as_str()
        .map(str::to_owned)
        .unwrap_or_else(|| crate::python_value::python_repr(value))
        .trim_matches(crate::python_value::python_whitespace)
        .to_owned()
}

fn custom_name(name: &str) -> String {
    name.trim_matches(crate::python_value::python_whitespace)
        .to_lowercase()
        .replace(' ', "-")
}

/// Consume the compatible custom-provider view, shared with request settings.
/// An explicit name wins over URL matching when multiple entries share a URL.
pub fn custom_pool_candidates(
    entries: &serde_json::Value,
    base_url: &str,
    provider_name: Option<&str>,
) -> Vec<String> {
    if base_url.is_empty() {
        return vec![];
    }
    let url = base_url
        .trim_matches(crate::python_value::python_whitespace)
        .trim_end_matches('/');
    let requested = provider_name.map(custom_name).unwrap_or_default();
    let alias = requested.strip_prefix("custom:").map(custom_name);
    let candidates: Vec<_> = entries
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|entry| {
            entry["name"]
                .as_str()
                .map(|name| (custom_name(name), entry))
        })
        .collect();
    let keys = |(name, entry): &(String, &serde_json::Value)| {
        let mut keys = vec![];
        let slug = custom_name(&config_text(&entry["provider_key"]));
        if !slug.is_empty() {
            keys.push(slug);
        }
        let legacy = format!("custom:{name}");
        if !name.is_empty() && !keys.contains(&legacy) {
            keys.push(legacy);
        }
        keys
    };
    if !requested.is_empty() {
        for pair @ (name, entry) in &candidates {
            let slug = custom_name(&config_text(&entry["provider_key"]));
            if [name.as_str(), slug.as_str()]
                .into_iter()
                .filter(|name| !name.is_empty())
                .any(|name| name == requested || alias.as_deref() == Some(name))
            {
                return keys(pair);
            }
        }
    }
    for pair @ (_, entry) in &candidates {
        let entry_url = config_text(&entry["base_url"]);
        let entry_url = entry_url.trim_end_matches('/');
        if !entry_url.is_empty() && entry_url == url {
            return keys(pair);
        }
    }
    vec![]
}

/// Seed custom-provider and model credentials from an already normalized
/// compatibility view. This does not write that derived view back to YAML.
pub fn seed_custom_from_config(
    pool: &str,
    entries: &mut Vec<PooledCredential>,
    config: &serde_json::Value,
    suppressed: impl FnMut(&str, &str) -> bool,
) -> Result<(bool, HashSet<String>)> {
    seed_custom_pool(
        pool,
        entries,
        &crate::custom_provider_config::compatible(config),
        config,
        suppressed,
    )
}

pub fn seed_custom_pool(
    pool: &str,
    entries: &mut Vec<PooledCredential>,
    custom: &serde_json::Value,
    config: &serde_json::Value,
    mut suppressed: impl FnMut(&str, &str) -> bool,
) -> Result<(bool, HashSet<String>)> {
    let mut changed = false;
    let mut active = HashSet::new();
    if let Some(suffix) = pool.strip_prefix("custom:") {
        if let Some(entry) = custom.as_array().into_iter().flatten().find(|entry| {
            entry["name"]
                .as_str()
                .is_some_and(|name| custom_name(name) == suffix)
        }) {
            let key = config_text(&entry["api_key"]);
            let base = config_text(&entry["base_url"])
                .trim_end_matches('/')
                .to_owned();
            let name = config_text(&entry["name"]);
            if !key.is_empty() {
                let source = format!("config:{name}");
                if !suppressed(pool, &source) {
                    active.insert(source.clone());
                    let mut payload = json!({"source":source,"auth_type":"api_key","access_token":key,"base_url":base,"label":if name.is_empty(){source.as_str()}else{&name}});
                    changed |= upsert_entry(entries, pool, &source, &mut payload)?;
                }
            }
        }
    }
    let model = &config["model"];
    let base = config_text(&model["base_url"])
        .trim_end_matches('/')
        .to_owned();
    let key = ["api_key", "api"]
        .iter()
        .filter_map(|key| model[*key].as_str())
        .map(|key| key.trim_matches(crate::python_value::python_whitespace))
        .find(|key| !key.is_empty());
    if config_text(&model["provider"]).to_lowercase() == "custom" && !base.is_empty() {
        if let Some(key) = key {
            if custom_pool_candidates(custom, &base, None)
                .iter()
                .any(|candidate| {
                    candidate
                        .trim_matches(crate::python_value::python_whitespace)
                        .to_lowercase()
                        == pool
                })
                && !suppressed(pool, "model_config")
            {
                active.insert("model_config".into());
                let mut payload = json!({"source":"model_config","auth_type":"api_key","access_token":key,"base_url":base,"label":"model_config"});
                // Python tolerates errors in the model-config fallback while
                // retaining any successful custom-entry ingestion above.
                if let Ok(updated) = upsert_entry(entries, pool, "model_config", &mut payload) {
                    changed |= updated;
                }
            }
        }
    }
    Ok((changed, active))
}

/// Pool seeding prefers the profile file, except unresolved op:// references
/// defer to a resolved scoped value. Scope errors propagate like Python.
pub fn prefer_dotenv(path: &Path, key: &str) -> Result<String> {
    let file = crate::secret_scope::load_env_file(path);
    let scoped = crate::secret_scope::get_secret(key, Some(""))?.unwrap_or_default();
    Ok(preferred(
        file.get(key).map(String::as_str).unwrap_or(""),
        &scoped,
    ))
}

fn preferred(file: &str, scoped: &str) -> String {
    let file = file.trim_matches(crate::python_value::python_whitespace);
    let scoped = scoped.trim_matches(crate::python_value::python_whitespace);
    if file.is_empty() || file.starts_with("op://") && !scoped.is_empty() {
        scoped.into()
    } else {
        file.into()
    }
}

/// The runner owns profile paths, external-secret metadata and vendor endpoint
/// resolution. Reads stay lazy so suppressed sources never reach URL probes.
pub trait EnvSource {
    fn get(&mut self, key: &str) -> Result<String>;
    fn suppressed(&mut self, provider: &str, source: &str) -> bool;
    fn secret_source(&mut self, key: &str) -> Option<String>;
    fn provider_url(
        &mut self,
        provider: &str,
        key: &str,
        default: &str,
        override_url: &str,
    ) -> Result<String>;
}

pub fn seed_from_env(
    provider: &str,
    entries: &mut Vec<PooledCredential>,
    config: Option<&EnvProvider>,
    source: &mut impl EnvSource,
) -> Result<(bool, HashSet<String>)> {
    let mut active = HashSet::new();
    let mut changed = false;
    if provider == "copilot" {
        return Ok((changed, active));
    }
    let (vars, base, env_url) = if provider == "openrouter" {
        (
            vec!["OPENROUTER_API_KEY".into()],
            "https://openrouter.ai/api/v1",
            String::new(),
        )
    } else {
        let Some(config) = config.filter(|config| config.auth_type == "api_key") else {
            return Ok((changed, active));
        };
        let env_url = match config.base_url_env.as_deref().filter(|key| !key.is_empty()) {
            Some(key) => source.get(key)?.trim_end_matches('/').to_owned(),
            None => String::new(),
        };
        let vars = if provider == "anthropic" {
            [
                "ANTHROPIC_TOKEN",
                "CLAUDE_CODE_OAUTH_TOKEN",
                "ANTHROPIC_API_KEY",
            ]
            .map(str::to_owned)
            .to_vec()
        } else {
            config.key_vars.clone()
        };
        (vars, config.base_url.as_str(), env_url)
    };
    for key_var in vars {
        let token = source.get(&key_var)?;
        if token.is_empty() {
            continue;
        }
        let identity = format!("env:{key_var}");
        if source.suppressed(provider, &identity) {
            continue;
        }
        active.insert(identity.clone());
        let base_url = if matches!(provider, "kimi-coding" | "zai") {
            source.provider_url(provider, &token, base, &env_url)?
        } else if env_url.is_empty() {
            base.into()
        } else {
            env_url.clone()
        };
        let mut payload = json!({"source":identity,"auth_type":"api_key","access_token":token,"base_url":base_url,"label":key_var});
        if let Some(label) = source
            .secret_source(&key_var)
            .map(|label| {
                label
                    .trim_matches(crate::python_value::python_whitespace)
                    .to_owned()
            })
            .filter(|label| !label.is_empty())
        {
            payload["secret_source"] = json!(label);
        }
        let ingested = upsert_entry(entries, provider, &identity, &mut payload)?;
        changed |= ingested;
        if provider == "openrouter" && ingested {
            static WARNED: std::sync::LazyLock<std::sync::Mutex<bool>> =
                std::sync::LazyLock::new(|| std::sync::Mutex::new(false));
            let mut warned = WARNED.lock().unwrap_or_else(|error| error.into_inner());
            if !*warned {
                tracing::warn!(
                    provider,
                    env_var = key_var,
                    "environment credential ingested into pool"
                );
                *warned = true;
            }
        }
    }
    Ok((changed, active))
}

/// Concrete profile reads, with endpoint hooks and resolved-secret provenance
/// supplied by the provider/secret-loader layers.
pub struct ProfileEnvSource<U> {
    pub dotenv_path: std::path::PathBuf,
    pub auth_path: std::path::PathBuf,
    pub provenance: std::collections::HashMap<String, String>,
    pub resolve_url: U,
}
impl<U: FnMut(&str, &str, &str, &str) -> Result<String>> EnvSource for ProfileEnvSource<U> {
    fn get(&mut self, key: &str) -> Result<String> {
        prefer_dotenv(&self.dotenv_path, key)
    }
    fn suppressed(&mut self, provider: &str, source: &str) -> bool {
        crate::auth_store::source_suppressed(&self.auth_path, provider, source)
    }
    fn secret_source(&mut self, key: &str) -> Option<String> {
        self.provenance.get(key).cloned()
    }
    fn provider_url(
        &mut self,
        provider: &str,
        key: &str,
        default: &str,
        override_url: &str,
    ) -> Result<String> {
        (self.resolve_url)(provider, key, default, override_url)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn custom_pool_identity_and_seeding_match_python() {
        let rows: serde_json::Value = serde_json::from_str(include_str!(
            "../../../tools/credential-custom-seed-goldens.json"
        ))
        .unwrap();
        for row in rows.as_array().unwrap() {
            if row["kind"] == "candidates" {
                assert_eq!(
                    json!(custom_pool_candidates(
                        &row["custom"],
                        row["base"].as_str().unwrap(),
                        row["name"].as_str()
                    )),
                    row["result"],
                    "{row}"
                );
                continue;
            }
            let mut entries = vec![];
            let mut calls = vec![];
            let (changed, active) = seed_custom_pool(
                row["pool"].as_str().unwrap(),
                &mut entries,
                &row["custom"],
                &row["config"],
                |pool, source| {
                    calls.push(json!([pool, source]));
                    row["suppressed"].as_bool().unwrap()
                },
            )
            .unwrap();
            let mut active: Vec<_> = active.into_iter().collect();
            active.sort();
            let result: Vec<_> = entries
                .iter()
                .map(|entry| {
                    let mut value = entry.to_dict();
                    value.as_object_mut().unwrap().remove("id");
                    value
                })
                .collect();
            assert_eq!(json!(result), row["result"], "{row}");
            assert_eq!(json!(active), row["active"], "{row}");
            assert_eq!(changed, row["changed"].as_bool().unwrap(), "{row}");
            assert_eq!(json!(calls), row["calls"], "{row}");
        }
    }
    #[test]
    fn source_helpers_match_python() {
        let rows: serde_json::Value = serde_json::from_str(include_str!(
            "../../../tools/credential-source-helper-goldens.json"
        ))
        .unwrap();
        for row in rows.as_array().unwrap() {
            let result = if row["kind"] == "prefer" {
                json!(preferred(
                    row["file"].as_str().unwrap(),
                    row["scoped"].as_str().unwrap()
                ))
            } else {
                json!(crate::auth_store::suppressed_in(
                    &row["store"],
                    "openai-api",
                    row["source"].as_str().unwrap()
                ))
            };
            assert_eq!(result, row["result"], "{row}");
        }
    }
    #[test]
    fn profile_files_suppression_and_rehydration_work_together() {
        let _lock = crate::secret_scope::GLOBAL_TEST_LOCK.lock().unwrap();
        let root = std::env::temp_dir().join(format!(
            "hermes-env-seed-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&root).unwrap();
        struct Cleanup(std::path::PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let _cleanup = Cleanup(root.clone());
        let mut source = ProfileEnvSource {
            dotenv_path: root.join(".env"),
            auth_path: root.join("auth.json"),
            provenance: Default::default(),
            resolve_url: |_: &str, _: &str, _: &str, _: &str| -> Result<String> {
                panic!("OpenAI seeding must not probe vendor URLs")
            },
        };
        let config = EnvProvider {
            auth_type: "api_key".into(),
            base_url: "https://default.example/v1".into(),
            base_url_env: None,
            key_vars: vec!["FIRST_KEY".into()],
        };
        std::fs::write(&source.dotenv_path, "FIRST_KEY=file-key\n").unwrap();
        std::fs::write(
            &source.auth_path,
            r#"{"providers":{},"suppressed_sources":{"openai-api":["env:FIRST_KEY"]}}"#,
        )
        .unwrap();
        let scope = std::collections::HashMap::from([("FIRST_KEY".into(), "scope-key".into())]);
        tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(crate::secret_scope::with_secret_scope(Some(scope), async {
                let mut entries = vec![];
                let (changed, active) =
                    seed_from_env("openai-api", &mut entries, Some(&config), &mut source).unwrap();
                assert!(!changed && active.is_empty() && entries.is_empty());
                std::fs::write(&source.auth_path, r#"{"providers":{}}"#).unwrap();
                assert!(
                    seed_from_env("openai-api", &mut entries, Some(&config), &mut source)
                        .unwrap()
                        .0
                );
                assert_eq!(entries[0].runtime_key(|_, _, _| false), "file-key");
                let serialized = entries[0].to_dict();
                assert!(serialized.get("access_token").is_none());
                std::fs::write(
                    &source.auth_path,
                    serde_json::to_vec(&json!({"credential_pool":{"openai-api":[serialized]}}))
                        .unwrap(),
                )
                .unwrap();
                let mut reloaded = crate::credential_pool::read_stored_entries(
                    &source.auth_path,
                    None,
                    "openai-api",
                )
                .unwrap();
                assert!(
                    !seed_from_env("openai-api", &mut reloaded, Some(&config), &mut source)
                        .unwrap()
                        .0
                );
                assert_eq!(reloaded[0].runtime_key(|_, _, _| false), "file-key");
                assert_eq!(reloaded[0].to_dict(), serialized);
                std::fs::write(&source.dotenv_path, "FIRST_KEY=op://Vault/Item/key\n").unwrap();
                assert!(
                    seed_from_env("openai-api", &mut reloaded, Some(&config), &mut source)
                        .unwrap()
                        .0
                );
                assert_eq!(reloaded[0].runtime_key(|_, _, _| false), "scope-key");
                let config_path=root.join("config.yaml");
                let contents="custom_providers:\n  - name: Local STT\n    base_url: http://localhost:8000/v1\n    api_key: config-key\nmodel:\n  provider: custom\n  base_url: http://localhost:8000/v1\n  api_key: model-key\n";
                std::fs::write(&config_path,contents).unwrap();
                let settings=crate::config_file::load_config_from(&config_path);
                let mut custom_entries=vec![];
                let (changed,active)=seed_custom_from_config("custom:local-stt",&mut custom_entries,&settings,|pool,identity| crate::auth_store::source_suppressed(&source.auth_path,pool,identity)).unwrap();
                assert!(changed);
                assert!(active.contains("config:Local STT") && active.contains("model_config"));
                assert_eq!(custom_entries.iter().map(|entry|entry.runtime_key(|_,_,_|false)).collect::<Vec<_>>(),["config-key","model-key"]);
                assert!(custom_entries.iter().all(|entry|entry.to_dict().get("access_token").is_none()));
                assert_eq!(std::fs::read_to_string(&config_path).unwrap(),contents);
                let keyed="providers:\n  keyed-stt:\n    api: http://localhost:9000/v1\n    apiKey: keyed-key\n";
                std::fs::write(&config_path,keyed).unwrap();
                let settings=crate::config_file::load_config_from(&config_path);
                let mut entries=vec![];
                assert!(seed_custom_from_config("custom:keyed-stt",&mut entries,&settings,|_,_|false).unwrap().0);
                assert_eq!(entries[0].runtime_key(|_,_,_|false),"keyed-key");
                assert_eq!(std::fs::read_to_string(&config_path).unwrap(),keyed);
            }));
    }
    struct Fixture<'a> {
        row: &'a serde_json::Value,
        calls: Vec<serde_json::Value>,
    }
    impl EnvSource for Fixture<'_> {
        fn get(&mut self, key: &str) -> Result<String> {
            self.calls.push(json!(["get", key]));
            Ok(if key == "BASE_URL" {
                self.row["override"].as_str().unwrap()
            } else if self.row["have_keys"].as_bool().unwrap() {
                "fixture-key"
            } else {
                ""
            }
            .into())
        }
        fn suppressed(&mut self, provider: &str, source: &str) -> bool {
            self.calls.push(json!(["suppressed", provider, source]));
            self.row["suppressed"].as_bool().unwrap()
        }
        fn secret_source(&mut self, key: &str) -> Option<String> {
            self.calls.push(json!(["provenance", key]));
            Some(" fixture-source ".into())
        }
        fn provider_url(
            &mut self,
            provider: &str,
            key: &str,
            default: &str,
            override_url: &str,
        ) -> Result<String> {
            self.calls
                .push(json!(["url", provider, key, default, override_url]));
            Ok(if override_url.is_empty() {
                "https://resolved.example/v1"
            } else {
                override_url
            }
            .into())
        }
    }
    #[test]
    fn environment_seed_results_and_effect_order_match_python() {
        let rows: serde_json::Value = serde_json::from_str(include_str!(
            "../../../tools/credential-env-seed-goldens.json"
        ))
        .unwrap();
        for row in rows.as_array().unwrap() {
            let config = (!row["config"].is_null()).then(|| EnvProvider {
                auth_type: row["config"]["auth_type"].as_str().unwrap().into(),
                base_url: row["config"]["base_url"].as_str().unwrap().into(),
                base_url_env: Some("BASE_URL".into()),
                key_vars: vec!["FIRST_KEY".into(), "SECOND_KEY".into()],
            });
            let mut source = Fixture { row, calls: vec![] };
            let mut entries = vec![];
            let (changed, active) = seed_from_env(
                row["provider"].as_str().unwrap(),
                &mut entries,
                config.as_ref(),
                &mut source,
            )
            .unwrap();
            let mut active: Vec<_> = active.into_iter().collect();
            active.sort();
            let result: Vec<_> = entries
                .iter()
                .map(|entry| {
                    let mut value = entry.to_dict();
                    value.as_object_mut().unwrap().remove("id");
                    value
                })
                .collect();
            assert_eq!(json!(result), row["result"], "{row}");
            assert_eq!(changed, row["changed"].as_bool().unwrap(), "{row}");
            assert_eq!(json!(active), row["active"], "{row}");
            assert_eq!(json!(source.calls), row["calls"], "{row}");
        }
    }
}
