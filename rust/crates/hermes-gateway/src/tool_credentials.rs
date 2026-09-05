//! Shared STT/TTS secret lookup from tools/tool_backend_helpers.py.
//! Pool selection remains the pool owner's job, including cooldown and refresh.
#![allow(dead_code)]
use std::path::Path;

fn nonblank(value: Option<String>) -> Option<String> {
    value
        .map(|value| {
            value
                .trim_matches(crate::python_value::python_whitespace)
                .to_owned()
        })
        .filter(|value| !value.is_empty())
}

/// Resolve one provider secret. The pool callback returns the selected entry's
/// runtime key or access token, and an error stops pool discovery like Python.
pub fn provider_secret(
    name: &str,
    provider: &str,
    config_value: &str,
    dotenv_path: &Path,
    pool: &mut impl FnMut(&str) -> anyhow::Result<Option<String>>,
) -> String {
    resolve_with(
        name,
        provider,
        config_value,
        |name| {
            crate::secret_scope::get_secret(name, None).unwrap_or_else(|_| std::env::var(name).ok())
        },
        crate::secret_scope::is_multiplex_active,
        |name| {
            crate::secret_scope::get_secret(name, None)
                .ok()
                .flatten()
                .or_else(|| crate::secret_scope::load_env_file(dotenv_path).remove(name))
        },
        pool,
    )
}

pub fn openai_audio_key(
    dotenv_path: &Path,
    pool: &mut impl FnMut(&str) -> anyhow::Result<Option<String>>,
) -> String {
    let voice = provider_secret("VOICE_TOOLS_OPENAI_KEY", "", "", dotenv_path, pool);
    if !voice.is_empty() {
        return voice;
    }
    provider_secret("OPENAI_API_KEY", "openai-api", "", dotenv_path, pool)
}

fn resolve_with(
    name: &str,
    provider: &str,
    config_value: &str,
    mut scoped: impl FnMut(&str) -> Option<String>,
    multiplex: impl FnOnce() -> bool,
    mut env: impl FnMut(&str) -> Option<String>,
    pool: &mut impl FnMut(&str) -> anyhow::Result<Option<String>>,
) -> String {
    if let Some(key) = nonblank(Some(config_value.into())) {
        return key;
    }
    if let Some(key) = nonblank(scoped(name)) {
        return key;
    }
    if multiplex() {
        return String::new();
    }
    if let Some(key) = nonblank(env(name)) {
        return key;
    }
    if !provider.is_empty() {
        for id in [provider.to_owned(), format!("custom:{provider}")] {
            match pool(&id) {
                Ok(value) => {
                    if let Some(key) = nonblank(value) {
                        return key;
                    }
                }
                Err(_) => break,
            }
        }
    }
    String::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    // Integration: the CredentialPool selection core drives provider_secret's
    // pool callback exactly as tools/tool_backend_helpers.py does (load_pool ->
    // has_credentials -> peek -> runtime_api_key). With no config, scope or env
    // value, resolution must fall through to the pool and return the peeked key.
    #[test]
    fn provider_secret_resolves_through_credential_pool() {
        use crate::credential_pool::{CredentialPool, PooledCredential};

        let _lock = crate::secret_scope::GLOBAL_TEST_LOCK.lock().unwrap();
        let prev = crate::secret_scope::is_multiplex_active();
        crate::secret_scope::set_multiplex_active(false);

        // Two API-key entries; priority 0 wins the peek.
        let entries = vec![
            PooledCredential::from_dict(
                "openai-api",
                &serde_json::json!({
                    "id": "hi0000", "auth_type": "api_key", "source": "manual",
                    "access_token": "sk-high", "priority": 0, "last_status": "ok"
                }),
            )
            .unwrap(),
            PooledCredential::from_dict(
                "openai-api",
                &serde_json::json!({
                    "id": "lo0000", "auth_type": "api_key", "source": "manual",
                    "access_token": "sk-low", "priority": 1, "last_status": "ok"
                }),
            )
            .unwrap(),
        ];
        let mut pool =
            CredentialPool::new("openai-api", entries, "fill_first").with_clock(|| 1_700_000_000.0);

        // A missing (non-empty dotenv) path with no matching key, and a callback
        // that only answers for "openai-api" (the plain id), mirroring load_pool.
        let dotenv = std::env::temp_dir().join(format!(
            "hermes_toolcred_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let mut cb = |id: &str| -> anyhow::Result<Option<String>> {
            if id == "openai-api" {
                Ok(pool.peek_runtime_key())
            } else {
                Ok(None)
            }
        };
        let key = provider_secret(
            "OPENAI_API_KEY_UNSET_ENV_VAR_XYZ",
            "openai-api",
            "",
            &dotenv,
            &mut cb,
        );
        assert_eq!(key, "sk-high");

        crate::secret_scope::set_multiplex_active(prev);
    }

    #[test]
    fn real_scope_blocks_file_and_pool_and_preserves_voice_priority() {
        let _lock = crate::secret_scope::GLOBAL_TEST_LOCK.lock().unwrap();
        struct Reset(bool, std::path::PathBuf);
        impl Drop for Reset {
            fn drop(&mut self) {
                crate::secret_scope::set_multiplex_active(self.0);
                let _ = std::fs::remove_file(&self.1);
            }
        }
        let unique = format!(
            "HERMES_TEST_VOICE_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let path = std::env::temp_dir().join(&unique);
        let _reset = Reset(crate::secret_scope::is_multiplex_active(), path.clone());
        std::fs::write(
            &path,
            format!("{unique}=file-key\nVOICE_TOOLS_OPENAI_KEY=file-voice-key\n"),
        )
        .unwrap();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        crate::secret_scope::set_multiplex_active(true);
        runtime.block_on(crate::secret_scope::with_secret_scope(
            Some(Default::default()),
            async {
                let mut no_pool = |_: &str| -> anyhow::Result<Option<String>> {
                    panic!("scoped miss reached pool")
                };
                assert_eq!(
                    provider_secret(&unique, "openai-api", "", &path, &mut no_pool),
                    ""
                );
                assert_eq!(openai_audio_key(&path, &mut no_pool), "");
            },
        ));
        for (voice, expected) in [
            (" scoped-voice ", "scoped-voice"),
            (" \u{1c}", "scoped-openai"),
        ] {
            let scope = std::collections::HashMap::from([
                ("VOICE_TOOLS_OPENAI_KEY".into(), voice.into()),
                ("OPENAI_API_KEY".into(), " scoped-openai ".into()),
            ]);
            runtime.block_on(crate::secret_scope::with_secret_scope(Some(scope), async {
                assert_eq!(
                    openai_audio_key(&path, &mut |_| panic!("scope hit reached pool")),
                    expected
                );
                let mut source = crate::transcription_http::ProfileAudioCredentials {
                    dotenv_path: path.clone(),
                    pool: |_: &str| -> anyhow::Result<Option<String>> {
                        panic!("scope hit reached pool")
                    },
                    managed:
                        || -> anyhow::Result<Option<crate::transcription_http::AudioCredentials>> {
                            panic!("direct selection reached managed gateway")
                        },
                    unavailable: || -> Option<String> {
                        panic!("scope hit reached entitlement lookup")
                    },
                };
                let credentials = crate::transcription_http::AudioCredentials::resolve(
                    &serde_json::json!({"stt":{"provider":"openai"}}),
                    &serde_json::json!({}),
                    "https://api.openai.com/v1",
                    &mut source,
                )
                .unwrap();
                assert_eq!(credentials.key, expected);
                assert_eq!(credentials.base_url, "https://api.openai.com/v1");
            }));
        }
        crate::secret_scope::set_multiplex_active(false);
        assert_eq!(
            provider_secret(&unique, "openai-api", "", &path, &mut |_| panic!(
                "file hit reached pool"
            )),
            "file-key"
        );
        let scope = std::collections::HashMap::from([
            ("VOICE_TOOLS_OPENAI_KEY".into(), String::new()),
            ("OPENAI_API_KEY".into(), String::new()),
        ]);
        runtime.block_on(crate::secret_scope::with_secret_scope(Some(scope), async {
            let mut calls = vec![];
            let key = openai_audio_key(&path, &mut |id| {
                calls.push(id.to_owned());
                Ok((id == "custom:openai-api").then(|| " custom-key ".into()))
            });
            assert_eq!(key, "custom-key");
            assert_eq!(calls, ["openai-api", "custom:openai-api"]);
        }));
    }

    #[test]
    fn source_order_and_multiplex_gate_match_python() {
        let rows: serde_json::Value =
            serde_json::from_str(include_str!("../../../tools/provider-secret-goldens.json"))
                .unwrap();
        for row in rows.as_array().unwrap() {
            let calls = std::cell::RefCell::new(Vec::new());
            let text = |key: &str| row[key].as_str().map(str::to_owned);
            let actual = resolve_with(
                "TEST_KEY",
                row["provider"].as_str().unwrap(),
                row["config"].as_str().unwrap(),
                |_| {
                    calls.borrow_mut().push("scope".to_owned());
                    text("scope")
                },
                || row["multiplex"].as_bool().unwrap(),
                |_| {
                    calls.borrow_mut().push("env".to_owned());
                    text("env")
                },
                &mut |id| {
                    calls.borrow_mut().push(id.to_owned());
                    if row["pool_error"].as_bool().unwrap() {
                        anyhow::bail!("fixture pool error");
                    }
                    Ok(text(if id.starts_with("custom:") {
                        "custom"
                    } else {
                        "pool"
                    }))
                },
            );
            assert_eq!(actual, row["result"].as_str().unwrap(), "{row}");
            assert_eq!(serde_json::json!(calls.into_inner()), row["calls"], "{row}");
        }
    }
}
