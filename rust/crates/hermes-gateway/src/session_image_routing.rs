//! Resolve image routing for the same session model used by the upcoming turn.
//! Ports `GatewayRunner._decide_image_input_mode` from `gateway/run.py`.
// Runner integration follows the rich inbound event transport.
#![allow(dead_code)]

use anyhow::Result;
use async_trait::async_trait;
use serde_json::Value;

use crate::image_routing::{decide_image_input_mode, ImageInputMode, VisionCapabilityLookup};
use crate::session::SessionSource;

/// Runtime operations are supplied by the runner so image preparation never
/// borrows another conversation's process-global model or provider identity.
#[async_trait]
pub trait SessionImageRuntime: Send + Sync {
    async fn load_config(&self) -> Result<Value>;
    async fn resolve_session_runtime(
        &self,
        source: Option<&SessionSource>,
        session_key: Option<&str>,
        config: &Value,
    ) -> Result<(Value, Value)>;
    async fn read_main_provider(&self) -> Result<String>;
    async fn read_main_model(&self) -> Result<String>;
}

/// Explicit fields can override one half of the session's provider/model pair.
/// A non-object config asks the runtime to load the configured defaults.
pub struct SessionImageRequest<'a> {
    pub source: Option<&'a SessionSource>,
    pub session_key: Option<&'a str>,
    pub user_config: &'a Value,
    pub provider: &'a str,
    pub model: &'a str,
}

fn python_trim(text: &str) -> &str {
    text.trim_matches(|c: char| c.is_whitespace() || ('\u{1c}'..='\u{1f}').contains(&c))
}

/// Resolve missing identity fields, then apply the image mode decision. A
/// failed session lookup falls back to defaults; any outer failure selects
/// text mode, matching the runner's two separate exception boundaries.
pub async fn decide_session_image_input_mode(
    request: SessionImageRequest<'_>,
    runtime: &dyn SessionImageRuntime,
    capabilities: &dyn VisionCapabilityLookup,
) -> ImageInputMode {
    let decision: Result<ImageInputMode> = async {
        let loaded;
        let cfg = if request.user_config.is_object() {
            request.user_config
        } else {
            loaded = runtime.load_config().await?;
            &loaded
        };
        let mut provider = python_trim(request.provider).to_owned();
        let mut model = python_trim(request.model).to_owned();
        let mut requested_provider = String::new();
        let has_identity =
            request.source.is_some() || request.session_key.is_some_and(|key| !key.is_empty());
        if (provider.is_empty() || model.is_empty()) && has_identity {
            match runtime
                .resolve_session_runtime(request.source, request.session_key, cfg)
                .await
            {
                Ok((turn_model, kwargs)) => {
                    if model.is_empty() {
                        if let Some(value) = turn_model.as_str() {
                            model = python_trim(value).to_owned();
                        }
                    }
                    if provider.is_empty() {
                        if let Some(value) = kwargs.get("provider").and_then(Value::as_str) {
                            provider = python_trim(value).to_owned();
                        }
                    }
                    if let Some(value) = kwargs.get("requested_provider").and_then(Value::as_str) {
                        requested_provider = python_trim(value).to_owned();
                    }
                }
                Err(error) => {
                    tracing::debug!(%error, "image routing: session lookup failed, using defaults")
                }
            }
        }
        if provider.is_empty() {
            provider = runtime.read_main_provider().await?;
        }
        if model.is_empty() {
            model = runtime.read_main_model().await?;
        }
        decide_image_input_mode(&provider, &model, cfg, &requested_provider, capabilities).await
    }
    .await;
    match decision {
        Ok(mode) => mode,
        Err(error) => {
            tracing::debug!(%error, "image routing: decision failed, using text mode");
            ImageInputMode::Text
        }
    }
}

#[cfg(test)]
mod golden_corpus {
    use super::*;
    use anyhow::bail;
    use serde_json::json;
    use std::sync::Mutex;

    struct Runtime<'a> {
        case: &'a Value,
        calls: Mutex<Vec<Value>>,
    }

    impl Runtime<'_> {
        fn effect(&self, name: &str, result: Value) -> Result<Value> {
            self.calls.lock().unwrap().push(json!([name]));
            if self.case["fault"].as_str() == Some(name) {
                bail!("{name} failed");
            }
            Ok(result)
        }
    }

    #[async_trait]
    impl SessionImageRuntime for Runtime<'_> {
        async fn load_config(&self) -> Result<Value> {
            self.effect("load", json!({}))
        }
        async fn resolve_session_runtime(
            &self,
            source: Option<&SessionSource>,
            key: Option<&str>,
            cfg: &Value,
        ) -> Result<(Value, Value)> {
            self.calls
                .lock()
                .unwrap()
                .push(json!(["resolve", source.is_some(), key, cfg]));
            if self.case["fault"] == "resolve" {
                bail!("resolve failed");
            }
            Ok((
                self.case["runtime_model"].clone(),
                self.case["runtime"].clone(),
            ))
        }
        async fn read_main_provider(&self) -> Result<String> {
            Ok(self
                .effect("provider", json!("default-p"))?
                .as_str()
                .unwrap()
                .to_owned())
        }
        async fn read_main_model(&self) -> Result<String> {
            Ok(self
                .effect("model", json!("default-m"))?
                .as_str()
                .unwrap()
                .to_owned())
        }
    }

    #[async_trait]
    impl VisionCapabilityLookup for Runtime<'_> {
        async fn lookup(
            &self,
            provider: &str,
            model: &str,
            cfg: &Value,
            requested_provider: &str,
        ) -> Result<Option<bool>> {
            self.calls.lock().unwrap().push(json!([
                "lookup",
                provider,
                model,
                cfg,
                requested_provider
            ]));
            if self.case["fault"] == "lookup" {
                bail!("lookup failed");
            }
            Ok(Some(true))
        }
    }

    #[tokio::test]
    async fn session_routing_matches_python() {
        let cases: Vec<Value> = serde_json::from_str(include_str!(
            "../../../tools/session-image-routing-goldens.json"
        ))
        .unwrap();
        let source = SessionSource::default();
        for (index, case) in cases.iter().enumerate() {
            let runtime = Runtime {
                case,
                calls: Mutex::new(Vec::new()),
            };
            let request = SessionImageRequest {
                source: case["source"].as_bool().unwrap().then_some(&source),
                session_key: case["session_key"].as_str(),
                user_config: &case["cfg"],
                provider: case["provider"].as_str().unwrap(),
                model: case["model"].as_str().unwrap(),
            };
            let mode = decide_session_image_input_mode(request, &runtime, &runtime).await;
            let text = match mode {
                ImageInputMode::Native => "native",
                ImageInputMode::Text => "text",
                ImageInputMode::Auto => "auto",
            };
            assert_eq!(
                text,
                case["expected"]["output"].as_str().unwrap(),
                "case {index}"
            );
            assert_eq!(
                json!(*runtime.calls.lock().unwrap()),
                case["expected"]["calls"],
                "case {index}"
            );
        }
    }
}
