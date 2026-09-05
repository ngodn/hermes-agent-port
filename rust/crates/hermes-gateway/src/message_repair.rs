//! Provider-bound thinking-only removal and adjacent-user repair.
//!
//! These transformations operate on a copy. Stored reasoning and clean user
//! messages remain intact for transcripts and future provider selection.
use crate::python_value::{python_whitespace, truthy};
use serde_json::{json, Value};

fn has_visible_text(value: &Value) -> bool {
    value
        .as_str()
        .is_some_and(|text| !text.trim_matches(python_whitespace).is_empty())
}

/// Structural payloads count even when visible text is empty. In particular,
/// Codex carriers must not acquire fabricated text before transport projection.
fn has_payload(message: &Value) -> bool {
    match &message["content"] {
        Value::String(_) if has_visible_text(&message["content"]) => return true,
        Value::Array(blocks) => {
            for block in blocks {
                if block.is_object() {
                    if block["type"] != "text" || has_visible_text(&block["text"]) {
                        return true;
                    }
                } else if truthy(block) {
                    return true;
                }
            }
        }
        Value::Null | Value::String(_) => {}
        _ => return true,
    }
    truthy(&message["tool_calls"])
        || has_visible_text(&message["reasoning_content"])
        || [
            "reasoning",
            "reasoning_details",
            "codex_message_items",
            "codex_reasoning_items",
        ]
        .iter()
        .any(|field| truthy(&message[*field]))
}

/// Heal empty historical user/assistant turns on the outgoing copy. The final
/// message is deliberately untouched, as are tool results and other roles.
/// This precedes thinking-only removal and positional tool-call repair.
pub fn heal_empty_non_final(messages: &mut [Value]) {
    let last = messages.len().saturating_sub(1);
    for message in &mut messages[..last] {
        if matches!(message["role"].as_str(), Some("user" | "assistant")) && !has_payload(message) {
            message["content"] = Value::String("[response interrupted]".into());
        }
    }
}

/// Port of AIAgent._is_thinking_only_assistant. Compaction carriers protect
/// already-pruned history and must survive even when they also carry reasoning.
fn thinking_only(message: &Value, drop_codex_reasoning_items: bool) -> bool {
    if message["role"] != "assistant" || truthy(&message["tool_calls"]) {
        return false;
    }
    if truthy(&message["_thinking_prefill"]) {
        return true;
    }
    match &message["content"] {
        Value::String(_) if has_visible_text(&message["content"]) => return false,
        Value::Array(blocks) => {
            for block in blocks {
                if !block.is_object() {
                    if truthy(block) {
                        return false;
                    }
                    continue;
                }
                match block["type"].as_str() {
                    Some("thinking" | "redacted_thinking") => {}
                    Some("text") => {
                        if has_visible_text(&block["text"]) {
                            return false;
                        }
                    }
                    _ => return false,
                }
            }
        }
        Value::Null | Value::String(_) => {}
        _ => return false,
    }
    let codex = message["codex_reasoning_items"].as_array();
    if codex.is_some_and(|items| items.iter().any(|item| item["type"] == "compaction")) {
        return false;
    }
    let reasoning = if truthy(&message["reasoning_content"]) {
        &message["reasoning_content"]
    } else {
        &message["reasoning"]
    };
    if has_visible_text(reasoning)
        || message["reasoning_details"]
            .as_array()
            .is_some_and(|items| !items.is_empty())
    {
        return true;
    }
    drop_codex_reasoning_items
        && codex.is_some_and(|items| items.iter().any(|item| item["type"] == "reasoning"))
}

/// Drop thinking-only assistant messages, then merge adjacent user content.
/// Unknown content shapes remain separate, matching the reference's fallback.
/// The caller must substitute api_content before this pass and strip internal
/// metadata afterwards, so a stale sidecar cannot undo the merged content.
pub fn repair(messages: &[Value], drop_codex_reasoning_items: bool) -> Vec<Value> {
    let mut merged: Vec<Value> = Vec::with_capacity(messages.len());
    for message in messages {
        if thinking_only(message, drop_codex_reasoning_items) {
            continue;
        }
        if let Some(previous) = merged.last_mut() {
            if previous["role"] == "user" && message["role"] == "user" {
                // Missing content defaults to empty text; explicit null does not.
                let empty = Value::String(String::new());
                let left = previous.get("content").unwrap_or(&empty);
                let right = message.get("content").unwrap_or(&empty);
                let content = match (left, right) {
                    (Value::String(left), Value::String(right)) => {
                        let separator = if !left.is_empty() && !right.is_empty() {
                            "\n\n"
                        } else {
                            ""
                        };
                        Some(Value::String(format!("{left}{separator}{right}")))
                    }
                    (Value::Array(left), Value::Array(right)) => {
                        Some(Value::Array(left.iter().chain(right).cloned().collect()))
                    }
                    (Value::Array(left), Value::String(right)) => {
                        let mut blocks = left.clone();
                        if !right.is_empty() {
                            blocks.push(json!({"type":"text","text":right}));
                        }
                        Some(Value::Array(blocks))
                    }
                    (Value::String(left), Value::Array(right)) => {
                        let mut blocks = Vec::new();
                        if !left.is_empty() {
                            blocks.push(json!({"type":"text","text":left}));
                        }
                        blocks.extend(right.iter().cloned());
                        Some(Value::Array(blocks))
                    }
                    _ => None,
                };
                if let Some(content) = content {
                    previous["content"] = content;
                    continue;
                }
            }
        }
        merged.push(message.clone());
    }
    merged
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_message_healing_matches_python() {
        let rows: Value =
            serde_json::from_str(include_str!("../../../tools/empty-message-goldens.json"))
                .unwrap();
        for row in rows.as_array().unwrap() {
            let mut messages = row["messages"].as_array().unwrap().clone();
            heal_empty_non_final(&mut messages);
            assert_eq!(messages, *row["expected"].as_array().unwrap(), "{row}");
        }
    }

    #[test]
    fn python_reference_repair_cases() {
        let rows: Value =
            serde_json::from_str(include_str!("../../../tools/thinking-repair-goldens.json"))
                .unwrap();
        for row in rows.as_array().unwrap() {
            let messages = row["messages"].as_array().unwrap();
            let original = messages.clone();
            assert_eq!(
                repair(
                    messages,
                    row["drop_codex_reasoning_items"].as_bool().unwrap()
                ),
                *row["expected"].as_array().unwrap(),
                "{row}"
            );
            assert_eq!(*messages, original);
        }
    }

    #[test]
    fn thinking_carriers_respect_output_and_checkpoint_precedence() {
        assert!(!thinking_only(
            &json!({"role":"assistant","content":false,"reasoning":"thought"}),
            true
        ));
        assert!(!thinking_only(
            &json!({"role":"assistant","reasoning_content":" ","reasoning":"thought"}),
            true
        ));
        assert!(thinking_only(
            &json!({"role":"assistant","reasoning_details":[null]}),
            true
        ));
        assert!(!thinking_only(
            &json!({"role":"assistant","reasoning":"thought","codex_reasoning_items":[{"type":"compaction"}]}),
            true
        ));
        assert!(!thinking_only(
            &json!({"role":"assistant","_thinking_prefill":true,"tool_calls":[{}]}),
            true
        ));
    }
}
