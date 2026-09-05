//! OpenAI-compatible STT transport consumed by gateway enrichment.
//!
//! Saved provider selection controls lazy credential resolution. Profile-aware
//! credential sources and SILK preprocessing remain with the runner.
//! Rejected containers are converted once before retrying. No credentials are logged.
#![allow(dead_code)]

use crate::transcription_enrichment::TranscriptionBackend;
use anyhow::Result;
use async_trait::async_trait;
use reqwest::multipart::{Form, Part};
use serde_json::{json, Value};
use std::{path::Path, time::Duration};

/// Credential effects are lazy: an explicit provider choice must not read
/// credentials for a different provider. The runner owns profile and pool scope.
pub trait AudioCredentialSource {
    fn direct_key(&mut self) -> Result<Option<String>>;
    fn managed_audio(&mut self) -> Result<Option<AudioCredentials>>;
    fn unavailable_note(&mut self) -> Option<String>;
}

/// Profile-scoped direct lookup with managed account operations supplied by
/// the runner. Callbacks stay lazy, including pool access and entitlement reads.
pub struct ProfileAudioCredentials<P, M, N> {
    pub dotenv_path: std::path::PathBuf,
    pub pool: P,
    pub managed: M,
    pub unavailable: N,
}

impl<P, M, N> AudioCredentialSource for ProfileAudioCredentials<P, M, N>
where
    P: FnMut(&str) -> Result<Option<String>>,
    M: FnMut() -> Result<Option<AudioCredentials>>,
    N: FnMut() -> Option<String>,
{
    fn direct_key(&mut self) -> Result<Option<String>> {
        let key = crate::tool_credentials::openai_audio_key(&self.dotenv_path, &mut self.pool);
        Ok((!key.is_empty()).then_some(key))
    }
    fn managed_audio(&mut self) -> Result<Option<AudioCredentials>> {
        (self.managed)()
    }
    fn unavailable_note(&mut self) -> Option<String> {
        (self.unavailable)()
    }
}

/// Managed sources supply the final vendor API URL, including its /v1 path.
/// Deliberately omit Debug so a client configuration cannot log its secret.
pub struct AudioCredentials {
    pub key: String,
    pub base_url: String,
}

impl AudioCredentials {
    pub fn resolve(
        raw_config: &Value,
        stt: &Value,
        default_base_url: &str,
        source: &mut impl AudioCredentialSource,
    ) -> Result<Self> {
        let selected = crate::tool_backend_selection::from_raw(raw_config, "stt");
        let selection_error = |name: &str, failure: &str| {
            anyhow::anyhow!("stt is configured to use {name} (set via hermes tools), but {failure}. Run 'hermes tools' to change it.")
        };
        if selected.as_deref() == Some("nous") {
            return source.managed_audio()?.ok_or_else(|| {
                selection_error(
                    "nous",
                    "the Nous Tool Gateway is not available (not entitled or unreachable)",
                )
            });
        }
        let config = &stt["openai"];
        let key = config["api_key"].as_str().unwrap_or_default();
        let base = config["base_url"].as_str().filter(|base| !base.is_empty());
        if !key.is_empty() {
            return Ok(Self {
                key: key.into(),
                base_url: base.unwrap_or(default_base_url).into(),
            });
        }
        if let Some(base) = base.filter(|base| is_local_or_private_url(base)) {
            return Ok(Self {
                key: "not-needed".into(),
                base_url: base.into(),
            });
        }
        if let Some(key) = source.direct_key()?.filter(|key| !key.is_empty()) {
            // A remote config URL without its own key is not the destination
            // for an environment credential, even when it is explicitly saved.
            return Ok(Self {
                key,
                base_url: default_base_url.into(),
            });
        }
        if let Some(selected) = selected {
            return Err(selection_error(&selected, "neither stt.openai.api_key in config nor VOICE_TOOLS_OPENAI_KEY/OPENAI_API_KEY is set"));
        }
        if let Some(credentials) = source.managed_audio()? {
            return Ok(credentials);
        }
        let mut message =
            "Neither stt.openai.api_key in config nor VOICE_TOOLS_OPENAI_KEY/OPENAI_API_KEY is set"
                .to_owned();
        if let Some(note) = source.unavailable_note() {
            message.push_str(". ");
            message.push_str(&note);
        }
        anyhow::bail!(message)
    }
}

#[derive(Debug)]
struct ApiFailure {
    status: reqwest::StatusCode,
    body: String,
}
impl std::fmt::Display for ApiFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "API error: HTTP {}: {}", self.status, self.body)
    }
}
impl std::error::Error for ApiFailure {}

fn rejected_container(error: &anyhow::Error) -> bool {
    error.downcast_ref::<ApiFailure>().is_some_and(|error| {
        error.status == reqwest::StatusCode::BAD_REQUEST
            && ["unsupported", "corrupted", "invalid file"]
                .iter()
                .any(|keyword| error.body.to_lowercase().contains(keyword))
    })
}

/// Resolved request settings from `_transcribe_openai`. Keeping the provider
/// label explicit avoids rewriting Whisper variants served by other endpoints.
pub struct TranscriptionHttp {
    client: reqwest::Client,
    endpoint: String,
    key: String,
    provider: String,
    model: String,
    language: Option<String>,
    prompt: Option<String>,
}

impl TranscriptionHttp {
    /// Build the OpenAI STT client from saved intent and resolved STT settings.
    /// Keep raw settings separate so schema defaults cannot select a provider.
    pub fn from_openai_config(
        raw_config: &Value,
        stt: &Value,
        default_base_url: &str,
        model: String,
        default_model: &str,
        source: &mut impl AudioCredentialSource,
    ) -> Result<Self> {
        let credentials = AudioCredentials::resolve(raw_config, stt, default_base_url, source)?;
        Ok(Self::new(
            &credentials.base_url,
            credentials.key,
            "openai".into(),
            model,
            default_model,
            None,
            None,
        )?
        .with_language_config(stt, None))
    }

    pub fn new(
        base_url: &str,
        key: String,
        provider: String,
        model: String,
        default_openai_model: &str,
        language: Option<String>,
        prompt: Option<String>,
    ) -> Result<Self> {
        let model = if provider == "openai"
            && matches!(
                model.as_str(),
                "whisper-large-v3" | "whisper-large-v3-turbo" | "distil-whisper-large-v3-en"
            ) {
            default_openai_model.to_owned()
        } else {
            model
        };
        Ok(Self {
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(30))
                .redirect(reqwest::redirect::Policy::none())
                .build()?,
            endpoint: format!("{}/audio/transcriptions", base_url.trim_end_matches('/')),
            key,
            provider,
            model,
            language,
            prompt,
        })
    }

    /// Resolve provider/global language config when no hook/caller override
    /// was supplied. The legacy environment value is passed explicitly so a
    /// profile-scoped runner can resolve it without mutating process state.
    pub fn with_language_config(mut self, stt: &Value, legacy_language: Option<&str>) -> Self {
        if self
            .language
            .as_ref()
            .is_none_or(|language| language.is_empty())
        {
            self.language = resolve_language(stt, &self.provider, &[], legacy_language);
        }
        self
    }

    /// Upload an already-prepared audio file. Read policy is applied before
    /// opening it. Source validation/preprocessing belongs to the runner.
    pub async fn transcribe(
        &self,
        path: &str,
        policy: &crate::file_read_safety::FileReadPolicy,
    ) -> Value {
        let result = match self.request(path, policy).await {
            Err(error) if rejected_container(&error) => {
                // Call request directly on retry, so another container error
                // cannot trigger an unbounded transcode/upload loop.
                match crate::audio_process::transcode(path).await {
                    Ok(converted) => {
                        self.request(&converted.path.to_string_lossy(), policy)
                            .await
                    }
                    Err(error) => Err(error),
                }
            }
            result => result,
        };
        match result {
            Ok(transcript) => {
                json!({"success":true,"transcript":transcript,"provider":self.provider})
            }
            Err(error) => json!({"success":false,"transcript":"","error":error.to_string()}),
        }
    }

    async fn request(
        &self,
        path: &str,
        policy: &crate::file_read_safety::FileReadPolicy,
    ) -> Result<String> {
        policy.check_read(path)?;
        crate::audio_process::validate_audio_file(path, false).await?;
        let metadata = tokio::fs::metadata(path).await?;
        crate::audio_process::validate_upload_size(metadata.len())?;
        let audio = tokio::fs::read(path).await?;
        let name = Path::new(path)
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        let response_format = if self.model == "whisper-1" {
            "text"
        } else {
            "json"
        };
        let mut form = Form::new()
            .text("model", self.model.clone())
            .part("file", Part::bytes(audio).file_name(name))
            .text("response_format", response_format);
        if let Some(language) = self.language.as_ref().filter(|s| !s.is_empty()) {
            let field = if self.model == "gpt-transcribe" {
                "languages[]"
            } else {
                "language"
            };
            form = form.text(field, language.clone());
        }
        if let Some(prompt) = self.prompt.as_ref().filter(|s| !s.is_empty()) {
            form = form.text("prompt", prompt.clone());
        }
        let response = self
            .client
            .post(&self.endpoint)
            .bearer_auth(&self.key)
            .multipart(form)
            .send()
            .await?;
        let status = response.status();
        let text = response.text().await?;
        if !status.is_success() {
            return Err(ApiFailure { status, body: text }.into());
        }
        let transcript = if response_format == "text" {
            text
        } else {
            let value: Value = serde_json::from_str(&text)?;
            value
                .get("text")
                .and_then(Value::as_str)
                .map(str::to_owned)
                .unwrap_or_else(|| {
                    value
                        .as_str()
                        .map(str::to_owned)
                        .unwrap_or_else(|| crate::python_value::python_repr(&value))
                })
        };
        Ok(extract_text(&transcript))
    }
}

/// Keyless STT endpoints must pass the reference's narrower locality gate.
/// Do not add a scheme or accept bare/private-looking malformed hostnames.
fn is_local_or_private_url(url: &str) -> bool {
    let host = crate::local_probe::urlparse_hostname(url);
    if host == "localhost"
        || [".local", ".lan", ".internal"]
            .iter()
            .any(|suffix| host.ends_with(suffix))
    {
        return true;
    }
    let address = if host.contains(':') {
        match host.split_once('%') {
            Some((address, scope)) if !scope.is_empty() && !scope.contains('%') => address,
            Some(_) => return false,
            None => &host,
        }
    } else {
        &host
    };
    address
        .parse()
        .ok()
        .is_some_and(crate::local_probe::addr_is_private_or_loopback)
}

/// Provider language, historical aliases, global language, then legacy env.
/// Only nonblank strings participate; config numbers and booleans are ignored.
fn resolve_language(
    stt: &Value,
    provider: &str,
    aliases: &[&str],
    legacy: Option<&str>,
) -> Option<String> {
    let section = &stt[provider];
    std::iter::once(section.get("language").and_then(Value::as_str))
        .chain(
            aliases
                .iter()
                .map(|key| section.get(*key).and_then(Value::as_str)),
        )
        .chain([stt.get("language").and_then(Value::as_str), legacy])
        .flatten()
        .map(|value| value.trim_matches(crate::python_value::python_whitespace))
        .find(|value| !value.is_empty())
        .map(str::to_owned)
}

/// Strip the ASR language wrapper accepted by the Python response normalizer.
fn extract_text(text: &str) -> String {
    static WRAPPER: std::sync::LazyLock<fancy_regex::Regex> = std::sync::LazyLock::new(|| {
        fancy_regex::Regex::new(r"(?si)^[\s\x1c-\x1f]*language[\s\x1c-\x1f]+[\w.-]+(?:[\s\x1c-\x1f]*<audio_language>[^<]*</audio_language>)?[\s\x1c-\x1f]*<asr_text>[\s\x1c-\x1f]*(.*)").expect("fixed ASR wrapper")
    });
    let text = text.trim_matches(crate::python_value::python_whitespace);
    let visible = WRAPPER
        .captures(text)
        .ok()
        .flatten()
        .and_then(|c| c.get(1))
        .map(|m| m.as_str())
        .unwrap_or(text);
    visible
        .trim_matches(crate::python_value::python_whitespace)
        .to_owned()
}

/// Use HTTP STT and native duration probing while retaining the runner's local fallback,
/// and sandbox path mapping. This plugs into the existing enrichment pipeline.
pub struct HttpTranscriptionBackend<B> {
    pub transport: TranscriptionHttp,
    pub read_policy: crate::file_read_safety::FileReadPolicy,
    pub context: B,
}

#[async_trait]
impl<B: TranscriptionBackend> TranscriptionBackend for HttpTranscriptionBackend<B> {
    fn absolute_path(&self, path: &str) -> String {
        self.context.absolute_path(path)
    }
    async fn probe_duration(&self, path: &str) -> Option<String> {
        crate::audio_process::probe_duration(path).await
    }
    async fn transcribe(&self, path: &str) -> Result<Value> {
        Ok(self.transport.transcribe(path, &self.read_policy).await)
    }
    async fn local_fallback(&self, path: &str) -> Result<Value> {
        self.context.local_fallback(path).await
    }
    fn agent_visible_path(&self, path: &str) -> String {
        self.context.agent_visible_path(path)
    }
}

#[cfg(test)]
mod tests {
    struct CredentialsFixture<'a> {
        row: &'a serde_json::Value,
        calls: Vec<&'static str>,
    }

    impl super::AudioCredentialSource for CredentialsFixture<'_> {
        fn direct_key(&mut self) -> anyhow::Result<Option<String>> {
            self.calls.push("direct");
            Ok(self.row["direct_key"].as_str().map(str::to_owned))
        }

        fn managed_audio(&mut self) -> anyhow::Result<Option<super::AudioCredentials>> {
            self.calls.push("openai-audio");
            Ok(self.row["managed"]
                .as_bool()
                .unwrap_or(false)
                .then(|| super::AudioCredentials {
                    key: "managed-key".into(),
                    base_url: "https://gateway.example/vendor/v1".into(),
                }))
        }

        fn unavailable_note(&mut self) -> Option<String> {
            self.row["unavailable_note"].as_str().map(str::to_owned)
        }
    }

    #[test]
    fn credential_selection_and_lazy_effects_match_python() {
        let rows: serde_json::Value =
            serde_json::from_str(include_str!("../../../tools/stt-credential-goldens.json"))
                .unwrap();
        for row in rows.as_array().unwrap() {
            let mut source = CredentialsFixture { row, calls: vec![] };
            let raw = serde_json::json!({"stt":{"provider":row["selection"]}});
            let stt = serde_json::json!({"openai":row["openai"]});
            let result = super::AudioCredentials::resolve(
                &raw,
                &stt,
                "https://api.openai.com/v1",
                &mut source,
            );
            match result {
                Ok(pair) => {
                    assert!(row["error"].is_null(), "{row}");
                    assert_eq!(
                        serde_json::json!([pair.key, pair.base_url]),
                        row["result"],
                        "{row}"
                    );
                }
                Err(error) => {
                    assert_eq!(error.to_string(), row["error"].as_str().unwrap(), "{row}")
                }
            }
            assert_eq!(serde_json::json!(source.calls), row["calls"], "{row}");
        }
    }

    #[test]
    fn configured_client_uses_raw_intent_and_preserves_key_destination() {
        let row = serde_json::json!({"direct_key":"direct-key", "managed":true});
        let mut source = CredentialsFixture {
            row: &row,
            calls: vec![],
        };
        let client = super::TranscriptionHttp::from_openai_config(
            &serde_json::json!({"stt":{"provider":"openai"}}),
            &serde_json::json!({"openai":{"base_url":"https://custom.example/v1", "language":"ms"}}),
            "https://api.openai.com/v1", "whisper-1".into(), "whisper-1", &mut source,
        ).unwrap();
        assert_eq!(
            client.endpoint,
            "https://api.openai.com/v1/audio/transcriptions"
        );
        assert_eq!(client.key, "direct-key");
        assert_eq!(client.language.as_deref(), Some("ms"));
        assert_eq!(source.calls, ["direct"]);

        source.calls.clear();
        let pair = super::AudioCredentials::resolve(
            &serde_json::json!({"stt":{"provider":"openai","use_gateway":true}}),
            &serde_json::json!({"openai":{"api_key":"config-key"}}),
            "https://api.openai.com/v1",
            &mut source,
        )
        .unwrap();
        assert_eq!(pair.key, "managed-key");
        assert_eq!(source.calls, ["openai-audio"]);
    }

    use super::*;
    use std::sync::{Arc, Mutex};

    #[test]
    fn stt_locality_matches_python_without_model_discovery_shortcuts() {
        let cases: Value =
            serde_json::from_str(include_str!("../../../tools/stt-locality-goldens.json")).unwrap();
        for case in cases.as_array().unwrap() {
            assert_eq!(
                is_local_or_private_url(case["url"].as_str().unwrap()),
                case["expected"].as_bool().unwrap(),
                "{case}"
            );
        }
        for url in [
            "http://whisper",
            "http://100.64.1.2",
            "http://10.01.2.3",
            "localhost:8000",
        ] {
            assert!(crate::local_probe::is_local_endpoint(url));
            assert!(!is_local_or_private_url(url));
        }
    }

    #[test]
    fn language_resolution_matches_python_and_preserves_overrides() {
        let cases: Value =
            serde_json::from_str(include_str!("../../../tools/stt-language-goldens.json")).unwrap();
        for case in cases.as_array().unwrap() {
            assert_eq!(
                resolve_language(
                    &case["config"],
                    "openai",
                    &["language_code"],
                    case["env"].as_str()
                )
                .as_deref(),
                case["expected"].as_str(),
                "{case}"
            );
        }
        for (override_, expected) in [
            (None, "ms"),
            (Some(""), "ms"),
            (Some("ja"), "ja"),
            (Some(" "), " "),
        ] {
            let client = TranscriptionHttp::new(
                "http://localhost",
                "unused".into(),
                "openai".into(),
                "whisper-1".into(),
                "whisper-1",
                override_.map(str::to_owned),
                None,
            )
            .unwrap()
            .with_language_config(&json!({"openai":{"language":" ms "}}), Some("en"));
            assert_eq!(client.language.as_deref(), Some(expected));
        }
    }

    #[test]
    fn transcript_normalization_matches_python() {
        let cases: Value =
            serde_json::from_str(include_str!("../../../tools/stt-text-goldens.json")).unwrap();
        for case in cases.as_array().unwrap() {
            assert_eq!(
                extract_text(case["input"].as_str().unwrap()),
                case["expected"].as_str().unwrap(),
                "{case}"
            );
        }
    }

    #[tokio::test]
    #[ignore = "requires ffmpeg with AAC encoding support"]
    async fn rejected_container_retries_real_m4a_once() {
        use axum::{body::Bytes, http::StatusCode, routing::post, Router};
        for (status, reason, reject_retry, expected_calls) in [
            (StatusCode::BAD_REQUEST, "Unsupported audio", false, 2),
            (StatusCode::BAD_REQUEST, "corrupted file", true, 2),
            (StatusCode::BAD_REQUEST, "unknown model", false, 1),
            (
                StatusCode::UNAUTHORIZED,
                "unsupported credentials",
                false,
                1,
            ),
        ] {
            let uploads = Arc::new(Mutex::new(Vec::new()));
            let capture = uploads.clone();
            let app = Router::new().route(
                "/audio/transcriptions",
                post(move |body: Bytes| {
                    let mut uploads = capture.lock().unwrap();
                    uploads.push(body.to_vec());
                    let response = if uploads.len() == 1 || reject_retry {
                        (status, reason)
                    } else {
                        (StatusCode::OK, "{\"text\":\"converted speech\"}")
                    };
                    async move { response }
                }),
            );
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            struct Server(tokio::task::JoinHandle<()>);
            impl Drop for Server {
                fn drop(&mut self) {
                    self.0.abort();
                }
            }
            let _server = Server(tokio::spawn(async move {
                axum::serve(listener, app).await.unwrap();
            }));
            struct Directory(std::path::PathBuf);
            impl Drop for Directory {
                fn drop(&mut self) {
                    let _ = std::fs::remove_dir_all(&self.0);
                }
            }
            let root = std::env::temp_dir().join(format!(
                "hermes-stt-retry-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            std::fs::create_dir(&root).unwrap();
            let _directory = Directory(root.clone());
            // Valid PCM WAV, 0.1 seconds at 16 kHz, generated without tools.
            let mut wav = b"RIFF".to_vec();
            wav.extend_from_slice(&3236_u32.to_le_bytes());
            wav.extend_from_slice(b"WAVEfmt ");
            wav.extend_from_slice(&16_u32.to_le_bytes());
            wav.extend_from_slice(&1_u16.to_le_bytes());
            wav.extend_from_slice(&1_u16.to_le_bytes());
            wav.extend_from_slice(&16000_u32.to_le_bytes());
            wav.extend_from_slice(&32000_u32.to_le_bytes());
            wav.extend_from_slice(&2_u16.to_le_bytes());
            wav.extend_from_slice(&16_u16.to_le_bytes());
            wav.extend_from_slice(b"data");
            wav.extend_from_slice(&3200_u32.to_le_bytes());
            wav.resize(3244, 0);
            let audio = root.join("voice.wav");
            std::fs::write(&audio, wav).unwrap();
            let policy = crate::file_read_safety::FileReadPolicy {
                home: root.clone(),
                cwd: root.clone(),
                hermes_home: root.join(".hermes"),
                hermes_root: root.join(".hermes"),
            };
            let client = TranscriptionHttp::new(
                &format!("http://{address}"),
                "fixture-key".into(),
                "openai".into(),
                "gpt-4o-transcribe".into(),
                "whisper-1",
                None,
                None,
            )
            .unwrap();
            let result = client.transcribe(audio.to_str().unwrap(), &policy).await;
            let uploads = uploads.lock().unwrap();
            assert_eq!(uploads.len(), expected_calls);
            assert_eq!(result["success"], expected_calls == 2 && !reject_retry);
            if expected_calls == 2 {
                let converted = &uploads[1];
                assert!(converted.windows(4).any(|bytes| bytes == b"ftyp"));
                assert!(String::from_utf8_lossy(converted).contains("filename=\"voice-stt.m4a\""));
                if !reject_retry {
                    assert_eq!(result["transcript"], "converted speech");
                }
            }
        }
    }

    struct Context;
    #[async_trait]
    impl TranscriptionBackend for Context {
        fn absolute_path(&self, path: &str) -> String {
            path.into()
        }
        async fn probe_duration(&self, _: &str) -> Option<String> {
            None
        }
        async fn transcribe(&self, _: &str) -> Result<Value> {
            panic!("HTTP transport must handle transcription")
        }
        async fn local_fallback(&self, _: &str) -> Result<Value> {
            panic!("successful HTTP must not fall back")
        }
        fn agent_visible_path(&self, path: &str) -> String {
            path.into()
        }
    }

    #[tokio::test]
    async fn disabled_stt_uses_native_wav_duration_in_voice_note() {
        use base64::Engine;
        let cases: Value =
            serde_json::from_str(include_str!("../../../tools/audio-duration-goldens.json"))
                .unwrap();
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(cases["wav"][4]["wav"].as_str().unwrap())
            .unwrap();
        let path = std::env::temp_dir().join(format!(
            "hermes-disabled-stt-{}-{}.wav",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::write(&path, bytes).unwrap();
        let root = path.parent().unwrap().to_owned();
        let backend = HttpTranscriptionBackend {
            transport: TranscriptionHttp::new(
                "http://127.0.0.1:1",
                "unused".into(),
                "openai".into(),
                "whisper-1".into(),
                "whisper-1",
                None,
                None,
            )
            .unwrap(),
            read_policy: crate::file_read_safety::FileReadPolicy {
                home: root.clone(),
                cwd: root.clone(),
                hermes_home: root.join(".hermes"),
                hermes_root: root.join(".hermes"),
            },
            context: Context,
        };
        let (text, transcripts) =
            crate::transcription_enrichment::enrich_message_with_transcription(
                "caption",
                &[path.to_string_lossy().into_owned()],
                false,
                true,
                &backend,
            )
            .await
            .unwrap();
        assert!(text.contains("(duration: 1:01)"));
        assert!(text.ends_with("caption"));
        assert!(transcripts.is_empty());
        std::fs::remove_file(path).unwrap();
    }

    #[tokio::test]
    async fn multipart_upload_reaches_gateway_enrichment_and_blocks_secret_files() {
        use axum::{body::Bytes, http::HeaderMap, routing::post, Router};
        let recorded = Arc::new(Mutex::new(Vec::new()));
        let capture = recorded.clone();
        let app = Router::new().route(
            "/v1/audio/transcriptions",
            post(move |headers: HeaderMap, bytes: Bytes| {
                let body = String::from_utf8(bytes.to_vec()).unwrap();
                assert_eq!(headers["authorization"], "Bearer fixture-key");
                assert!(headers["content-type"]
                    .to_str()
                    .unwrap()
                    .starts_with("multipart/form-data; boundary="));
                assert!(body.contains("filename=\"voice.wav\""));
                assert!(body.contains("audio-fixture-bytes"));
                let response = if body.contains("\r\ntext\r\n") {
                    "  spoken words  "
                } else {
                    "{\"text\":\"language en<asr_text> spoken words \"}"
                };
                capture.lock().unwrap().push(body);
                async move { response }
            }),
        );
        let app = app.route(
            "/denied/audio/transcriptions",
            post(|| async { (axum::http::StatusCode::UNAUTHORIZED, "fixture denial") }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        struct Server(tokio::task::JoinHandle<()>);
        impl Drop for Server {
            fn drop(&mut self) {
                self.0.abort();
            }
        }
        let _server = Server(tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        }));
        struct Directory(std::path::PathBuf);
        impl Drop for Directory {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let root = std::env::temp_dir().join(format!(
            "hermes-stt-http-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let _directory = Directory(root.clone());
        let audio = root.join("voice.wav");
        std::fs::write(&audio, "audio-fixture-bytes").unwrap();
        std::fs::write(root.join(".env"), "fixture-secret").unwrap();
        std::fs::write(root.join("unsupported.txt"), "not audio").unwrap();
        for model in [
            "whisper-1",
            "gpt-4o-transcribe",
            "gpt-transcribe",
            "whisper-large-v3",
        ] {
            let row = json!({});
            let mut source = CredentialsFixture {
                row: &row,
                calls: vec![],
            };
            let mut transport = TranscriptionHttp::from_openai_config(
                &json!({"stt":{"provider":"openai"}}),
                &json!({"openai":{"api_key":"fixture-key", "base_url":format!("http://{address}/v1/"), "language":" ms "}, "language":"en"}),
                "https://api.openai.com/v1",
                model.into(),
                "whisper-1",
                &mut source,
            )
            .unwrap();
            transport.prompt = Some("vocabulary".into());
            assert!(source.calls.is_empty());
            let backend = HttpTranscriptionBackend {
                transport,
                context: Context,
                read_policy: crate::file_read_safety::FileReadPolicy {
                    home: root.clone(),
                    cwd: root.clone(),
                    hermes_home: root.join(".hermes"),
                    hermes_root: root.join(".hermes"),
                },
            };
            let (text, transcripts) =
                crate::transcription_enrichment::enrich_message_with_transcription(
                    "caption",
                    &[audio.to_string_lossy().into_owned()],
                    true,
                    true,
                    &backend,
                )
                .await
                .unwrap();
            assert_eq!(transcripts, vec!["spoken words"]);
            assert!(text.contains("spoken words"));
            assert!(text.ends_with("caption"));
            let before = recorded.lock().unwrap().len();
            let denied = backend
                .transcribe(root.join(".env").to_str().unwrap())
                .await
                .unwrap();
            assert_eq!(denied["success"], false);
            let unsupported = backend
                .transcribe(root.join("unsupported.txt").to_str().unwrap())
                .await
                .unwrap();
            assert_eq!(unsupported["success"], false);
            assert!(unsupported["error"]
                .as_str()
                .unwrap()
                .starts_with("Unsupported format: .txt."));
            assert_eq!(recorded.lock().unwrap().len(), before);
            let requests = recorded.lock().unwrap();
            let body = requests.last().unwrap();
            assert!(body.contains(if model == "gpt-transcribe" {
                "name=\"languages[]\""
            } else {
                "name=\"language\""
            }));
            assert!(body.contains("\r\nms\r\n"));
            assert!(body.contains("name=\"prompt\""));
            assert!(body.contains("\r\nvocabulary\r\n"));
            if model == "whisper-large-v3" {
                assert!(body.contains("\r\nwhisper-1\r\n"));
            }
        }
        let denied = TranscriptionHttp::new(
            &format!("http://{address}/denied"),
            "fixture-key".into(),
            "openai".into(),
            "whisper-1".into(),
            "whisper-1",
            None,
            None,
        )
        .unwrap();
        let policy = crate::file_read_safety::FileReadPolicy {
            home: root.clone(),
            cwd: root.clone(),
            hermes_home: root.join(".hermes"),
            hermes_root: root.join(".hermes"),
        };
        let result = denied.transcribe(audio.to_str().unwrap(), &policy).await;
        assert_eq!(result["success"], false);
        assert_eq!(result["transcript"], "");
        assert!(result["error"].as_str().unwrap().contains("401"));
        assert_eq!(recorded.lock().unwrap().len(), 4);
    }
}
