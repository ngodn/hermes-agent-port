//! Image input decisions and the live capability lookup from agent/image_routing.py.
//!
//! Config overrides precede managed, cloud, and local-probe stages. The live
//! lookup owns their ordering; provider-specific HTTP and caches live in their
//! respective modules. Runner construction must still supply discovered profiles
//! and shared dependencies through `LiveVisionLookup`.
#![allow(dead_code)]
//!
//! `cfg` is the loaded config.yaml as a `serde_json::Value` (the parent converts
//! YAML to JSON before this layer sees it). A JSON `null` or any non-object
//! stands in for Python's "cfg is None / not a dict" branches.

use crate::python_value::{python_number, python_repr};
use anyhow::Result;
use async_trait::async_trait;
use serde_json::{Map, Value};

/// The three configured routing modes. `decide_image_input_mode` only ever
/// returns `Native` or `Text`; `Auto` is the pre-decision state read from
/// config and resolved by capability lookup.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageInputMode {
    Auto,
    Native,
    Text,
}

// YAML 1.1/1.2 boolean tokens (post strip+lower). Anything outside these plus
// real bool / int 0|1 is rejected so a quoted `"false"` cannot silently enable
// native vision on a model that cannot see.
const TRUE_TOKENS: [&str; 4] = ["true", "yes", "on", "1"];
const FALSE_TOKENS: [&str; 4] = ["false", "no", "off", "0"];

/// Port of `_coerce_capability_bool`. Returns `Some` only for values a strict
/// YAML/JSON boolean coercion recognizes; everything else is `None` so the
/// caller falls through to models.dev rather than honoring garbage.
///
/// Booleans map straight through. Integers coerce only for exactly 0 or 1;
/// floats are rejected (Python `isinstance(x, int)` is false for float, and a
/// JSON float parses with `is_i64`/`is_u64` both false here). Strings are
/// stripped and lowercased before matching the token sets.
pub fn coerce_capability_bool(raw: &Value) -> Option<bool> {
    match raw {
        Value::Bool(b) => Some(*b),
        Value::Number(n) => {
            // Only genuine integers reach the 0/1 gate; a JSON float has neither
            // an i64 nor a u64 representation, matching Python's int-only check.
            if n.is_i64() || n.is_u64() {
                match n.as_i64() {
                    Some(0) => Some(false),
                    Some(1) => Some(true),
                    _ => None,
                }
            } else {
                None
            }
        }
        Value::String(s) => {
            let t = s.trim_matches(python_whitespace).to_lowercase();
            if TRUE_TOKENS.contains(&t.as_str()) {
                Some(true)
            } else if FALSE_TOKENS.contains(&t.as_str()) {
                Some(false)
            } else {
                None
            }
        }
        _ => None,
    }
}

// Python strip includes the four ASCII information separators.
fn python_whitespace(c: char) -> bool {
    c.is_whitespace() || ('\u{1c}'..='\u{1f}').contains(&c)
}

/// Python `str(x or "")`: falsy values collapse to the empty string, otherwise
/// `str(x)`. Handles the "odd config values" the source coerces defensively
/// (a provider written as a number, a stray `true`, and so on). Compound values use Python representation because configuration may contain
/// provider names matching those coerced strings.
fn py_str_or_empty(v: &Value) -> String {
    match v {
        Value::Null => String::new(),
        Value::Bool(false) => String::new(),
        Value::Bool(true) => "True".to_string(),
        // 0 / 0.0 are falsy -> "" ; other numbers stringify like Python str().
        Value::Number(n) => {
            if n.as_f64() == Some(0.0) {
                String::new()
            } else {
                python_number(n)
            }
        }
        Value::String(s) => s.clone(),
        Value::Array(a) if a.is_empty() => String::new(),
        Value::Object(o) if o.is_empty() => String::new(),
        other => python_repr(other),
    }
}

/// `per_model.get("supports_vision", per_model.get("vision"))`: the
/// `supports_vision` key wins whenever it is present, even when its value is
/// `null` (which coerces to `None` and suppresses the `vision` alias). The
/// alias is consulted only when `supports_vision` is absent entirely.
fn per_model_capability(per_model: &Map<String, Value>) -> Option<bool> {
    let raw = per_model
        .get("supports_vision")
        .or_else(|| per_model.get("vision"))?;
    coerce_capability_bool(raw)
}

/// Port of `_supports_vision_override`. Resolves a user-declared vision
/// capability from config.yaml. Resolution order, first hit wins:
///   1. top-level `model.supports_vision`
///   2. `providers.<candidate>.models.<model>` (`supports_vision` or the
///      `vision` alias), where candidates are the requested provider, then the
///      runtime provider, then the config-declared `model.provider`, each also
///      expanded past a `custom:` prefix
///   3. Legacy list-form `custom_providers`, walked candidate-first so list
///      order cannot let a persisted default shadow the live route.
///
/// Returns `None` when nothing is declared so the caller falls through to the
/// capability lookup. The provider/requested_provider strings are used verbatim
/// as `providers` keys (matching the source, which does not re-strip them) and
/// are stripped+lowercased only for legacy list name matching.
pub fn supports_vision_override(
    cfg: &Value,
    provider: &str,
    model: &str,
    requested_provider: &str,
) -> Option<bool> {
    let cfg = cfg.as_object()?;

    // 1. Top-level shortcut.
    let model_cfg = cfg.get("model").and_then(Value::as_object);
    if let Some(mc) = model_cfg {
        if let Some(top) = mc.get("supports_vision").and_then(coerce_capability_bool) {
            return Some(top);
        }
    }

    // Build ordered candidate provider keys, expanding `custom:<name>`.
    let config_provider = model_cfg
        .and_then(|mc| mc.get("provider"))
        .map(py_str_or_empty)
        .unwrap_or_default();
    let config_provider = config_provider.trim_matches(python_whitespace);

    let mut candidates: Vec<String> = Vec::new();
    for c in [requested_provider, provider, config_provider] {
        if c.is_empty() {
            continue;
        }
        candidates.push(c.to_string());
        if let Some(stripped) = c.strip_prefix("custom:") {
            if !stripped.is_empty() {
                candidates.push(stripped.to_string());
            }
        }
    }
    // dict.fromkeys: dedupe, preserve first-seen order.
    let mut deduped: Vec<String> = Vec::new();
    for c in candidates {
        if !deduped.contains(&c) {
            deduped.push(c);
        }
    }
    let candidates = deduped;

    // 2. Per-provider, per-model under `providers`.
    if let Some(providers) = cfg.get("providers").and_then(Value::as_object) {
        for p in &candidates {
            let per_model = providers
                .get(p)
                .and_then(Value::as_object)
                .and_then(|e| e.get("models"))
                .and_then(Value::as_object)
                .and_then(|m| m.get(model))
                .and_then(Value::as_object);
            if let Some(pm) = per_model {
                if let Some(c) = per_model_capability(pm) {
                    return Some(c);
                }
            }
        }
    }

    // 2b. Legacy list-style `custom_providers`, candidate-first.
    if let Some(list) = cfg.get("custom_providers").and_then(Value::as_array) {
        for candidate in &candidates {
            let candidate_name = candidate.trim_matches(python_whitespace).to_lowercase();
            for entry in list {
                let entry = match entry.as_object() {
                    Some(e) => e,
                    None => continue,
                };
                let entry_name = py_str_or_empty(entry.get("name").unwrap_or(&Value::Null));
                if entry_name.trim_matches(python_whitespace).to_lowercase() != candidate_name {
                    continue;
                }
                let per_model = entry
                    .get("models")
                    .and_then(Value::as_object)
                    .and_then(|m| m.get(model))
                    .and_then(Value::as_object);
                if let Some(pm) = per_model {
                    if let Some(c) = per_model_capability(pm) {
                        return Some(c);
                    }
                }
            }
        }
    }

    None
}

/// Values from the current turn's context-local inference runtime.
/// Empty fields also represent a failed runtime lookup, matching Python's
/// fallthrough to config. Callers supply the context explicitly.
pub struct InferenceRuntime<'a> {
    pub provider: &'a str,
    pub base_url: &'a str,
    pub api_key: &'a str,
}

// Keep the requested provider verbatim, as Python does. Only model.provider
// is stripped before candidate construction. Python uses a set; Rust uses
// requested name, its alias, configured name, then its alias. Conflicting
// dictionary entries can therefore differ from a particular Python process.
fn inference_candidate_names(provider: &str, config_provider: &str) -> Vec<String> {
    let mut names = Vec::new();
    for name in [provider, config_provider]
        .into_iter()
        .filter(|name| !name.is_empty())
    {
        let alias = if name.to_lowercase().starts_with("custom:") {
            name.split_once(':').unwrap().1.to_owned()
        } else {
            format!("custom:{name}")
        };
        for candidate in [name.to_owned(), alias] {
            if !names.contains(&candidate) {
                names.push(candidate);
            }
        }
    }
    names
}

fn cfg_str_field(map: &Map<String, Value>, key: &str) -> String {
    py_str_or_empty(map.get(key).unwrap_or(&Value::Null))
        .trim_matches(python_whitespace)
        .to_owned()
}

// Both Python resolvers share the same config fallback. Legacy custom
// providers are searched in list order, independent of candidate precedence.
fn inference_config_field(cfg: &Value, provider: &str, field: &str) -> String {
    let model = cfg.get("model").and_then(Value::as_object);
    if let Some(model) = model {
        let value = cfg_str_field(model, field);
        if !value.is_empty() {
            return value;
        }
    }
    let config_provider = model
        .map(|model| cfg_str_field(model, "provider"))
        .unwrap_or_default();
    let candidates = inference_candidate_names(provider, &config_provider);
    if let Some(providers) = cfg.get("providers").and_then(Value::as_object) {
        for candidate in &candidates {
            if let Some(entry) = providers.get(candidate).and_then(Value::as_object) {
                let value = cfg_str_field(entry, field);
                if !value.is_empty() {
                    return value;
                }
            }
        }
    }
    if let Some(providers) = cfg.get("custom_providers").and_then(Value::as_array) {
        let lowered: Vec<_> = candidates.iter().map(|name| name.to_lowercase()).collect();
        for entry in providers.iter().filter_map(Value::as_object) {
            let name = cfg_str_field(entry, "name");
            if candidates.contains(&name) || lowered.contains(&name.to_lowercase()) {
                let value = cfg_str_field(entry, field);
                if !value.is_empty() {
                    return value;
                }
            }
        }
    }
    String::new()
}

/// Port of Python's _resolve_inference_base_url: matching turn runtime first,
/// then model config, provider dictionaries, and legacy custom providers.
pub fn resolve_inference_base_url(
    cfg: &Value,
    provider: &str,
    runtime: &InferenceRuntime,
) -> String {
    let base_url = runtime.base_url.trim_matches(python_whitespace);
    let requested = provider.trim_matches(python_whitespace).to_lowercase();
    let actual = runtime
        .provider
        .trim_matches(python_whitespace)
        .to_lowercase();
    if !base_url.is_empty() && (requested.is_empty() || requested == actual) {
        return base_url.to_owned();
    }
    inference_config_field(cfg, provider, "base_url")
}

/// Port of Python's _resolve_inference_api_key. Unlike the URL resolver,
/// Python accepts a non-empty runtime key without checking the provider.
/// Preserve that asymmetry without treating it as a guarantee that keys match.
pub fn resolve_inference_api_key(
    cfg: &Value,
    provider: &str,
    runtime: &InferenceRuntime,
) -> String {
    let api_key = runtime.api_key.trim_matches(python_whitespace);
    if !api_key.is_empty() {
        return api_key.to_owned();
    }
    inference_config_field(cfg, provider, "api_key")
}

/// Managed-local stage of Python's _lookup_supports_vision. This stage runs
/// after explicit config overrides and before cloud catalog lookup.
pub async fn lookup_managed_vision(
    provider: &str,
    model: &str,
    cfg: &Value,
    runtime: &InferenceRuntime<'_>,
    managed: &crate::managed_capabilities::ManagedCapabilities,
) -> Option<bool> {
    let base_url = resolve_inference_base_url(cfg, provider, runtime);
    if !managed.is_managed_provider(provider, &base_url).await {
        return None;
    }
    managed.managed_model_supports_vision(model).await
}

/// Cloud catalog stage, after managed capabilities and before local probes.
pub async fn lookup_catalog_vision(
    provider: &str,
    model: &str,
    cfg: &Value,
    catalog: &std::sync::Arc<crate::models_dev::ModelsDev>,
) -> Option<bool> {
    catalog
        .capabilities(provider, model, cfg, true)
        .await
        .map(|caps| caps.supports_vision)
}

/// Final Ollama stage of Python's _lookup_supports_vision. The caller must
/// first consult config overrides, managed runtime, and the model catalog.
/// Provider-profile prefix stripping must already have produced bare_model.
pub async fn lookup_ollama_vision(
    provider: &str,
    bare_model: &str,
    cfg: &Value,
    runtime: &InferenceRuntime<'_>,
    probes: &crate::local_probe::LocalProbe,
) -> Option<bool> {
    let mut base_url = resolve_inference_base_url(cfg, provider, runtime);
    if base_url.is_empty() && provider.trim_matches(python_whitespace).to_lowercase() == "ollama" {
        base_url = "http://localhost:11434/v1".into();
    }
    let api_key = resolve_inference_api_key(cfg, provider, runtime);
    if !probes
        .should_probe_ollama_vision(provider, &base_url, &api_key)
        .await
    {
        return None;
    }
    probes
        .query_ollama_supports_vision(bare_model, &base_url, &api_key)
        .await
}

/// Port of `_coerce_mode`. Non-strings and unrecognized strings normalize to
/// `Auto`; recognized values are matched after strip+lower.
pub fn coerce_mode(raw: &Value) -> ImageInputMode {
    if let Value::String(s) = raw {
        match s.trim_matches(python_whitespace).to_lowercase().as_str() {
            "auto" => ImageInputMode::Auto,
            "native" => ImageInputMode::Native,
            "text" => ImageInputMode::Text,
            _ => ImageInputMode::Auto,
        }
    } else {
        ImageInputMode::Auto
    }
}

/// Port of `_explicit_aux_vision_override`. True when the user named a specific
/// auxiliary vision backend (a non-`auto`/non-empty provider, model, or
/// base_url), which is the de-facto image route in `auto` mode.
///
/// The source's `x or {}` / `isinstance(x, dict)` dance collapses to: only a
/// dict `auxiliary.vision` can be explicit; every other shape (falsy, or a
/// truthy non-dict) yields false.
pub fn explicit_aux_vision_override(cfg: &Value) -> bool {
    let cfg = match cfg.as_object() {
        Some(o) => o,
        None => return false,
    };
    let vision = match cfg
        .get("auxiliary")
        .and_then(Value::as_object)
        .and_then(|aux| aux.get("vision"))
        .and_then(Value::as_object)
    {
        Some(v) => v,
        None => return false,
    };

    let provider = py_str_or_empty(vision.get("provider").unwrap_or(&Value::Null))
        .trim_matches(python_whitespace)
        .to_lowercase();
    let model = py_str_or_empty(vision.get("model").unwrap_or(&Value::Null))
        .trim_matches(python_whitespace)
        .to_string();
    let base_url = py_str_or_empty(vision.get("base_url").unwrap_or(&Value::Null))
        .trim_matches(python_whitespace)
        .to_string();

    // "auto" / "" / blank on all three = not explicit.
    if (provider.is_empty() || provider == "auto") && model.is_empty() && base_url.is_empty() {
        return false;
    }
    true
}

/// The capability-lookup effect `decide_image_input_mode` depends on. In the
/// source this is `_lookup_supports_vision`: consults the config override, then
/// managed-runtime caps, then models.dev, then an Ollama probe, with network IO
/// and base_url/key resolution. `LiveVisionLookup` supplies those stages;
/// decision-only tests can still provide a deterministic lookup.
///
/// `Ok(Some(true))` routes native, `Ok(Some(false))` / `Ok(None)` route text.
/// An `Err` is propagated by `decide_image_input_mode` (see below) rather than
/// being swallowed at this layer.
#[async_trait]
pub trait VisionCapabilityLookup: Send + Sync {
    async fn lookup(
        &self,
        provider: &str,
        model: &str,
        cfg: &Value,
        requested_provider: &str,
    ) -> Result<Option<bool>>;
}

/// The active turn's identity, kept explicit so auxiliary lookups cannot borrow
/// another conversation's named custom provider.
pub struct VisionRuntime<'a> {
    pub inference: InferenceRuntime<'a>,
    pub model: &'a str,
    pub requested_provider: &'a str,
}

/// Live capability waterfall. The caller supplies the shared caches and an
/// initialized provider registry; discovery is not replaced by a static name list.
pub struct LiveVisionLookup<'a> {
    pub runtime: VisionRuntime<'a>,
    pub profiles: &'a crate::provider_registry::ProviderRegistry,
    pub managed: &'a crate::managed_capabilities::ManagedCapabilities,
    pub catalog: &'a std::sync::Arc<crate::models_dev::ModelsDev>,
    pub probes: &'a crate::local_probe::LocalProbe,
}

#[async_trait]
impl VisionCapabilityLookup for LiveVisionLookup<'_> {
    async fn lookup(
        &self,
        provider: &str,
        model: &str,
        cfg: &Value,
        requested_provider: &str,
    ) -> Result<Option<bool>> {
        let mut requested = requested_provider;
        if requested.is_empty()
            && self
                .runtime
                .inference
                .provider
                .trim_matches(python_whitespace)
                .to_lowercase()
                == provider.trim_matches(python_whitespace).to_lowercase()
            && self.runtime.model.trim_matches(python_whitespace)
                == model.trim_matches(python_whitespace)
        {
            requested = self
                .runtime
                .requested_provider
                .trim_matches(python_whitespace);
        }
        if let Some(vision) = supports_vision_override(cfg, provider, model, requested) {
            return Ok(Some(vision));
        }
        if provider.is_empty() || model.is_empty() {
            return Ok(None);
        }
        if let Some(vision) =
            lookup_managed_vision(provider, model, cfg, &self.runtime.inference, self.managed).await
        {
            return Ok(Some(vision));
        }
        if let Some(vision) = lookup_catalog_vision(provider, model, cfg, self.catalog).await {
            return Ok(Some(vision));
        }
        let bare_model = self.profiles.strip_model_prefix(model);
        Ok(lookup_ollama_vision(
            provider,
            bare_model,
            cfg,
            &self.runtime.inference,
            self.probes,
        )
        .await)
    }
}

/// Port of `decide_image_input_mode`. Returns `Native` or `Text` for the turn.
///
/// `agent.image_input_mode: native` / `text` are absolute overrides. In `auto`
/// mode an explicitly configured `auxiliary.vision` backend routes text (the
/// user named a dedicated vision model, maintainer decision 2026-08-28);
/// otherwise the capability lookup decides, with only a definite `Some(true)`
/// routing native.
///
/// The source keeps a three-argument lookup call for the empty
/// `requested_provider` case so that layer can resolve the runtime identity
/// itself. That resolution lives inside the lookup implementation here, so both
/// paths are one call through the seam with `requested_provider` forwarded
/// verbatim (empty string when unset).
///
/// Lookup errors propagate: unlike the per-image defensive `except` blocks
/// elsewhere in the source, this decision layer does not turn a failed lookup
/// into a silent text fallback.
pub async fn decide_image_input_mode(
    provider: &str,
    model: &str,
    cfg: &Value,
    requested_provider: &str,
    lookup: &dyn VisionCapabilityLookup,
) -> Result<ImageInputMode> {
    let mode_cfg = match cfg.as_object().and_then(|c| c.get("agent")) {
        Some(Value::Object(agent)) => {
            coerce_mode(agent.get("image_input_mode").unwrap_or(&Value::Null))
        }
        // Missing agent, falsy agent, or a truthy non-dict all leave auto.
        _ => ImageInputMode::Auto,
    };

    match mode_cfg {
        ImageInputMode::Native => return Ok(ImageInputMode::Native),
        ImageInputMode::Text => return Ok(ImageInputMode::Text),
        ImageInputMode::Auto => {}
    }

    if explicit_aux_vision_override(cfg) {
        return Ok(ImageInputMode::Text);
    }

    let supports = lookup
        .lookup(provider, model, cfg, requested_provider)
        .await?;
    if supports == Some(true) {
        Ok(ImageInputMode::Native)
    } else {
        Ok(ImageInputMode::Text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::future::Future;

    fn block_on<F: Future>(fut: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(fut)
    }

    // ---- coerce_capability_bool -------------------------------------------

    #[test]
    fn coerce_bool_accepts_real_bools() {
        assert_eq!(coerce_capability_bool(&json!(true)), Some(true));
        assert_eq!(coerce_capability_bool(&json!(false)), Some(false));
    }

    #[test]
    fn coerce_bool_accepts_int_0_and_1_only() {
        assert_eq!(coerce_capability_bool(&json!(1)), Some(true));
        assert_eq!(coerce_capability_bool(&json!(0)), Some(false));
        // Other integers are not booleans.
        assert_eq!(coerce_capability_bool(&json!(2)), None);
        assert_eq!(coerce_capability_bool(&json!(-1)), None);
    }

    #[test]
    fn coerce_bool_rejects_floats() {
        // isinstance(1.0, int) is False in Python; a JSON float must not coerce.
        assert_eq!(coerce_capability_bool(&json!(1.0)), None);
        assert_eq!(coerce_capability_bool(&json!(0.0)), None);
    }

    #[test]
    fn coerce_bool_string_tokens_with_strip_and_lower() {
        for t in ["true", "  TRUE ", "Yes", "on", "1"] {
            assert_eq!(coerce_capability_bool(&json!(t)), Some(true), "{t}");
        }
        for f in ["false", " No ", "OFF", "0"] {
            assert_eq!(coerce_capability_bool(&json!(f)), Some(false), "{f}");
        }
    }

    #[test]
    fn coerce_bool_unrecognized_is_none() {
        // The classic "false" trap: any non-token string is None, not truthy.
        assert_eq!(coerce_capability_bool(&json!("truthy")), None);
        assert_eq!(coerce_capability_bool(&json!("")), None);
        assert_eq!(coerce_capability_bool(&Value::Null), None);
        assert_eq!(coerce_capability_bool(&json!([])), None);
        assert_eq!(coerce_capability_bool(&json!({})), None);
    }

    // ---- supports_vision_override -----------------------------------------

    #[test]
    fn override_none_when_cfg_not_object() {
        assert_eq!(supports_vision_override(&Value::Null, "p", "m", ""), None);
        assert_eq!(supports_vision_override(&json!("x"), "p", "m", ""), None);
    }

    #[test]
    fn override_top_level_model_precedes_provider_config() {
        // Top-level model.supports_vision wins even when the provider block
        // declares the opposite for the same model.
        let cfg = json!({
            "model": {"supports_vision": true, "provider": "p"},
            "providers": {"p": {"models": {"m": {"supports_vision": false}}}},
        });
        assert_eq!(supports_vision_override(&cfg, "p", "m", ""), Some(true));
    }

    #[test]
    fn override_provider_priority_requested_over_runtime() {
        // requested_provider is tried before the runtime provider.
        let cfg = json!({
            "providers": {
                "reqp": {"models": {"m": {"supports_vision": true}}},
                "runp": {"models": {"m": {"supports_vision": false}}},
            }
        });
        assert_eq!(
            supports_vision_override(&cfg, "runp", "m", "reqp"),
            Some(true)
        );
    }

    #[test]
    fn override_falls_back_to_configured_provider() {
        // No requested provider, runtime provider absent from config, so the
        // config-declared model.provider is the matching candidate.
        let cfg = json!({
            "model": {"provider": "cfgp"},
            "providers": {"cfgp": {"models": {"m": {"vision": true}}}},
        });
        assert_eq!(
            supports_vision_override(&cfg, "custom", "m", ""),
            Some(true)
        );
    }

    #[test]
    fn override_custom_prefix_alias_expansion() {
        // provider="custom" plus a config identity of "custom:myvllm" must reach
        // a providers block keyed by the bare "myvllm".
        let cfg = json!({
            "model": {"provider": "custom:myvllm"},
            "providers": {"myvllm": {"models": {"m": {"supports_vision": true}}}},
        });
        assert_eq!(
            supports_vision_override(&cfg, "custom", "m", ""),
            Some(true)
        );
    }

    #[test]
    fn override_vision_alias_and_null_suppression() {
        // The `vision` alias works when supports_vision is absent.
        let alias = json!({"providers": {"p": {"models": {"m": {"vision": true}}}}});
        assert_eq!(supports_vision_override(&alias, "p", "m", ""), Some(true));

        // A present-but-null supports_vision suppresses the alias (the key wins,
        // coerces to None), so resolution falls through to None here.
        let suppressed = json!({
            "providers": {"p": {"models": {"m": {"supports_vision": null, "vision": true}}}}
        });
        assert_eq!(supports_vision_override(&suppressed, "p", "m", ""), None);
    }

    #[test]
    fn override_legacy_list_candidate_first_walk() {
        // custom_providers list order puts "b" first, but requested_provider "a"
        // is the first candidate, so "a"'s entry wins regardless of list order.
        let cfg = json!({
            "custom_providers": [
                {"name": "b", "models": {"m": {"vision": false}}},
                {"name": "a", "models": {"m": {"vision": true}}},
            ]
        });
        assert_eq!(supports_vision_override(&cfg, "b", "m", "a"), Some(true));
    }

    #[test]
    fn override_none_when_nothing_declared() {
        let cfg = json!({"providers": {"p": {"models": {"other": {"vision": true}}}}});
        assert_eq!(supports_vision_override(&cfg, "p", "m", ""), None);
    }

    // ---- coerce_mode ------------------------------------------------------

    #[test]
    fn coerce_mode_valid_values() {
        assert_eq!(coerce_mode(&json!("auto")), ImageInputMode::Auto);
        assert_eq!(coerce_mode(&json!(" Native ")), ImageInputMode::Native);
        assert_eq!(coerce_mode(&json!("TEXT")), ImageInputMode::Text);
    }

    #[test]
    fn coerce_mode_invalid_and_non_string_default_auto() {
        assert_eq!(coerce_mode(&json!("bogus")), ImageInputMode::Auto);
        assert_eq!(coerce_mode(&Value::Null), ImageInputMode::Auto);
        assert_eq!(coerce_mode(&json!(1)), ImageInputMode::Auto);
        assert_eq!(coerce_mode(&json!(true)), ImageInputMode::Auto);
    }

    // ---- explicit_aux_vision_override -------------------------------------

    #[test]
    fn aux_override_false_for_auto_or_empty() {
        assert!(!explicit_aux_vision_override(&Value::Null));
        assert!(!explicit_aux_vision_override(&json!({})));
        assert!(!explicit_aux_vision_override(&json!({"auxiliary": {}})));
        assert!(!explicit_aux_vision_override(
            &json!({"auxiliary": {"vision": {}}})
        ));
        assert!(!explicit_aux_vision_override(
            &json!({"auxiliary": {"vision": {"provider": "auto"}}})
        ));
        assert!(!explicit_aux_vision_override(
            &json!({"auxiliary": {"vision": {"provider": "  ", "model": "", "base_url": ""}}})
        ));
    }

    #[test]
    fn aux_override_true_for_any_explicit_field() {
        assert!(explicit_aux_vision_override(
            &json!({"auxiliary": {"vision": {"provider": "openai"}}})
        ));
        assert!(explicit_aux_vision_override(
            &json!({"auxiliary": {"vision": {"model": "gpt-4o"}}})
        ));
        assert!(explicit_aux_vision_override(
            &json!({"auxiliary": {"vision": {"base_url": "http://x"}}})
        ));
    }

    #[test]
    fn aux_override_false_for_non_dict_shapes() {
        // auxiliary present but not a dict, and vision present but not a dict.
        assert!(!explicit_aux_vision_override(&json!({"auxiliary": "x"})));
        assert!(!explicit_aux_vision_override(
            &json!({"auxiliary": {"vision": "x"}})
        ));
    }

    // ---- decide_image_input_mode ------------------------------------------

    enum Stub {
        Ok(Option<bool>),
        Err,
    }

    #[async_trait]
    impl VisionCapabilityLookup for Stub {
        async fn lookup(
            &self,
            _provider: &str,
            _model: &str,
            _cfg: &Value,
            _requested_provider: &str,
        ) -> Result<Option<bool>> {
            match self {
                Stub::Ok(o) => Ok(*o),
                Stub::Err => Err(anyhow::anyhow!("lookup boom")),
            }
        }
    }

    #[test]
    fn decide_native_and_text_are_absolute_overrides() {
        let native_cfg = json!({"agent": {"image_input_mode": "native"}});
        // Lookup would say text, but the explicit mode wins without consulting it.
        let mode = block_on(decide_image_input_mode(
            "p",
            "m",
            &native_cfg,
            "",
            &Stub::Ok(Some(false)),
        ))
        .unwrap();
        assert_eq!(mode, ImageInputMode::Native);

        let text_cfg = json!({"agent": {"image_input_mode": "text"}});
        let mode = block_on(decide_image_input_mode(
            "p",
            "m",
            &text_cfg,
            "",
            &Stub::Ok(Some(true)),
        ))
        .unwrap();
        assert_eq!(mode, ImageInputMode::Text);
    }

    #[test]
    fn decide_auto_aux_backend_routes_text() {
        // Explicit aux vision backend beats a vision-capable main model in auto.
        let cfg = json!({
            "agent": {"image_input_mode": "auto"},
            "auxiliary": {"vision": {"provider": "openai", "model": "gpt-4o"}},
        });
        let mode = block_on(decide_image_input_mode(
            "p",
            "m",
            &cfg,
            "",
            &Stub::Ok(Some(true)),
        ))
        .unwrap();
        assert_eq!(mode, ImageInputMode::Text);
    }

    #[test]
    fn decide_auto_uses_capability_lookup() {
        // No mode set (defaults auto), no aux backend: the lookup decides.
        let cfg = json!({});
        assert_eq!(
            block_on(decide_image_input_mode(
                "p",
                "m",
                &cfg,
                "",
                &Stub::Ok(Some(true))
            ))
            .unwrap(),
            ImageInputMode::Native
        );
        // Only a definite true routes native; false and unknown route text.
        assert_eq!(
            block_on(decide_image_input_mode(
                "p",
                "m",
                &cfg,
                "",
                &Stub::Ok(Some(false))
            ))
            .unwrap(),
            ImageInputMode::Text
        );
        assert_eq!(
            block_on(decide_image_input_mode("p", "m", &cfg, "", &Stub::Ok(None))).unwrap(),
            ImageInputMode::Text
        );
    }

    #[test]
    fn decide_propagates_lookup_error() {
        // The decision layer must not swallow a failed lookup into a text fallback.
        let cfg = json!({});
        let err = block_on(decide_image_input_mode("p", "m", &cfg, "", &Stub::Err));
        assert!(err.is_err());
    }

    // ---- resolve_inference_base_url / _api_key ----------------------------

    // No runtime value present: empty strings stand in for Python's fallthrough.
    const NO_RUNTIME: InferenceRuntime = InferenceRuntime {
        provider: "",
        base_url: "",
        api_key: "",
    };

    #[test]
    fn base_url_runtime_used_when_provider_matches_or_unrequested() {
        let cfg = json!({"model": {"base_url": "http://cfg"}});
        // Requested provider matches runtime provider (case-insensitive, stripped).
        let rt = InferenceRuntime {
            provider: " OpenAI ",
            base_url: " http://runtime ",
            api_key: "",
        };
        assert_eq!(
            resolve_inference_base_url(&cfg, "openai", &rt),
            "http://runtime"
        );
        // No requested provider still takes the runtime value.
        assert_eq!(resolve_inference_base_url(&cfg, "", &rt), "http://runtime");
    }

    #[test]
    fn base_url_runtime_skipped_on_provider_mismatch() {
        // Provider requested but different from the runtime provider: the runtime
        // base_url must not be borrowed across providers. Falls to config.
        let cfg = json!({"model": {"base_url": "http://cfg"}});
        let rt = InferenceRuntime {
            provider: "openai",
            base_url: "http://runtime",
            api_key: "",
        };
        assert_eq!(
            resolve_inference_base_url(&cfg, "ollama", &rt),
            "http://cfg"
        );
    }

    #[test]
    fn base_url_config_waterfall_model_then_providers_then_legacy() {
        // model.base_url wins first.
        let cfg = json!({
            "model": {"base_url": "http://model", "provider": "p"},
            "providers": {"p": {"base_url": "http://prov"}},
        });
        assert_eq!(
            resolve_inference_base_url(&cfg, "p", &NO_RUNTIME),
            "http://model"
        );

        // No model.base_url: providers.<candidate>.base_url.
        let cfg = json!({"providers": {"p": {"base_url": "http://prov"}}});
        assert_eq!(
            resolve_inference_base_url(&cfg, "p", &NO_RUNTIME),
            "http://prov"
        );

        // custom: expansion reaches a bare-named providers block.
        let cfg = json!({"providers": {"myvllm": {"base_url": "http://v"}}});
        assert_eq!(
            resolve_inference_base_url(&cfg, "custom:myvllm", &NO_RUNTIME),
            "http://v"
        );

        // A bare provider also tries its custom:-prefixed form.
        let cfg = json!({"providers": {"custom:p": {"base_url": "http://cp"}}});
        assert_eq!(
            resolve_inference_base_url(&cfg, "p", &NO_RUNTIME),
            "http://cp"
        );

        // Legacy list, matched case-insensitively by name.
        let cfg = json!({"custom_providers": [{"name": "P", "base_url": "http://legacy"}]});
        assert_eq!(
            resolve_inference_base_url(&cfg, "p", &NO_RUNTIME),
            "http://legacy"
        );
    }

    #[test]
    fn base_url_candidate_precedence_requested_before_config_provider() {
        // Deterministic precedence: the requested provider's block is consulted
        // before the config-declared model.provider's block.
        let cfg = json!({
            "model": {"provider": "cfgp"},
            "providers": {
                "reqp": {"base_url": "http://req"},
                "cfgp": {"base_url": "http://cfg"},
            },
        });
        assert_eq!(
            resolve_inference_base_url(&cfg, "reqp", &NO_RUNTIME),
            "http://req"
        );
    }

    #[test]
    fn base_url_empty_when_nothing_declared() {
        assert_eq!(
            resolve_inference_base_url(&Value::Null, "p", &NO_RUNTIME),
            ""
        );
        assert_eq!(resolve_inference_base_url(&json!({}), "p", &NO_RUNTIME), "");
        // Falsy config values coerce to "" and are skipped, not returned.
        let cfg = json!({"model": {"base_url": 0, "provider": false}});
        assert_eq!(resolve_inference_base_url(&cfg, "", &NO_RUNTIME), "");
    }

    #[test]
    fn api_key_runtime_used_regardless_of_provider() {
        // Asymmetry vs. base_url: a runtime api_key is honored even when the
        // requested provider differs from the runtime provider.
        let cfg = json!({"model": {"api_key": "cfg-key"}});
        let rt = InferenceRuntime {
            provider: "openai",
            base_url: "",
            api_key: " rt-key ",
        };
        assert_eq!(resolve_inference_api_key(&cfg, "ollama", &rt), "rt-key");
    }

    #[test]
    fn api_key_config_waterfall() {
        // model.api_key first.
        let cfg = json!({
            "model": {"api_key": "model-key", "provider": "p"},
            "providers": {"p": {"api_key": "prov-key"}},
        });
        assert_eq!(
            resolve_inference_api_key(&cfg, "p", &NO_RUNTIME),
            "model-key"
        );

        // Then providers.<candidate>.api_key.
        let cfg = json!({"providers": {"p": {"api_key": "prov-key"}}});
        assert_eq!(
            resolve_inference_api_key(&cfg, "p", &NO_RUNTIME),
            "prov-key"
        );

        // Then legacy list.
        let cfg = json!({"custom_providers": [{"name": "p", "api_key": "legacy-key"}]});
        assert_eq!(
            resolve_inference_api_key(&cfg, "p", &NO_RUNTIME),
            "legacy-key"
        );

        // Nothing declared.
        assert_eq!(resolve_inference_api_key(&json!({}), "p", &NO_RUNTIME), "");
    }

    #[test]
    fn candidate_names_first_seen_order_and_dedupe() {
        // provider then its custom: form, then config provider then its form.
        assert_eq!(
            inference_candidate_names("p", "c"),
            vec!["p", "custom:p", "c", "custom:c"]
        );
        // custom:-prefixed provider expands to the bare name.
        assert_eq!(
            inference_candidate_names("custom:x", ""),
            vec!["custom:x", "x"]
        );
        // Overlapping expansions dedupe, first-seen order preserved.
        assert_eq!(
            inference_candidate_names("p", "custom:p"),
            vec!["p", "custom:p"]
        );
    }
}

#[cfg(test)]
mod golden_corpus {
    use super::*;
    use anyhow::bail;
    use serde_json::json;
    use std::sync::Mutex;

    #[tokio::test]
    async fn live_waterfall_stops_at_first_definite_answer() {
        use axum::{
            extract::State,
            http::{StatusCode, Uri},
            Json, Router,
        };
        use std::sync::{Arc, RwLock};
        for (index, provider, model, override_value, managed_state, expected, expected_calls) in [
            (0, "openai", "m", Some(false), false, Some(false), vec![]),
            (
                1,
                "llamacpp",
                "m",
                None,
                true,
                Some(false),
                vec!["health", "props"],
            ),
            (2, "openai", "m", None, false, Some(false), vec!["catalog"]),
            (
                3,
                "openai",
                "local:unknown",
                None,
                false,
                Some(true),
                vec!["catalog", "detect", "tags", "show:unknown"],
            ),
            (
                4,
                "custom",
                "local:7b",
                None,
                false,
                Some(true),
                vec!["detect", "tags", "show:local:7b"],
            ),
            (5, "", "", None, false, None, vec![]),
        ] {
            let calls = Arc::new(Mutex::new(Vec::<String>::new()));
            let app = Router::new().fallback(|State(calls): State<Arc<Mutex<Vec<String>>>>, uri: Uri, body: String| async move {
                let (name, status, data) = match uri.path() {
                    "/catalog" => ("catalog".into(), StatusCode::OK, json!({"openai": {"models": {"m": {"attachment": false}}}})),
                    "/health" => ("health".into(), StatusCode::OK, json!({})),
                    "/props" => ("props".into(), StatusCode::OK, json!({"modalities": {"vision": false}})),
                    "/api/v1/models" => ("detect".into(), StatusCode::NOT_FOUND, json!({})),
                    "/api/tags" => ("tags".into(), StatusCode::OK, json!({"models": []})),
                    "/api/show" => {
                        let body: Value = serde_json::from_str(&body).unwrap();
                        (format!("show:{}", body["name"].as_str().unwrap()), StatusCode::OK, json!({"capabilities": ["vision"]}))
                    },
                    path => panic!("unexpected request {path}"),
                };
                calls.lock().unwrap().push(name);
                (status, Json(data))
            }).with_state(calls.clone());
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let base = format!("http://{}", listener.local_addr().unwrap());
            let home = std::env::temp_dir()
                .join(format!("hermes-live-vision-{}-{index}", std::process::id()));
            struct Cleanup(std::path::PathBuf, tokio::task::JoinHandle<()>);
            impl Drop for Cleanup {
                fn drop(&mut self) {
                    self.1.abort();
                    let _ = std::fs::remove_dir_all(&self.0);
                }
            }
            let _cleanup = Cleanup(
                home.clone(),
                tokio::spawn(async move {
                    axum::serve(listener, app).await.unwrap();
                }),
            );
            if managed_state {
                std::fs::create_dir_all(home.join("models")).unwrap();
                std::fs::write(home.join("models/m.gguf"), b"").unwrap();
                std::fs::create_dir_all(home.join("runtimes/llamacpp")).unwrap();
                std::fs::write(
                    home.join("runtimes/llamacpp/server.json"),
                    json!({"pid": std::process::id(), "base_url": format!("{base}/v1")})
                        .to_string(),
                )
                .unwrap();
            }
            let catalog = crate::models_dev::ModelsDev::new(
                home.clone(),
                &json!({"models_dev": {"url": format!("{base}/catalog")}}),
            );
            let managed = crate::managed_capabilities::ManagedCapabilities::new(
                home.clone(),
                json!({"models": []}),
            );
            let probes = crate::local_probe::LocalProbe::new(home);
            let profiles = crate::provider_registry::ProviderRegistry::default();
            profiles.register(Arc::new(RwLock::new(
                crate::provider_registry::ProviderProfile::new("local"),
            )));
            let endpoint = format!("{base}/v1");
            let lookup = LiveVisionLookup {
                runtime: VisionRuntime {
                    inference: InferenceRuntime {
                        provider,
                        base_url: &endpoint,
                        api_key: "",
                    },
                    model,
                    requested_provider: "",
                },
                profiles: &profiles,
                managed: &managed,
                catalog: &catalog,
                probes: &probes,
            };
            let cfg = json!({"model": {"supports_vision": override_value}});
            assert_eq!(
                lookup.lookup(provider, model, &cfg, "").await.unwrap(),
                expected,
                "case {index}"
            );
            assert_eq!(*calls.lock().unwrap(), expected_calls, "case {index}");
            if expected.is_some() {
                assert_eq!(
                    decide_image_input_mode(provider, model, &cfg, "", &lookup)
                        .await
                        .unwrap(),
                    if expected == Some(true) {
                        ImageInputMode::Native
                    } else {
                        ImageInputMode::Text
                    }
                );
            }
        }
    }

    #[tokio::test]
    async fn live_lookup_borrows_requested_identity_only_for_the_same_runtime_model() {
        let home =
            std::env::temp_dir().join(format!("hermes-vision-identity-{}", std::process::id()));
        let profiles = crate::provider_registry::ProviderRegistry::default();
        let managed =
            crate::managed_capabilities::ManagedCapabilities::new(home.clone(), json!({}));
        let catalog = crate::models_dev::ModelsDev::new(home.clone(), &json!({}));
        let probes = crate::local_probe::LocalProbe::new(home);
        let lookup = LiveVisionLookup {
            runtime: VisionRuntime {
                inference: InferenceRuntime {
                    provider: " CUSTOM ",
                    base_url: "",
                    api_key: "",
                },
                model: " m ",
                requested_provider: " fast ",
            },
            profiles: &profiles,
            managed: &managed,
            catalog: &catalog,
            probes: &probes,
        };
        let cfg = json!({"providers": {"fast": {"models": {"m": {"supports_vision": true}, "q": {"supports_vision": true}}}, "slow": {"models": {"m": {"supports_vision": false}}}}});
        assert_eq!(
            lookup.lookup("custom", "m", &cfg, "").await.unwrap(),
            Some(true)
        );
        assert_eq!(lookup.lookup("custom", "q", &cfg, "").await.unwrap(), None);
        assert_eq!(lookup.lookup("other", "m", &cfg, "").await.unwrap(), None);
        assert_eq!(
            lookup.lookup("custom", "m", &cfg, "slow").await.unwrap(),
            Some(false)
        );
    }

    #[tokio::test]
    async fn ollama_fallback_resolves_endpoint_and_key_before_real_http() {
        use axum::{
            extract::State,
            http::{HeaderMap, StatusCode},
            routing::{get, post},
            Json, Router,
        };
        use std::sync::Arc;
        let requests = Arc::new(Mutex::new(Vec::<Value>::new()));
        let router = Router::new()
            .route("/api/v1/models", get(|| async { StatusCode::NOT_FOUND }))
            .route("/api/tags", get(|State(requests): State<Arc<Mutex<Vec<Value>>>>, headers: HeaderMap| async move {
                requests.lock().unwrap().push(json!(["tags", headers["authorization"].to_str().unwrap()]));
                Json(json!({"models": []}))
            }))
            .route("/api/show", post(|State(requests): State<Arc<Mutex<Vec<Value>>>>, headers: HeaderMap, Json(body): Json<Value>| async move {
                requests.lock().unwrap().push(json!(["show", headers["authorization"].to_str().unwrap(), body]));
                Json(json!({"capabilities": ["vision"]}))
            }))
            .with_state(requests.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let url = format!("http://{address}/v1");
        let home = std::env::temp_dir().join(format!(
            "hermes-ollama-routing-{}-{}",
            std::process::id(),
            address.port()
        ));
        struct Cleanup(std::path::PathBuf, tokio::task::JoinHandle<()>);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                self.1.abort();
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let _cleanup = Cleanup(
            home.clone(),
            tokio::spawn(async move {
                axum::serve(listener, router).await.unwrap();
            }),
        );
        let probes = crate::local_probe::LocalProbe::new(home);
        let cfg = json!({"providers": {"custom:box": {"base_url": url, "api_key": "config-key"}}});
        let runtime = InferenceRuntime {
            provider: "other",
            base_url: "http://unused.invalid",
            api_key: " runtime-key ",
        };
        let result =
            lookup_ollama_vision("custom:box", "image-model:7b", &cfg, &runtime, &probes).await;
        assert_eq!(result, Some(true));
        assert_eq!(
            *requests.lock().unwrap(),
            vec![
                json!(["tags", "Bearer runtime-key"]),
                json!(["show", "Bearer runtime-key", {"name": "image-model:7b"}]),
            ]
        );
    }

    #[test]
    fn inference_endpoints_match_python() {
        let cases: Value = serde_json::from_str(include_str!(
            "../../../tools/inference-endpoint-goldens.json"
        ))
        .unwrap();
        for case in cases.as_array().unwrap() {
            let runtime = &case["runtime"];
            let runtime = InferenceRuntime {
                provider: runtime["provider"].as_str().unwrap_or(""),
                base_url: runtime["base_url"].as_str().unwrap_or(""),
                api_key: runtime["api_key"].as_str().unwrap_or(""),
            };
            let provider = case["provider"].as_str().unwrap();
            assert_eq!(
                resolve_inference_base_url(&case["cfg"], provider, &runtime),
                case["base_url"],
                "{case}"
            );
            assert_eq!(
                resolve_inference_api_key(&case["cfg"], provider, &runtime),
                case["api_key"],
                "{case}"
            );
        }
    }

    fn mode_name(mode: ImageInputMode) -> &'static str {
        match mode {
            ImageInputMode::Auto => "auto",
            ImageInputMode::Native => "native",
            ImageInputMode::Text => "text",
        }
    }

    struct Lookup {
        value: Value,
        calls: Mutex<Vec<Value>>,
    }
    #[async_trait]
    impl VisionCapabilityLookup for Lookup {
        async fn lookup(
            &self,
            provider: &str,
            model: &str,
            _cfg: &Value,
            requested: &str,
        ) -> Result<Option<bool>> {
            self.calls
                .lock()
                .unwrap()
                .push(json!([provider, model, requested]));
            if self.value == "error" {
                bail!("lookup failed");
            }
            Ok(self.value.as_bool())
        }
    }

    #[tokio::test]
    async fn routing_matches_python() {
        let fixture: Value =
            serde_json::from_str(include_str!("../../../tools/image-routing-goldens.json"))
                .unwrap();
        for case in fixture["coercion"].as_array().unwrap() {
            assert_eq!(
                json!(coerce_capability_bool(&case["value"])),
                case["expected"],
                "{case}"
            );
        }
        for case in fixture["modes"].as_array().unwrap() {
            assert_eq!(
                mode_name(coerce_mode(&case["value"])),
                case["expected"].as_str().unwrap(),
                "{case}"
            );
        }
        for case in fixture["overrides"].as_array().unwrap() {
            let result = supports_vision_override(
                &case["cfg"],
                case["provider"].as_str().unwrap(),
                case["model"].as_str().unwrap(),
                case["requested"].as_str().unwrap(),
            );
            assert_eq!(json!(result), case["expected"], "{case}");
        }
        for case in fixture["aux"].as_array().unwrap() {
            assert_eq!(
                json!(explicit_aux_vision_override(&case["cfg"])),
                case["expected"],
                "{case}"
            );
        }
        for case in fixture["decisions"].as_array().unwrap() {
            let lookup = Lookup {
                value: case["capability"].clone(),
                calls: Mutex::new(Vec::new()),
            };
            let result = decide_image_input_mode(
                "custom",
                "m",
                &case["cfg"],
                case["requested"].as_str().unwrap(),
                &lookup,
            )
            .await;
            let actual = result.map(mode_name).unwrap_or("error");
            assert_eq!(actual, case["expected"].as_str().unwrap(), "{case}");
            assert_eq!(
                json!(*lookup.calls.lock().unwrap()),
                case["calls"],
                "{case}"
            );
        }
    }
}
