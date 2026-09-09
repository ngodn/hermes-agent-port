//! Live health state and deterministic native subset ordering for built-in
//! auxiliary compression discovery.
//!
//! Candidate clients remain conversation-scoped because they contain secrets.
//! Only provider health is shared across conversations, keyed by profile home,
//! so one profile can never quarantine another profile's credential surface.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

pub(crate) const UNHEALTHY_TTL: Duration = Duration::from_secs(600);

#[derive(Default)]
pub(crate) struct Health {
    unhealthy_until: Mutex<HashMap<(PathBuf, String), Instant>>,
}

impl Health {
    pub(crate) fn mark(&self, home: &Path, provider: &str, now: Instant) {
        self.mark_for(home, provider, now, UNHEALTHY_TTL);
    }

    pub(crate) fn mark_for(&self, home: &Path, provider: &str, now: Instant, ttl: Duration) {
        let provider = normalize_label(provider);
        if provider.is_empty() {
            return;
        }
        self.unhealthy_until
            .lock()
            .unwrap()
            .insert((home.to_path_buf(), provider), now + ttl);
    }

    pub(crate) fn is_unhealthy(&self, home: &Path, provider: &str, now: Instant) -> bool {
        let key = (home.to_path_buf(), normalize_label(provider));
        if key.1.is_empty() {
            return false;
        }
        let mut state = self.unhealthy_until.lock().unwrap();
        match state.get(&key).copied() {
            Some(until) if now < until => true,
            Some(_) => {
                state.remove(&key);
                false
            }
            None => false,
        }
    }
}

pub(crate) fn normalize_label(provider: &str) -> String {
    match provider.trim().to_lowercase().as_str() {
        "custom" => "local/custom".into(),
        "codex" => "openai-codex".into(),
        value => value.into(),
    }
}

/// Map a resolved native provider back to Python's built-in discovery label.
/// Direct API-key profiles all occupy the single `api-key` chain slot.
pub(crate) fn candidate_chain_label(provider: &str) -> String {
    let provider = normalize_label(provider);
    if provider.starts_with("custom:") {
        return "local/custom".into();
    }
    match provider.as_str() {
        "openrouter" | "nous" | "local/custom" | "openai-codex" => provider,
        _ => "api-key".into(),
    }
}

/// Manual entries in `hermes_cli.auth.PROVIDER_REGISTRY`, in insertion order.
/// Profiles that Python appends dynamically follow these in registry order.
const API_KEY_PROVIDER_ORDER: &[&str] = &[
    "openai-api",
    "lmstudio",
    "copilot",
    "gemini",
    "zai",
    "kimi-coding",
    "kimi-coding-cn",
    "stepfun",
    "arcee",
    "gmi",
    "actual",
    "minimax",
    "anthropic",
    "alibaba",
    "alibaba-coding-plan",
    "minimax-cn",
    "deepseek",
    "xai",
    "nvidia",
    "ai-gateway",
    "opencode-zen",
    "opencode-go",
    "opencode-free",
    "kilocode",
    "huggingface",
    "xiaomi",
    "tencent-tokenhub",
    "tencent-tokenplan",
    "ollama-cloud",
    "azure-foundry",
];

/// Return the strict native subset of Python's API-key discovery tier.
/// Unsupported wire modes, profiles without a known auxiliary model, and
/// OAuth providers are deliberately absent instead of being misrouted.
pub(crate) fn ordered_native_profiles(
    registry: &crate::provider_registry::ProviderRegistry,
) -> Vec<crate::provider_registry::ProviderProfile> {
    let profiles: Vec<_> = registry
        .list()
        .into_iter()
        .map(|profile| profile.read().unwrap().clone())
        .filter(|profile| {
            profile.auth_type == "api_key"
                && profile.api_mode == "chat_completions"
                && !profile.default_aux_model.trim().is_empty()
        })
        .collect();
    let mut result = Vec::new();
    for name in API_KEY_PROVIDER_ORDER {
        if let Some(profile) = profiles.iter().find(|profile| profile.name == *name) {
            result.push(profile.clone());
        }
    }
    for profile in profiles {
        if !result.iter().any(|known| known.name == profile.name) {
            result.push(profile);
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn health_is_profile_scoped_and_lazily_expires() {
        let health = Health::default();
        let now = Instant::now();
        let red = Path::new("/profiles/red");
        let blue = Path::new("/profiles/blue");
        health.mark_for(red, "Custom", now, Duration::from_secs(10));
        assert!(health.is_unhealthy(red, "local/custom", now));
        assert!(!health.is_unhealthy(blue, "custom", now));
        assert!(!health.is_unhealthy(red, "custom", now + Duration::from_secs(10)));
        assert!(!health.is_unhealthy(red, "custom", now));
    }

    #[test]
    fn native_profile_subset_keeps_python_order() {
        let registry = crate::provider_registry::ProviderRegistry::default();
        registry.register_bundled_base_profiles("fixture");
        registry.register_vercel();
        let names: Vec<_> = ordered_native_profiles(&registry)
            .into_iter()
            .map(|profile| profile.name)
            .collect();
        assert_eq!(
            names,
            [
                "stepfun",
                "gmi",
                "ai-gateway",
                "kilocode",
                "fireworks",
                "novita"
            ]
        );
    }

    #[test]
    fn resolved_providers_map_back_to_python_chain_slots() {
        assert_eq!(candidate_chain_label("openrouter"), "openrouter");
        assert_eq!(candidate_chain_label("custom"), "local/custom");
        assert_eq!(candidate_chain_label("custom:lab"), "local/custom");
        assert_eq!(candidate_chain_label("gmi"), "api-key");
    }

    #[test]
    fn golden_contract_pins_chain_and_health_values() {
        let corpus: serde_json::Value = serde_json::from_str(include_str!(
            "../../../tools/compression-builtin-discovery-goldens.json"
        ))
        .unwrap();
        let chain = &corpus["provider_chain_and_order"];
        let exact = chain
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["case"] == "provider_chain_exact_order_and_callables")
            .unwrap();
        assert_eq!(
            exact["labels"],
            serde_json::json!(["openrouter", "nous", "local/custom", "api-key"])
        );
        let health = &corpus["unhealthy_cache_mutation_and_ttl"];
        let default_ttl = health
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["case"] == "default_unhealthy_ttl_is_600s")
            .unwrap();
        assert_eq!(default_ttl["matches_600s"], true);
        assert_eq!(UNHEALTHY_TTL, Duration::from_secs(600));
        let openrouter = &corpus["openrouter_discovery_gates"];
        let default_model = openrouter
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["case"] == "openrouter_built_in_default_model")
            .unwrap();
        assert_eq!(
            default_model["default_model"],
            "nvidia/nemotron-3-ultra-550b-a55b:free"
        );
        let budget = &corpus["startup_vs_runtime_paths_and_budget"];
        let auth = budget
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["case"] == "candidate_auth_error_second_discovery_attempt")
            .unwrap();
        assert_eq!(auth["maximum_discovery_candidates_io"], 2);
    }
}
