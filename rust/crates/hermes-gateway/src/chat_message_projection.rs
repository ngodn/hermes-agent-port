//! Projection of internal chat messages onto the wire Chat Completions schema.
//!
//! Port of `ChatCompletionsTransport.convert_messages` and
//! `_model_consumes_thought_signature` from `agent/transports/chat_completions.py`.
//! Messages reaching this layer are already OpenAI-shaped; [`convert`] strips the
//! Hermes-internal fields that strict OpenAI-compatible providers reject with
//! HTTP 400/422 (or, on some gateways, 5xx) before the payload leaves the box.
//!
//! What gets stripped, and why (all mirrored from the Python docstring):
//!
//! - Codex Responses API fields: `codex_reasoning_items` / `codex_message_items`
//!   on the message, `call_id` / `response_item_id` on `tool_calls` entries.
//! - `extra_content` on `tool_calls` (Gemini thought_signature). Kept only when
//!   the outgoing `model` is itself Gemini-family, because Gemini 3 thinking
//!   models attach it and reject a follow-up request that omits it, while strict
//!   providers (Fireworks, Mistral) reject any payload that contains it. So it is
//!   dropped for everyone else, including a non-Gemini model that inherited stale
//!   Gemini `extra_content` earlier in a mixed-provider session.
//! - `tool_name` on tool-result messages: written for the SQLite FTS index, not
//!   part of the Chat Completions schema. Strict providers (Fireworks,
//!   Moonshot/Kimi) reject it; permissive ones (OpenRouter, MiniMax) ignore it,
//!   which masked the bug for months.
//! - Other persistence-only sidecars: `effect_disposition`, `timestamp` (#47868),
//!   `platform_message_id` (gateway dedup id), `api_content`
//!   (persist-what-you-send), and the ordered-replay blocks
//!   `anthropic_content_blocks` / `bedrock_content_blocks`, which are durable
//!   history for their native transports and must not cross a provider boundary.
//! - Every top-level message key starting with `_` (e.g.
//!   `_empty_recovery_synthetic`, `_empty_terminal_sentinel`, `_thinking_prefill`).
//!   These are agent-loop scaffolding markers. Permissive providers drop unknown
//!   keys silently; strict gateways reject them and poison every later request in
//!   the session.
//! - An empty (`[]`) or explicit-`null` `tool_calls` on an assistant message:
//!   strict providers (onerouter/Qwen, DeepSeek v4) reject it with
//!   "Empty tool_calls is not supported in message." The key is dropped so the
//!   message stays schema-valid, matching the pre-API sanitizer on other routes.
//!
//! The transform preserves insertion order of message and tool-call keys, leaves
//! nested tool-call structures untouched except for the popped fields, and never
//! mutates the caller's input (it returns fresh values). Because `serde_json` is
//! built with `preserve_order`, key order is faithful to CPython dict order, and
//! `shift_remove` is used throughout so removing a key does not disturb the
//! position of the keys around it.

use serde_json::{Map, Value};

/// Persistence-only / transport-internal sidecar keys that must never reach a
/// strict Chat Completions endpoint. Popped unconditionally when present, which
/// matches the Python `pop(key, None)` calls (a no-op for an absent key).
const SIDECAR_KEYS: [&str; 9] = [
    "codex_reasoning_items",
    "codex_message_items",
    "tool_name",
    "effect_disposition",
    "timestamp",
    "platform_message_id",
    "api_content",
    "anthropic_content_blocks",
    "bedrock_content_blocks",
];

/// Port of `_model_consumes_thought_signature`: true when the outgoing model is
/// a Gemini-family model (Gemini or Gemma) that requires `extra_content`
/// (thought_signature) to be replayed on tool calls. A non-Gemini target must
/// have `extra_content` stripped. `str(model or "").lower()` collapses to `""`
/// for an empty model, so an empty string is (correctly) not Gemini-family.
fn model_consumes_thought_signature(model: &str) -> bool {
    let m = model.to_lowercase();
    m.contains("gemini") || m.contains("gemma")
}

/// Sanitize a single message object, returning a fresh map. `strip_extra_content`
/// is resolved once by the caller from the target model. When nothing needs
/// removing the returned map equals the input, mirroring the Python path that
/// returns the message unchanged.
fn sanitize_message(msg: &Map<String, Value>, strip_extra_content: bool) -> Map<String, Value> {
    let mut out = msg.clone();

    // Persistence-only sidecars. Popping an absent key is a no-op, so removing
    // all nine unconditionally is equivalent to the guarded Python block.
    for key in SIDECAR_KEYS {
        out.shift_remove(key);
    }

    // Hermes-internal scaffolding markers: any top-level `_`-prefixed key.
    // `retain` keeps the surviving keys in their original order.
    out.retain(|key, _| !key.starts_with('_'));

    // `tool_calls` handling reads from the original message: the removals above
    // never touch that key, so original and working copy agree on it. `role` is
    // read the same way Python does with `msg.get("role") == "assistant"`.
    let is_assistant = msg.get("role").and_then(Value::as_str) == Some("assistant");
    match msg.get("tool_calls") {
        Some(Value::Array(tool_calls)) => {
            // An assistant message carrying `tool_calls: []` is rejected by
            // strict providers; drop the key entirely (mirrors the Python
            // `continue`, so the null branch below is not also consulted).
            if is_assistant && tool_calls.is_empty() {
                out.shift_remove("tool_calls");
                return out;
            }
            // Copy the array lazily: only when at least one entry actually needs
            // a field popped, so an untouched array stays shared/equal. Entries
            // that are not objects, and object entries without a stripped field,
            // pass through verbatim.
            let mut copied: Option<Vec<Value>> = None;
            for (idx, tc) in tool_calls.iter().enumerate() {
                let Value::Object(tc_map) = tc else { continue };
                let should_copy = tc_map.contains_key("call_id")
                    || tc_map.contains_key("response_item_id")
                    || (strip_extra_content && tc_map.contains_key("extra_content"));
                if !should_copy {
                    continue;
                }
                let copied = copied.get_or_insert_with(|| tool_calls.clone());
                let mut copied_tc = tc_map.clone();
                copied_tc.shift_remove("call_id");
                copied_tc.shift_remove("response_item_id");
                if strip_extra_content {
                    copied_tc.shift_remove("extra_content");
                }
                copied[idx] = Value::Object(copied_tc);
            }
            if let Some(copied) = copied {
                // `insert` on an existing key keeps its position in the map.
                out.insert("tool_calls".to_string(), Value::Array(copied));
            }
        }
        // Explicit `tool_calls: null` on an assistant message is as invalid as
        // the empty array; drop the key. A null on any other role, or an absent
        // key, is left alone (Python `msg.get` returns None for both, but only
        // the present-and-null assistant case has `"tool_calls" in msg` true and
        // matches the role guard).
        Some(Value::Null) if is_assistant => {
            out.shift_remove("tool_calls");
        }
        // A non-list, non-null `tool_calls` (or an absent key) is untouched:
        // neither Python `isinstance` branch fires.
        _ => {}
    }

    out
}

/// Restore the content previously sent to the provider before stripping
/// bookkeeping fields. Python's turn_context.substitute_api_content only
/// restores nonempty strings on user and assistant messages. Tool/system
/// sidecars are removed without changing their content.
///
/// Call this on the outgoing copy, so persisted clean content stays available.
pub fn substitute_api_content(message: &mut Value) {
    let Some(message) = message.as_object_mut() else {
        return;
    };
    let sidecar = message.shift_remove("api_content");
    if matches!(
        message.get("role").and_then(Value::as_str),
        Some("user" | "assistant")
    ) {
        if let Some(Value::String(content)) = sidecar {
            if !content.is_empty() {
                message.insert("content".into(), Value::String(content));
            }
        }
    }
}

/// Project already-OpenAI-shaped `messages` onto the strict Chat Completions
/// wire schema for `model`, stripping Hermes-internal fields. Non-object entries
/// pass through unchanged. Whether `extra_content` (Gemini thought_signature) is
/// kept depends on `model`, resolved once up front.
pub fn convert(messages: &[Value], model: &str) -> Vec<Value> {
    let strip_extra_content = !model_consumes_thought_signature(model);
    messages
        .iter()
        .map(|msg| match msg {
            Value::Object(map) => Value::Object(sanitize_message(map, strip_extra_content)),
            other => other.clone(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn api_content_substitution_matches_python() {
        let rows: Value =
            serde_json::from_str(include_str!("../../../tools/api-content-goldens.json")).unwrap();
        for row in rows.as_array().unwrap() {
            let mut message = row["input"].clone();
            substitute_api_content(&mut message);
            assert_eq!(message, row["expected"], "{row}");
        }
    }

    #[derive(serde::Deserialize)]
    struct Golden {
        name: String,
        model: String,
        messages: Vec<Value>,
        expected: Vec<Value>,
    }

    #[test]
    fn matches_python_goldens() {
        let raw = include_str!("../../../tools/chat-message-goldens.json");
        let goldens: Vec<Golden> = serde_json::from_str(raw).expect("golden JSON parses");
        assert!(!goldens.is_empty(), "expected at least one golden case");
        for case in &goldens {
            let got = convert(&case.messages, &case.model);
            assert_eq!(got, case.expected, "case `{}`", case.name);
        }
    }

    #[test]
    fn gemini_family_keeps_extra_content() {
        assert!(model_consumes_thought_signature("gemini-3-pro"));
        assert!(model_consumes_thought_signature("GEMINI-3-PRO"));
        assert!(model_consumes_thought_signature("google/gemma-2"));
        assert!(!model_consumes_thought_signature("gpt-4"));
        assert!(!model_consumes_thought_signature(""));
    }

    #[test]
    fn no_op_message_is_returned_equal() {
        let messages = vec![json!({"role": "user", "content": "hi"})];
        assert_eq!(convert(&messages, "gpt-4"), messages);
    }

    #[test]
    fn non_object_messages_pass_through() {
        let messages = vec![json!("raw"), json!(42), Value::Null];
        assert_eq!(convert(&messages, "gpt-4"), messages);
    }

    #[test]
    fn preserves_key_order_after_sidecar_removal() {
        // `timestamp` sits between `role` and `content`; removing it must leave
        // the surviving keys in their original positions.
        let messages = vec![json!({
            "role": "assistant",
            "timestamp": 123,
            "content": "ok",
        })];
        let got = convert(&messages, "gpt-4");
        let keys: Vec<&str> = got[0]
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(keys, ["role", "content"]);
    }

    #[test]
    fn extra_content_kept_for_gemini_stripped_otherwise() {
        let messages = vec![json!({
            "role": "assistant",
            "tool_calls": [{"id": "c1", "extra_content": {"thought_signature": "sig"}}],
        })];
        // Non-Gemini: extra_content removed, so the tool call loses the key.
        let stripped = convert(&messages, "gpt-4");
        assert_eq!(
            stripped[0]["tool_calls"][0],
            json!({"id": "c1"}),
            "extra_content must be stripped for non-Gemini targets"
        );
        // Gemini: extra_content preserved untouched.
        let kept = convert(&messages, "gemini-3-pro");
        assert_eq!(kept, messages, "Gemini target must keep extra_content");
    }

    #[test]
    fn empty_and_null_assistant_tool_calls_are_dropped() {
        let empty = vec![json!({"role": "assistant", "tool_calls": []})];
        assert_eq!(convert(&empty, "gpt-4"), vec![json!({"role": "assistant"})]);

        let null = vec![json!({"role": "assistant", "tool_calls": null})];
        assert_eq!(convert(&null, "gpt-4"), vec![json!({"role": "assistant"})]);

        // Same shapes on a non-assistant role are left alone.
        let user_empty = vec![json!({"role": "user", "tool_calls": []})];
        assert_eq!(convert(&user_empty, "gpt-4"), user_empty);
        let user_null = vec![json!({"role": "user", "tool_calls": null})];
        assert_eq!(convert(&user_null, "gpt-4"), user_null);
    }

    #[test]
    fn only_touched_tool_calls_are_rewritten() {
        // A mix: one entry needs a field popped, the other is untouched and must
        // pass through byte-for-byte (including its own nested structure).
        let messages = vec![json!({
            "role": "assistant",
            "tool_calls": [
                {"id": "keep", "function": {"name": "f", "arguments": "{}"}},
                {"id": "strip", "call_id": "rc", "response_item_id": "ri"},
            ],
        })];
        let got = convert(&messages, "gpt-4");
        assert_eq!(
            got[0]["tool_calls"][0],
            json!({"id": "keep", "function": {"name": "f", "arguments": "{}"}}),
        );
        assert_eq!(got[0]["tool_calls"][1], json!({"id": "strip"}));
    }

    #[test]
    fn non_object_tool_call_entries_pass_through() {
        let messages = vec![json!({
            "role": "assistant",
            "tool_calls": ["raw", {"id": "x", "call_id": "rc"}],
        })];
        let got = convert(&messages, "gpt-4");
        assert_eq!(got[0]["tool_calls"][0], json!("raw"));
        assert_eq!(got[0]["tool_calls"][1], json!({"id": "x"}));
    }
}
