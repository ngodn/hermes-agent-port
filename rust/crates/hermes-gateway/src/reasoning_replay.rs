//! Reasoning replay policy for Chat Completions endpoints.
//!
//! Pure Python source port of the single-owner `reasoning_content` policy from
//! `agent/message_sanitization.py` (audit F4):
//! - Provider family detection table (`_REASONING_ECHO_RULES`) and classifiers
//!   (`matches_reasoning_echo_family`, `reasoning_echo_family`, `needs_reasoning_echo`).
//! - Outbound wire policy application (`apply_reasoning_content_policy`, `apply`)
//!   preserving insertion order.
//!
//! Host matching faithfully reproduces `utils.base_url_hostname` and
//! `utils.base_url_host_matches` via [`crate::local_probe::urlparse_hostname`] and
//! [`crate::python_value::python_whitespace`].

use serde_json::Value;

/// Specification for an echo-back provider family.
///
/// Ports `_REASONING_ECHO_RULES` from `agent/message_sanitization.py`:
/// - `kimi`: provider `kimi-coding` / `kimi-coding-cn`, or host `api.kimi.com`,
///   `moonshot.ai`, `moonshot.cn`. Host-driven on purpose: aggregators re-exporting
///   kimi models reject the echo.
/// - `deepseek`: provider `deepseek` (lowered), model contains `deepseek` (lowered),
///   or host `api.deepseek.com` (#15250; V4 rejects empty-string pads, hence " ").
/// - `mimo`: provider `xiaomi` (lowered), model contains `mimo` (lowered), or host
///   `api.xiaomimimo.com`, `xiaomimimo.com`.
struct ReasoningEchoRule {
    pub family: &'static str,
    pub raw_providers: &'static [&'static str],
    pub lowered_providers: &'static [&'static str],
    pub model_substrings: &'static [&'static str],
    pub hosts: &'static [&'static str],
}

const REASONING_ECHO_RULES: &[ReasoningEchoRule] = &[
    ReasoningEchoRule {
        family: "kimi",
        raw_providers: &["kimi-coding", "kimi-coding-cn"],
        lowered_providers: &[],
        model_substrings: &[],
        hosts: &["api.kimi.com", "moonshot.ai", "moonshot.cn"],
    },
    ReasoningEchoRule {
        family: "deepseek",
        raw_providers: &[],
        lowered_providers: &["deepseek"],
        model_substrings: &["deepseek"],
        hosts: &["api.deepseek.com"],
    },
    ReasoningEchoRule {
        family: "mimo",
        raw_providers: &[],
        lowered_providers: &["xiaomi"],
        model_substrings: &["mimo"],
        hosts: &["api.xiaomimimo.com", "xiaomimimo.com"],
    },
];

/// Return the lowercased hostname for a base URL, or `""` if absent.
///
/// Port of `utils.base_url_hostname`.
fn base_url_hostname(base_url: &str) -> String {
    let raw = base_url.trim_matches(crate::python_value::python_whitespace);
    if raw.is_empty() {
        return String::new();
    }
    let url = if raw.contains("://") {
        raw.to_string()
    } else {
        format!("//{raw}")
    };
    let host = crate::local_probe::urlparse_hostname(&url);
    host.to_lowercase().trim_end_matches('.').to_string()
}

/// Return true when the base URL's hostname is `domain` or a subdomain.
///
/// Counterpart to `domain in base_url` without substring false-positives.
/// Port of `utils.base_url_host_matches`.
fn base_url_host_matches(base_url: &str, domain: &str) -> bool {
    let hostname = base_url_hostname(base_url);
    if hostname.is_empty() {
        return false;
    }
    let domain = domain
        .trim_matches(crate::python_value::python_whitespace)
        .to_lowercase();
    let domain = domain.trim_end_matches('.');
    if domain.is_empty() {
        return false;
    }
    hostname == domain || hostname.ends_with(&format!(".{domain}"))
}

/// True when `(provider, model, base_url)` matches one echo-back family.
///
/// Families can overlap (e.g. a deepseek-named model pointed at a kimi host);
/// this membership test is independent per family.
/// Port of `matches_reasoning_echo_family`.
fn matches_reasoning_echo_family(
    family: &str,
    provider: &str,
    model: &str,
    base_url: &str,
) -> bool {
    let Some(rule) = REASONING_ECHO_RULES.iter().find(|r| r.family == family) else {
        return false;
    };
    let provider_lower = provider.to_lowercase();
    let model_lower = model.to_lowercase();

    if rule.raw_providers.contains(&provider)
        || rule.lowered_providers.contains(&provider_lower.as_str())
    {
        return true;
    }
    if rule
        .model_substrings
        .iter()
        .any(|sub| model_lower.contains(sub))
    {
        return true;
    }
    rule.hosts
        .iter()
        .any(|host| base_url_host_matches(base_url, host))
}

/// Classify the provider direction for the `reasoning_content` echo policy.
///
/// Returns `"kimi"`, `"deepseek"`, or `"mimo"` (first match in table order)
/// when the target endpoint enforces `reasoning_content` echo-back on
/// assistant turns, else `None` (strict side, where the field must be stripped).
/// Port of `reasoning_echo_family`.
fn reasoning_echo_family(provider: &str, model: &str, base_url: &str) -> Option<&'static str> {
    for rule in REASONING_ECHO_RULES {
        if matches_reasoning_echo_family(rule.family, provider, model, base_url) {
            return Some(rule.family);
        }
    }
    None
}

/// True when the endpoint requires `reasoning_content` echo-back on assistant turns.
///
/// Primary entry point used by NativeAgentClient and transports.
/// Port of `needs_reasoning_echo`.
pub fn needs_echo(provider: &str, model: &str, base_url: &str) -> bool {
    reasoning_echo_family(provider, model, base_url).is_some()
}

/// Copy provider-facing reasoning fields onto an API replay message.
///
/// Port of `apply_reasoning_content_policy` from `agent/message_sanitization.py`.
///
/// # Rules
/// 1. If `role` is not `"assistant"`, no-op.
/// 2. If `reasoning_content` is a string:
///    - when `needs_thinking_pad` is false: remove `reasoning_content`.
///    - when `needs_thinking_pad` is true and `reasoning_content` is `""`: upgrade to `" "` (#17341).
///    - when `needs_thinking_pad` is true and non-empty: preserve verbatim.
/// 3. Cross-provider poisoned history (#15748): on require-side, if `tool_calls` is present
///    and truthy, AND `reasoning` is a non-empty string, but `reasoning_content` was absent/non-string,
///    pad with `" "` instead of leaking foreign CoT.
/// 4. Healthy session: promote `reasoning` field to `reasoning_content` on require-side.
///    On strict side, remove `reasoning_content`.
/// 5. Bare assistant turn on require-side: pad with `" "` (#17341).
/// 6. Non-string `reasoning_content` (e.g. None after compaction) on strict side: remove.
fn apply_reasoning_content_policy(
    source_msg: &Value,
    api_msg: &mut Value,
    needs_thinking_pad: bool,
) {
    let Some(source_obj) = source_msg.as_object() else {
        return;
    };
    if source_obj.get("role").and_then(Value::as_str) != Some("assistant") {
        return;
    }
    let Some(api_obj) = api_msg.as_object_mut() else {
        return;
    };

    // 1. Explicit reasoning_content already set.
    if let Some(existing) = source_obj.get("reasoning_content").and_then(Value::as_str) {
        if !needs_thinking_pad {
            api_obj.shift_remove("reasoning_content");
        } else if existing.is_empty() {
            api_obj.insert("reasoning_content".to_string(), Value::String(" ".into()));
        } else {
            api_obj.insert(
                "reasoning_content".to_string(),
                Value::String(existing.to_string()),
            );
        }
        return;
    }

    // 2. Cross-provider poisoned history (#15748): on DeepSeek/Kimi,
    // if the source turn has tool_calls AND a 'reasoning' field but no
    // 'reasoning_content' key, the 'reasoning' text was written by a
    // prior provider (e.g. MiniMax). Inject a single space to satisfy the API
    // without leaking another provider's chain of thought to DeepSeek/Kimi.
    let normalized_reasoning = source_obj.get("reasoning").and_then(Value::as_str);
    let has_tool_calls = source_obj
        .get("tool_calls")
        .map(crate::python_value::truthy)
        .unwrap_or(false);

    if needs_thinking_pad && has_tool_calls && normalized_reasoning.is_some_and(|r| !r.is_empty()) {
        api_obj.insert("reasoning_content".to_string(), Value::String(" ".into()));
        return;
    }

    // 3. Healthy session: promote 'reasoning' field to 'reasoning_content'
    // for providers that use the internal 'reasoning' key.
    if let Some(reasoning) = normalized_reasoning.filter(|r| !r.is_empty()) {
        if needs_thinking_pad {
            api_obj.insert(
                "reasoning_content".to_string(),
                Value::String(reasoning.to_string()),
            );
        } else {
            api_obj.shift_remove("reasoning_content");
        }
        return;
    }

    // 4. DeepSeek / Kimi thinking mode: all assistant messages need
    // reasoning_content. Inject a single space to satisfy the provider's
    // requirement when no explicit reasoning content is present.
    if needs_thinking_pad {
        api_obj.insert("reasoning_content".to_string(), Value::String(" ".into()));
        return;
    }

    // 5. reasoning_content was present but not a string (e.g. None after
    // context compaction). Don't pass null to the API.
    api_obj.shift_remove("reasoning_content");
}

/// Apply reasoning content policy in-place to a message, preserving insertion order.
///
/// Modifies `message` in-place according to `needs_echo`.
/// If `needs_echo` is true, ensures `reasoning_content` is present (padded with `" "`
/// or preserved/promoted). If `needs_echo` is false, removes `reasoning_content` using
/// `shift_remove` to preserve the order of other keys. Non-assistant messages are untouched.
pub fn apply(message: &mut Value, needs_echo: bool) {
    let source = message.clone();
    apply_reasoning_content_policy(&source, message, needs_echo);
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;
    use serde_json::json;

    #[derive(Deserialize)]
    struct HostnameCase {
        name: String,
        base_url: String,
        domain: String,
        expected_hostname: String,
        expected_match: bool,
    }

    #[derive(Deserialize)]
    struct EchoCase {
        name: String,
        provider: String,
        model: String,
        base_url: String,
        expected_family: Option<String>,
        expected_needs_echo: bool,
        matches_kimi: bool,
        matches_deepseek: bool,
        matches_mimo: bool,
    }

    #[derive(Deserialize)]
    struct ApplyCase {
        name: String,
        source_msg: Value,
        api_msg: Value,
        needs_thinking_pad: bool,
        expected_api_msg: Value,
        expected_in_place_msg: Value,
    }

    #[derive(Deserialize)]
    struct Goldens {
        hostname_cases: Vec<HostnameCase>,
        echo_cases: Vec<EchoCase>,
        apply_cases: Vec<ApplyCase>,
    }

    fn load_goldens() -> Goldens {
        let raw = include_str!("../../../tools/reasoning-replay-goldens.json");
        serde_json::from_str(raw).expect("reasoning replay goldens JSON parse")
    }

    #[test]
    fn test_goldens_hostname_cases() {
        let goldens = load_goldens();
        assert!(!goldens.hostname_cases.is_empty());
        for case in goldens.hostname_cases {
            let actual_host = base_url_hostname(&case.base_url);
            assert_eq!(
                actual_host, case.expected_hostname,
                "hostname mismatch on case {}",
                case.name
            );
            let actual_match = base_url_host_matches(&case.base_url, &case.domain);
            assert_eq!(
                actual_match, case.expected_match,
                "host_matches mismatch on case {}",
                case.name
            );
        }
    }

    #[test]
    fn test_goldens_echo_cases() {
        let goldens = load_goldens();
        assert!(!goldens.echo_cases.is_empty());
        for case in goldens.echo_cases {
            let family = reasoning_echo_family(&case.provider, &case.model, &case.base_url);
            assert_eq!(
                family,
                case.expected_family.as_deref(),
                "family mismatch on case {}",
                case.name
            );

            let needs = needs_echo(&case.provider, &case.model, &case.base_url);
            assert_eq!(
                needs, case.expected_needs_echo,
                "needs_echo mismatch on case {}",
                case.name
            );

            assert_eq!(
                matches_reasoning_echo_family("kimi", &case.provider, &case.model, &case.base_url),
                case.matches_kimi,
                "matches_kimi mismatch on case {}",
                case.name
            );
            assert_eq!(
                matches_reasoning_echo_family(
                    "deepseek",
                    &case.provider,
                    &case.model,
                    &case.base_url
                ),
                case.matches_deepseek,
                "matches_deepseek mismatch on case {}",
                case.name
            );
            assert_eq!(
                matches_reasoning_echo_family("mimo", &case.provider, &case.model, &case.base_url),
                case.matches_mimo,
                "matches_mimo mismatch on case {}",
                case.name
            );
        }
    }

    #[test]
    fn test_goldens_apply_cases() {
        let goldens = load_goldens();
        assert!(!goldens.apply_cases.is_empty());
        for case in goldens.apply_cases {
            // Test 1: apply_reasoning_content_policy(source, api, needs)
            let mut api = case.api_msg.clone();
            apply_reasoning_content_policy(&case.source_msg, &mut api, case.needs_thinking_pad);
            assert_eq!(
                api, case.expected_api_msg,
                "apply_reasoning_content_policy mismatch on case {}",
                case.name
            );

            // Test 2: apply(in_place, needs)
            let mut in_place = case.source_msg.clone();
            apply(&mut in_place, case.needs_thinking_pad);
            assert_eq!(
                in_place, case.expected_in_place_msg,
                "apply (in-place) mismatch on case {}",
                case.name
            );
        }
    }

    #[test]
    fn test_insertion_order_preserved() {
        // When replacing reasoning_content, position should be preserved.
        let mut msg = json!({
            "role": "assistant",
            "reasoning_content": "",
            "content": "answer",
            "name": "bot"
        });
        apply(&mut msg, true);
        let keys: Vec<&str> = msg
            .as_object()
            .unwrap()
            .keys()
            .map(|k| k.as_str())
            .collect();
        assert_eq!(keys, vec!["role", "reasoning_content", "content", "name"]);
        assert_eq!(msg["reasoning_content"], " ");

        // When removing reasoning_content, other keys remain in relative order.
        apply(&mut msg, false);
        let keys_after: Vec<&str> = msg
            .as_object()
            .unwrap()
            .keys()
            .map(|k| k.as_str())
            .collect();
        assert_eq!(keys_after, vec!["role", "content", "name"]);

        // When inserting reasoning_content where it was absent, it goes to the end.
        apply(&mut msg, true);
        let keys_readded: Vec<&str> = msg
            .as_object()
            .unwrap()
            .keys()
            .map(|k| k.as_str())
            .collect();
        assert_eq!(
            keys_readded,
            vec!["role", "content", "name", "reasoning_content"]
        );
    }

    #[test]
    fn test_non_object_and_unknown_roles_are_noops() {
        let mut raw_str = json!("raw string");
        apply(&mut raw_str, true);
        assert_eq!(raw_str, json!("raw string"));

        let mut user_msg = json!({"role": "user", "content": "hi", "reasoning_content": "keep"});
        apply(&mut user_msg, false);
        assert_eq!(user_msg["reasoning_content"], "keep");
    }
}
