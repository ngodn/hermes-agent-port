//! Pure full-compression transcript replacement planner.
//!
//! Plans the replacement transcript for full conversation compression by
//! combining retained head messages, a freshly framed compaction summary carrier
//! (either standalone or merged into the tail), and retained tail messages.
//! Guarantees role alternation, cleans up stale handoff framing, and restores a
//! real user turn anchor or appends a continuation placeholder if no human turn
//! survives.

use crate::compression_prompt::{
    handoff_live_content, is_summary_content, is_synthetic_compression_user_content,
    strip_summary_prefix, COMPRESSION_CONTINUATION_USER_CONTENT, MERGED_PRIOR_CONTEXT_HEADER,
    MERGED_SUMMARY_DELIMITER, SUMMARY_END, SUMMARY_PREFIX,
};
pub(crate) use crate::session_db::CompressionReplacementRow;
use crate::session_db::{encode_message_content, CompressionHistoryMessage, CONTENT_JSON_PREFIX};
use serde_json::Value;

/// Plan the complete replacement transcript for full compression.
///
/// Accepts the full active conversation snapshot, head boundary `prefix_end`,
/// tail boundary `tail_start`, and the summary text. Returns `None` if ranges
/// are invalid or the normalized summary is empty.
pub fn plan_replacement(
    history: &[CompressionHistoryMessage],
    prefix_end: usize,
    tail_start: usize,
    summary: &str,
) -> Option<Vec<CompressionReplacementRow>> {
    if history.is_empty() || prefix_end > tail_start || tail_start > history.len() {
        return None;
    }

    let normalized_summary = strip_summary_prefix(summary);
    let normalized_summary = normalized_summary.trim();
    if normalized_summary.is_empty() {
        return None;
    }

    let mut head_candidates: Vec<RetainedCandidate<'_>> = Vec::new();
    for msg in &history[0..prefix_end] {
        match strip_context_summary_handoff_message(msg) {
            StripResult::Dropped => {}
            StripResult::Unwrapped(content) => {
                head_candidates.push(RetainedCandidate {
                    source: msg,
                    row: CompressionReplacementRow {
                        source_id: Some(msg.id),
                        role: msg.message.role.clone(),
                        content,
                        api_content: None,
                        compressed_summary: false,
                    },
                });
            }
            StripResult::Kept => {
                head_candidates.push(RetainedCandidate {
                    source: msg,
                    row: CompressionReplacementRow {
                        source_id: Some(msg.id),
                        role: msg.message.role.clone(),
                        content: msg.message.content.clone(),
                        api_content: msg.message.api_content.clone(),
                        compressed_summary: false,
                    },
                });
            }
        }
    }

    let mut tail_candidates: Vec<RetainedCandidate<'_>> = Vec::new();
    for msg in &history[tail_start..history.len()] {
        match strip_context_summary_handoff_message(msg) {
            StripResult::Dropped => {}
            StripResult::Unwrapped(content) => {
                tail_candidates.push(RetainedCandidate {
                    source: msg,
                    row: CompressionReplacementRow {
                        source_id: Some(msg.id),
                        role: msg.message.role.clone(),
                        content,
                        api_content: None,
                        compressed_summary: false,
                    },
                });
            }
            StripResult::Kept => {
                tail_candidates.push(RetainedCandidate {
                    source: msg,
                    row: CompressionReplacementRow {
                        source_id: Some(msg.id),
                        role: msg.message.role.clone(),
                        content: msg.message.content.clone(),
                        api_content: msg.message.api_content.clone(),
                        compressed_summary: false,
                    },
                });
            }
        }
    }

    let last_head_role: Option<String> = if head_candidates.is_empty() {
        Some("user".to_string())
    } else {
        head_candidates
            .iter()
            .rev()
            .find_map(|c| template_visible_role(&c.row.role, c.source.tool_calls.as_deref()))
            .map(|r| r.to_string())
    };

    let (first_tail_visible_idx, first_tail_role): (Option<usize>, Option<String>) =
        if tail_candidates.is_empty() {
            (None, None)
        } else {
            tail_candidates
                .iter()
                .enumerate()
                .find_map(|(idx, c)| {
                    template_visible_role(&c.row.role, c.source.tool_calls.as_deref())
                        .map(|r| (idx, r.to_string()))
                })
                .map(|(idx, r)| (Some(idx), Some(r)))
                .unwrap_or((None, None))
        };

    let mut force_user_leading = prefix_end == 0 || last_head_role.as_deref() == Some("system");
    if !force_user_leading {
        let is_nonempty_user = |row: &CompressionReplacementRow| -> bool {
            row.role == "user" && !content_text(&row.content).trim().is_empty()
        };
        let user_survives = head_candidates.iter().any(|c| is_nonempty_user(&c.row))
            || tail_candidates.iter().any(|c| is_nonempty_user(&c.row));
        if !user_survives {
            force_user_leading = true;
        }
    }

    let mut summary_role = if last_head_role.is_none()
        || matches!(last_head_role.as_deref(), Some("assistant") | Some("tool"))
        || force_user_leading
    {
        "user".to_string()
    } else {
        "assistant".to_string()
    };

    let mut merge_into_tail = false;
    if let Some(ref first_tail) = first_tail_role {
        if &summary_role == first_tail {
            let flipped = if summary_role == "user" {
                "assistant"
            } else {
                "user"
            };
            if Some(flipped) != last_head_role.as_deref()
                && last_head_role.is_some()
                && !force_user_leading
            {
                summary_role = flipped.to_string();
            } else {
                merge_into_tail = !tail_candidates.is_empty();
            }
        }
    }

    if merge_into_tail {
        let merge_target_idx = if force_user_leading {
            first_tail_visible_idx.unwrap_or(0)
        } else {
            0
        };
        let target = &mut tail_candidates[merge_target_idx].row;
        let old_content = target.content.clone();
        if force_user_leading && summary_role == "user" {
            let prefix = format!("{SUMMARY_PREFIX}\n{normalized_summary}\n\n{SUMMARY_END}\n\n");
            target.content = append_text_to_content(&old_content, &prefix, true);
        } else {
            let suffix = format!(
                "\n\n{MERGED_SUMMARY_DELIMITER}\n\n{SUMMARY_PREFIX}\n{normalized_summary}\n\n{SUMMARY_END}"
            );
            let intermediate = append_text_to_content(&old_content, &suffix, false);
            let header = format!("{MERGED_PRIOR_CONTEXT_HEADER}\n");
            target.content = append_text_to_content(&intermediate, &header, true);
        }
        target.api_content = None;
        target.compressed_summary = true;
    }

    let mut rows = Vec::with_capacity(head_candidates.len() + tail_candidates.len() + 1);
    for c in head_candidates {
        rows.push(c.row);
    }
    if !merge_into_tail {
        rows.push(CompressionReplacementRow {
            source_id: None,
            role: summary_role,
            content: format!("{SUMMARY_PREFIX}\n{normalized_summary}\n\n{SUMMARY_END}"),
            api_content: None,
            compressed_summary: true,
        });
    }
    for c in tail_candidates {
        rows.push(c.row);
    }

    let replacement_has_real_user = rows.iter().any(|r| {
        is_real_user_message(&r.role, &r.content, r.compressed_summary)
            || (r.role == "tool" && extract_steer_text(&r.content).is_some())
    });

    if !replacement_has_real_user {
        let real_user_anchor = history.iter().rev().find_map(|m| {
            if is_real_user_message(&m.message.role, &m.message.content, m.compressed_summary) {
                Some(m.message.content.clone())
            } else if m.message.role == "tool" {
                extract_steer_text(&m.message.content)
            } else {
                None
            }
        });

        if let Some(anchor_content) = real_user_anchor {
            insert_real_user_anchor(&mut rows, anchor_content);
        } else {
            rows.push(CompressionReplacementRow {
                source_id: None,
                role: "user".to_string(),
                content: COMPRESSION_CONTINUATION_USER_CONTENT.to_string(),
                api_content: None,
                compressed_summary: false,
            });
        }
    }

    Some(rows)
}

/// Return true when issuing another provider request would make a reference
/// handoff, rather than live user input or an in-flight tool exchange, drive
/// the call by itself.
pub fn reference_handoff_would_drive_next_model_call(messages: &[Value]) -> bool {
    if messages.is_empty() {
        return false;
    }
    let mut last_driving_handoff = None;
    for (index, message) in messages.iter().enumerate() {
        let content = stored_value_content(&message["content"]);
        let marked = message["_compressed_summary"].as_bool() == Some(true);
        let kind = crate::compression_prompt::classify_summary_content(&content);
        if !marked && kind.is_none() {
            continue;
        }
        let merged_completed_assistant = message["role"] == "assistant"
            && kind == Some(crate::compression_prompt::SummaryContentKind::Merged)
            && message["finish_reason"] == "stop"
            && !json_truthy(&message["tool_calls"]);
        if handoff_live_content(&content).is_some() && !merged_completed_assistant {
            continue;
        }
        last_driving_handoff = Some(index);
    }
    let Some(index) = last_driving_handoff else {
        return false;
    };
    for message in &messages[index + 1..] {
        let role = message["role"].as_str().unwrap_or_default();
        if role == "tool" || (role == "assistant" && json_truthy(&message["tool_calls"])) {
            return false;
        }
        let content = stored_value_content(&message["content"]);
        if role == "user"
            && actionable_user_content(&message["content"])
            && !is_synthetic_compression_user_content(&content)
        {
            return false;
        }
        if is_summary_content(&content) && handoff_live_content(&content).is_some() {
            return false;
        }
    }
    true
}

fn stored_value_content(content: &Value) -> String {
    encode_message_content(content)
}

fn json_truthy(value: &Value) -> bool {
    match value {
        Value::Null | Value::Bool(false) => false,
        Value::String(text) => !text.is_empty(),
        Value::Array(items) => !items.is_empty(),
        Value::Object(items) => !items.is_empty(),
        Value::Number(number) => number.as_f64() != Some(0.0),
        Value::Bool(true) => true,
    }
}

fn actionable_user_content(content: &Value) -> bool {
    match content {
        Value::Null => false,
        Value::String(text) => !text.trim().is_empty(),
        Value::Array(parts) => parts.iter().any(|part| match part {
            Value::String(text) => !text.trim().is_empty(),
            Value::Object(object)
                if matches!(
                    object.get("type").and_then(Value::as_str),
                    Some("text" | "input_text")
                ) =>
            {
                object
                    .get("text")
                    .and_then(Value::as_str)
                    .is_none_or(|text| !text.trim().is_empty())
            }
            Value::Null => false,
            _ => true,
        }),
        _ => true,
    }
}

struct RetainedCandidate<'a> {
    source: &'a CompressionHistoryMessage,
    row: CompressionReplacementRow,
}

enum StripResult {
    Dropped,
    Unwrapped(String),
    Kept,
}

fn strip_context_summary_handoff_message(msg: &CompressionHistoryMessage) -> StripResult {
    let is_summary = msg.compressed_summary || is_context_summary(&msg.message.content);
    if !is_summary {
        return StripResult::Kept;
    }
    handoff_live_content(&msg.message.content)
        .map(StripResult::Unwrapped)
        .unwrap_or(StripResult::Dropped)
}

fn append_text_to_content(content: &str, text: &str, prepend: bool) -> String {
    if let Some(rest) = content.strip_prefix(CONTENT_JSON_PREFIX) {
        if let Ok(Value::Array(mut parts)) = serde_json::from_str::<Value>(rest) {
            let text_block = serde_json::json!({"type": "text", "text": text});
            if prepend {
                parts.insert(0, text_block);
            } else {
                parts.push(text_block);
            }
            return encode_message_content(&Value::Array(parts));
        }
    }

    if prepend {
        format!("{text}{content}")
    } else {
        format!("{content}{text}")
    }
}

fn content_text(content: &str) -> String {
    if let Some(rest) = content.strip_prefix(CONTENT_JSON_PREFIX) {
        if let Ok(value) = serde_json::from_str::<Value>(rest) {
            match value {
                Value::String(text) => return text,
                Value::Array(parts) => {
                    let mut texts = Vec::new();
                    for part in parts {
                        match part {
                            Value::String(text) => texts.push(text),
                            Value::Object(object) => {
                                if let Some(text) = object
                                    .get("text")
                                    .or_else(|| object.get("content"))
                                    .and_then(Value::as_str)
                                {
                                    texts.push(text.to_owned());
                                }
                            }
                            _ => {}
                        }
                    }
                    return texts.join("\n");
                }
                _ => return String::new(),
            }
        }
    }
    content.to_owned()
}

fn is_context_summary(content: &str) -> bool {
    let text = content_text(content);
    is_summary_content(&text)
}

fn is_real_user_message(role: &str, content: &str, compressed_summary: bool) -> bool {
    if role != "user" || compressed_summary {
        return false;
    }
    let text = content_text(content);
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return false;
    }
    if is_context_summary(content) {
        return false;
    }
    if is_synthetic_compression_user_content(trimmed) {
        return false;
    }
    if is_synthetic_user_text(trimmed) {
        return false;
    }
    true
}

fn is_synthetic_user_text(trimmed: &str) -> bool {
    matches!(
        trimmed,
        "[System: Your previous response contained only internal reasoning and never produced a visible answer or tool call. Do not keep thinking. Produce your final answer as plain text now (or make the tool call you were planning).]"
            | "[System: Continue now. Execute the required tool calls and only send your final answer after completing the task.]"
            | "Your previous turn indicated a tool call but none was included. Do not narrate a plan or restate intent \u{2014} issue the actual tool call now to continue the task."
            | "You just executed tool calls but returned an empty response. Please process the tool results above and continue with the task."
            | "[System: The previous response was cut off by a network error mid-stream. Continue exactly where you left off. Do not restart or repeat prior text. Finish the answer directly.]"
            | "[System: Your previous response was truncated by the output length limit. Continue exactly where you left off. Do not restart or repeat prior text. Finish the answer directly.]"
    ) || trimmed.starts_with("[IMPORTANT: Background process ")
        || trimmed.starts_with("[Your active task list was preserved across context compression]")
        || trimmed.starts_with("[System: Your previous tool call")
        || trimmed.starts_with("[System: Your previous response was truncated")
        || trimmed.starts_with("[System: The previous response was cut off")
        || trimmed.starts_with("[System: Your previous turn requested tool calls that were dropped")
}

fn extract_steer_text(content: &str) -> Option<String> {
    let text = content_text(content);
    let open_marker = "[OUT-OF-BAND USER MESSAGE";
    let close_marker = "[/OUT-OF-BAND USER MESSAGE]";
    let start_idx = text.find(open_marker)?;
    let after_open = &text[start_idx..];
    let start = if let Some(nl) = after_open.find('\n') {
        start_idx + nl + 1
    } else {
        start_idx + open_marker.len()
    };
    if start >= text.len() {
        return None;
    }
    let close_idx = text[start..].find(close_marker)?;
    let extracted = text[start..start + close_idx].trim();
    if extracted.is_empty() {
        None
    } else {
        Some(extracted.to_string())
    }
}

fn template_visible_role<'a>(role: &'a str, tool_calls: Option<&str>) -> Option<&'a str> {
    if role == "tool" {
        return None;
    }
    if role == "assistant" {
        if let Some(raw) = tool_calls {
            let trimmed = raw.trim();
            if !trimmed.is_empty() {
                match serde_json::from_str::<Value>(trimmed) {
                    Ok(Value::Array(items)) if !items.is_empty() => return None,
                    Ok(Value::Array(_)) => {}
                    _ => return None,
                }
            }
        }
    }
    Some(role)
}

fn insert_real_user_anchor(rows: &mut Vec<CompressionReplacementRow>, anchor_content: String) {
    for index in 0..rows.len() {
        if rows[index].role == "assistant" {
            let previous_role = if index > 0 {
                Some(rows[index - 1].role.as_str())
            } else {
                None
            };
            if previous_role != Some("user") {
                rows.insert(
                    index,
                    CompressionReplacementRow {
                        source_id: None,
                        role: "user".to_string(),
                        content: anchor_content,
                        api_content: None,
                        compressed_summary: false,
                    },
                );
                return;
            }
        }
    }

    if rows.is_empty() || rows.last().map(|r| r.role.as_str()) != Some("user") {
        rows.push(CompressionReplacementRow {
            source_id: None,
            role: "user".to_string(),
            content: anchor_content,
            api_content: None,
            compressed_summary: false,
        });
        return;
    }

    let last = rows.last_mut().unwrap();
    if last.compressed_summary || is_context_summary(&last.content) {
        rows.push(CompressionReplacementRow {
            source_id: None,
            role: "user".to_string(),
            content: anchor_content,
            api_content: None,
            compressed_summary: false,
        });
        return;
    }

    if let Some(rest) = last.content.strip_prefix(CONTENT_JSON_PREFIX) {
        if let Ok(Value::Array(mut target_parts)) = serde_json::from_str::<Value>(rest) {
            let mut anchor_parts =
                if let Some(a_rest) = anchor_content.strip_prefix(CONTENT_JSON_PREFIX) {
                    if let Ok(Value::Array(a_parts)) = serde_json::from_str::<Value>(a_rest) {
                        a_parts
                    } else {
                        vec![serde_json::json!({"type": "text", "text": anchor_content})]
                    }
                } else {
                    vec![serde_json::json!({"type": "text", "text": anchor_content})]
                };
            anchor_parts.append(&mut target_parts);
            last.content = encode_message_content(&Value::Array(anchor_parts));
            last.api_content = None;
            return;
        }
    }

    last.content = format!("{anchor_content}\n\n{}", last.content)
        .trim()
        .to_string();
    last.api_content = None;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session_db::HistoryMessage;

    fn make_message(
        id: i64,
        role: &str,
        content: &str,
        tool_calls: Option<&str>,
        compressed_summary: bool,
    ) -> CompressionHistoryMessage {
        CompressionHistoryMessage {
            id,
            message: HistoryMessage {
                role: role.to_string(),
                content: content.to_string(),
                api_content: None,
            },
            tool_call_id: None,
            tool_calls: tool_calls.map(str::to_string),
            tool_name: None,
            effect_disposition: None,
            finish_reason: None,
            reasoning: None,
            reasoning_content: None,
            reasoning_details: None,
            codex_reasoning_items: None,
            codex_message_items: None,
            display_kind: None,
            display_metadata: None,
            timestamp: 0.0,
            compressed_summary,
        }
    }

    fn standalone_summary_text(body: &str) -> String {
        format!("{SUMMARY_PREFIX}\n{body}\n\n{SUMMARY_END}")
    }

    fn merged_carrier_text(prior: &str, body: &str) -> String {
        format!(
            "{MERGED_PRIOR_CONTEXT_HEADER}\n{prior}\n\n{MERGED_SUMMARY_DELIMITER}\n\n{SUMMARY_PREFIX}\n{body}\n\n{SUMMARY_END}"
        )
    }

    fn forced_user_leading_carrier_text(ask: &str, body: &str) -> String {
        format!("{SUMMARY_PREFIX}\n{body}\n\n{SUMMARY_END}\n\n{ask}")
    }

    #[test]
    fn test_merged_prior_header_parity() {
        let goldens_str = include_str!("../../../tools/compression-handoff-goldens.json");
        let goldens: Value = serde_json::from_str(goldens_str).expect("parse goldens json");
        let constants = goldens["constants"].as_array().expect("constants array");
        let entry = constants
            .iter()
            .find(|c| c["name"] == "MERGED_PRIOR_CONTEXT_HEADER")
            .expect("find MERGED_PRIOR_CONTEXT_HEADER");
        assert_eq!(
            MERGED_PRIOR_CONTEXT_HEADER,
            entry["value"].as_str().expect("string value")
        );
    }

    #[test]
    fn test_invalid_ranges_and_empty_summary() {
        let history = vec![make_message(1, "user", "hello", None, false)];
        assert!(plan_replacement(&history, 1, 0, "summary body").is_none());
        assert!(plan_replacement(&history, 0, 2, "summary body").is_none());
        assert!(plan_replacement(&history, 0, 1, "").is_none());
        assert!(plan_replacement(&history, 0, 1, "   ").is_none());
        assert!(plan_replacement(&[], 0, 0, "summary body").is_none());
    }

    #[test]
    fn test_role_selection_head_user_tail_assistant_collision_merges() {
        let history = vec![
            make_message(1, "system", "sys prompt", None, false),
            make_message(2, "user", "user turn", None, false),
            make_message(3, "user", "middle turn to drop", None, false),
            make_message(4, "assistant", "tail assistant", None, false),
        ];
        let plan = plan_replacement(&history, 2, 3, "checkpoint body").expect("plan succeeds");
        assert_eq!(plan.len(), 3);
        assert_eq!(plan[0].source_id, Some(1));
        assert_eq!(plan[0].role, "system");
        assert_eq!(plan[1].source_id, Some(2));
        assert_eq!(plan[1].role, "user");
        assert_eq!(plan[2].source_id, Some(4));
        assert_eq!(plan[2].role, "assistant");
        assert!(plan[2].compressed_summary);
        assert!(plan[2].content.contains(MERGED_PRIOR_CONTEXT_HEADER));
        assert!(plan[2].content.contains(MERGED_SUMMARY_DELIMITER));
        assert!(plan[2].content.contains("tail assistant"));
        assert!(plan[2].content.contains("checkpoint body"));
        assert!(plan[2].content.contains(SUMMARY_END));
        assert_eq!(plan.iter().filter(|r| r.compressed_summary).count(), 1);
    }

    #[test]
    fn test_role_selection_head_assistant_tail_user_collision_merges() {
        let history = vec![
            make_message(1, "system", "sys prompt", None, false),
            make_message(2, "user", "user turn", None, false),
            make_message(3, "assistant", "head assistant", None, false),
            make_message(4, "user", "middle turn to drop", None, false),
            make_message(5, "user", "tail user ask", None, false),
        ];
        let plan = plan_replacement(&history, 3, 4, "checkpoint body").expect("plan succeeds");
        assert_eq!(plan.len(), 4);
        assert_eq!(plan[0].source_id, Some(1));
        assert_eq!(plan[1].source_id, Some(2));
        assert_eq!(plan[2].source_id, Some(3));
        assert_eq!(plan[3].source_id, Some(5));
        assert_eq!(plan[3].role, "user");
        assert!(plan[3].compressed_summary);
        assert!(plan[3].content.contains(MERGED_PRIOR_CONTEXT_HEADER));
        assert!(plan[3].content.contains(MERGED_SUMMARY_DELIMITER));
        assert!(plan[3].content.contains("tail user ask"));
        assert_eq!(plan.iter().filter(|r| r.compressed_summary).count(), 1);
    }

    #[test]
    fn test_role_selection_compress_start_zero_forces_user_leading() {
        let history = vec![
            make_message(1, "user", "dropped user", None, false),
            make_message(2, "assistant", "tail assistant", None, false),
        ];
        let plan = plan_replacement(&history, 0, 1, "checkpoint body").expect("plan succeeds");
        assert_eq!(plan[0].source_id, None);
        assert_eq!(plan[0].role, "user");
        assert!(plan[0].compressed_summary);
        assert_eq!(plan[1].source_id, Some(2));
        assert_eq!(plan[1].role, "assistant");
        assert!(!plan[1].compressed_summary);
        assert_eq!(plan[2].source_id, None);
        assert_eq!(plan[2].role, "user");
        assert_eq!(plan[2].content, "dropped user");
        assert_eq!(plan.iter().filter(|r| r.compressed_summary).count(), 1);
    }

    #[test]
    fn test_role_selection_system_only_head_forces_user_leading() {
        let history = vec![
            make_message(1, "system", "sys prompt", None, false),
            make_message(2, "user", "dropped user", None, false),
            make_message(3, "assistant", "tail assistant", None, false),
        ];
        let plan = plan_replacement(&history, 1, 2, "checkpoint body").expect("plan succeeds");
        assert_eq!(plan[0].source_id, Some(1));
        assert_eq!(plan[0].role, "system");
        assert_eq!(plan[1].source_id, None);
        assert_eq!(plan[1].role, "user");
        assert!(plan[1].compressed_summary);
        assert_eq!(plan[2].source_id, Some(3));
        assert_eq!(plan[2].role, "assistant");
        assert_eq!(plan[3].source_id, None);
        assert_eq!(plan[3].role, "user");
        assert_eq!(plan[3].content, "dropped user");
        assert_eq!(plan.iter().filter(|r| r.compressed_summary).count(), 1);
    }

    #[test]
    fn test_role_selection_tool_flow_head_visible_role_is_user() {
        let tool_call_json =
            r#"[{"id":"c1","type":"function","function":{"name":"run","arguments":"{}"}}]"#;
        let history = vec![
            make_message(1, "system", "sys prompt", None, false),
            make_message(2, "user", "head user", None, false),
            make_message(3, "assistant", "", Some(tool_call_json), false),
            make_message(4, "tool", "tool result", None, false),
            make_message(5, "user", "middle dropped", None, false),
            make_message(6, "user", "tail user", None, false),
        ];
        let plan = plan_replacement(&history, 4, 5, "checkpoint body").expect("plan succeeds");
        assert_eq!(plan[0].source_id, Some(1));
        assert_eq!(plan[1].source_id, Some(2));
        assert_eq!(plan[2].source_id, Some(3));
        assert_eq!(plan[3].source_id, Some(4));
        assert_eq!(plan[4].source_id, None);
        assert_eq!(plan[4].role, "assistant");
        assert!(plan[4].compressed_summary);
        assert_eq!(plan[5].source_id, Some(6));
        assert_eq!(plan[5].role, "user");
        assert_eq!(plan.iter().filter(|r| r.compressed_summary).count(), 1);
    }

    #[test]
    fn test_role_selection_all_exempt_head_opens_with_user() {
        let tool_call_json =
            r#"[{"id":"c1","type":"function","function":{"name":"run","arguments":"{}"}}]"#;
        let history = vec![
            make_message(1, "assistant", "", Some(tool_call_json), false),
            make_message(2, "tool", "tool result", None, false),
            make_message(3, "user", "dropped user", None, false),
            make_message(4, "assistant", "tail assistant", None, false),
        ];
        let plan = plan_replacement(&history, 2, 3, "checkpoint body").expect("plan succeeds");
        // Because head has no preceding user, anchor is inserted before head assistant
        assert_eq!(plan[0].source_id, None);
        assert_eq!(plan[0].role, "user");
        assert_eq!(plan[0].content, "dropped user");
        assert_eq!(plan[1].source_id, Some(1));
        assert_eq!(plan[2].source_id, Some(2));
        assert_eq!(plan[3].source_id, None);
        assert_eq!(plan[3].role, "user");
        assert!(plan[3].compressed_summary);
        assert_eq!(plan[4].source_id, Some(4));
        assert_eq!(plan[4].role, "assistant");
        assert_eq!(plan.iter().filter(|r| r.compressed_summary).count(), 1);
    }

    #[test]
    fn test_role_selection_clean_alternation_summary_assistant() {
        let tool_call_json =
            r#"[{"id":"c1","type":"function","function":{"name":"run","arguments":"{}"}}]"#;
        let history = vec![
            make_message(1, "system", "sys prompt", None, false),
            make_message(2, "user", "head user", None, false),
            make_message(3, "user", "middle dropped", None, false),
            make_message(4, "assistant", "", Some(tool_call_json), false),
            make_message(5, "tool", "tool result", None, false),
            make_message(6, "user", "tail user", None, false),
        ];
        let plan = plan_replacement(&history, 2, 3, "checkpoint body").expect("plan succeeds");
        assert_eq!(plan[0].source_id, Some(1));
        assert_eq!(plan[1].source_id, Some(2));
        assert_eq!(plan[2].source_id, None);
        assert_eq!(plan[2].role, "assistant");
        assert!(plan[2].compressed_summary);
        assert_eq!(plan[3].source_id, Some(4));
        assert_eq!(plan[4].source_id, Some(5));
        assert_eq!(plan[5].source_id, Some(6));
        assert_eq!(plan.iter().filter(|r| r.compressed_summary).count(), 1);
    }

    #[test]
    fn test_role_selection_empty_tail_head_user_standalone_assistant() {
        let history = vec![
            make_message(1, "system", "sys prompt", None, false),
            make_message(2, "user", "head user", None, false),
            make_message(3, "assistant", "middle dropped", None, false),
        ];
        let plan = plan_replacement(&history, 2, 3, "checkpoint body").expect("plan succeeds");
        assert_eq!(plan.len(), 3);
        assert_eq!(plan[0].source_id, Some(1));
        assert_eq!(plan[1].source_id, Some(2));
        assert_eq!(plan[2].source_id, None);
        assert_eq!(plan[2].role, "assistant");
        assert!(plan[2].compressed_summary);
        assert_eq!(plan.iter().filter(|r| r.compressed_summary).count(), 1);
    }

    #[test]
    fn test_continuation_zero_real_user_appends_placeholder() {
        let history = vec![
            make_message(1, "system", "sys prompt", None, false),
            make_message(2, "assistant", "first assistant", None, false),
            make_message(3, "assistant", "tail assistant", None, false),
        ];
        let plan = plan_replacement(&history, 1, 2, "checkpoint body").expect("plan succeeds");
        let last = plan.last().expect("non-empty plan");
        assert_eq!(last.role, "user");
        assert_eq!(last.content, COMPRESSION_CONTINUATION_USER_CONTENT);
        assert_eq!(last.source_id, None);
        assert!(!last.compressed_summary);
        assert_eq!(plan.iter().filter(|r| r.compressed_summary).count(), 1);
    }

    #[test]
    fn test_continuation_real_user_present_no_insertion() {
        let history = vec![
            make_message(1, "system", "sys prompt", None, false),
            make_message(2, "user", "real ask survives", None, false),
            make_message(3, "user", "middle dropped", None, false),
            make_message(4, "assistant", "tail reply", None, false),
        ];
        let plan = plan_replacement(&history, 2, 3, "checkpoint body").expect("plan succeeds");
        assert!(plan
            .iter()
            .any(|r| r.role == "user" && r.content == "real ask survives"));
        assert!(!plan
            .iter()
            .any(|r| r.content == COMPRESSION_CONTINUATION_USER_CONTENT));
    }

    #[test]
    fn test_continuation_synthetic_only_anchors_real_user_from_originals() {
        let handoff_content = standalone_summary_text("old summary");
        let history = vec![
            make_message(1, "system", "sys prompt", None, false),
            make_message(2, "user", &handoff_content, None, true),
            make_message(3, "user", "original real ask", None, false),
            make_message(4, "assistant", "tail assistant", None, false),
        ];
        let plan = plan_replacement(&history, 1, 3, "fresh summary body").expect("plan succeeds");
        assert_eq!(plan[0].source_id, Some(1));
        assert_eq!(plan[1].source_id, None);
        assert_eq!(plan[1].role, "user");
        assert!(plan[1].compressed_summary);
        assert_eq!(plan[2].source_id, Some(4));
        assert_eq!(plan[2].role, "assistant");
        assert_eq!(plan[3].source_id, None);
        assert_eq!(plan[3].role, "user");
        assert_eq!(plan[3].content, "original real ask");
        assert_eq!(plan.iter().filter(|r| r.compressed_summary).count(), 1);
    }

    #[test]
    fn test_continuation_anchor_appended_after_trailing_summary() {
        let history = vec![
            make_message(1, "system", "sys prompt", None, false),
            make_message(2, "user", "the ask to re-anchor", None, false),
            make_message(3, "assistant", "dropped assistant", None, false),
        ];
        let plan = plan_replacement(&history, 1, 3, "fresh summary").expect("plan succeeds");
        assert_eq!(plan[0].source_id, Some(1));
        assert_eq!(plan[0].role, "system");
        assert_eq!(plan[1].source_id, None);
        assert_eq!(plan[1].role, "user");
        assert!(plan[1].compressed_summary);
        assert_eq!(plan[2].source_id, None);
        assert_eq!(plan[2].role, "user");
        assert_eq!(plan[2].content, "the ask to re-anchor");
        assert!(!plan[2].compressed_summary);
        assert_eq!(plan.iter().filter(|r| r.compressed_summary).count(), 1);
    }

    #[test]
    fn test_strip_unwrap_standalone_handoff_dropped() {
        let handoff_content = standalone_summary_text("old standalone handoff");
        let history = vec![
            make_message(1, "system", "sys prompt", None, false),
            make_message(2, "user", &handoff_content, None, true),
            make_message(3, "user", "middle user", None, false),
            make_message(4, "assistant", "tail reply", None, false),
        ];
        let plan = plan_replacement(&history, 2, 3, "new summary").expect("plan succeeds");
        assert!(!plan.iter().any(|r| r.source_id == Some(2)));
    }

    #[test]
    fn test_strip_unwrap_merged_carrier_unwrapped_to_prior_content() {
        let merged_content = merged_carrier_text("surviving live tail ask", "old summary");
        let history = vec![
            make_message(1, "system", "sys prompt", None, false),
            make_message(2, "user", &merged_content, None, true),
            make_message(3, "user", "middle user", None, false),
            make_message(4, "assistant", "tail assistant", None, false),
        ];
        let plan = plan_replacement(&history, 2, 3, "new summary").expect("plan succeeds");
        let unwrapped = plan
            .iter()
            .find(|r| r.source_id == Some(2))
            .expect("source 2 retained");
        assert_eq!(unwrapped.content, "surviving live tail ask");
        assert_eq!(unwrapped.api_content, None);
        assert!(!unwrapped.compressed_summary);
    }

    #[test]
    fn test_strip_unwrap_force_user_leading_carrier_keeps_live_ask() {
        let carrier = forced_user_leading_carrier_text("finish the port now", "old summary");
        let history = vec![
            make_message(1, "system", "sys prompt", None, false),
            make_message(2, "user", &carrier, None, true),
            make_message(3, "user", "middle user", None, false),
            make_message(4, "assistant", "tail assistant", None, false),
        ];
        let plan = plan_replacement(&history, 2, 3, "new summary").expect("plan succeeds");
        let unwrapped = plan
            .iter()
            .find(|r| r.source_id == Some(2))
            .expect("source 2 retained");
        assert_eq!(unwrapped.content, "finish the port now");
        assert_eq!(unwrapped.api_content, None);
        assert!(!unwrapped.compressed_summary);
    }

    #[test]
    fn test_strip_unwrap_list_content_merged_carrier_unwrapped() {
        let raw_prior = format!(
            "{MERGED_PRIOR_CONTEXT_HEADER}\nprior tail text\n\n{MERGED_SUMMARY_DELIMITER}\n\n{SUMMARY_PREFIX}\nbody\n\n{SUMMARY_END}"
        );
        let list_json = serde_json::json!([raw_prior]);
        let encoded_content = format!(
            "{CONTENT_JSON_PREFIX}{}",
            serde_json::to_string(&list_json).unwrap()
        );
        let history = vec![
            make_message(1, "system", "sys prompt", None, false),
            make_message(2, "user", &encoded_content, None, true),
            make_message(3, "user", "middle user", None, false),
            make_message(4, "assistant", "tail reply", None, false),
        ];
        let plan = plan_replacement(&history, 2, 3, "new summary").expect("plan succeeds");
        let unwrapped = plan
            .iter()
            .find(|r| r.source_id == Some(2))
            .expect("source 2 retained");
        let expected_json = serde_json::json!(["prior tail text\n\n"]);
        let expected_encoded = format!(
            "{CONTENT_JSON_PREFIX}{}",
            serde_json::to_string(&expected_json).unwrap()
        );
        assert_eq!(unwrapped.content, expected_encoded);
        assert_eq!(unwrapped.api_content, None);
        assert!(!unwrapped.compressed_summary);
    }

    #[test]
    fn test_strip_unwrap_non_summary_message_preserved_unchanged() {
        let history = vec![
            make_message(1, "system", "sys prompt", None, false),
            make_message(2, "user", "plain human message", None, false),
            make_message(3, "user", "middle user", None, false),
            make_message(4, "assistant", "tail reply", None, false),
        ];
        let plan = plan_replacement(&history, 2, 3, "new summary").expect("plan succeeds");
        let preserved = plan
            .iter()
            .find(|r| r.source_id == Some(2))
            .expect("source 2 retained");
        assert_eq!(preserved.content, "plain human message");
        assert_eq!(preserved.role, "user");
        assert!(!preserved.compressed_summary);
    }

    #[test]
    fn test_reference_only_call_suppression_matches_python_oracle() {
        let summary = standalone_summary_text("completed history");
        let merged_user = merged_carrier_text("live ask", "completed history");
        let forced_user = forced_user_leading_carrier_text("live ask", "completed history");
        let merged_assistant = merged_carrier_text("completed answer", "completed history");
        let cases = [
            (
                "sole_standalone_handoff_drives",
                vec![
                    serde_json::json!({"role":"user","content":summary,"_compressed_summary":true}),
                ],
                true,
            ),
            (
                "trailing_real_user_does_not_drive",
                vec![
                    serde_json::json!({"role":"user","content":standalone_summary_text("done"),"_compressed_summary":true}),
                    serde_json::json!({"role":"user","content":"new task"}),
                ],
                false,
            ),
            (
                "trailing_tool_result_does_not_drive",
                vec![
                    serde_json::json!({"role":"user","content":standalone_summary_text("done"),"_compressed_summary":true}),
                    serde_json::json!({"role":"tool","content":"result","tool_call_id":"call-1"}),
                ],
                false,
            ),
            (
                "pending_assistant_tool_calls_do_not_drive",
                vec![
                    serde_json::json!({"role":"user","content":standalone_summary_text("done"),"_compressed_summary":true}),
                    serde_json::json!({"role":"assistant","content":"","tool_calls":[{"id":"call-1"}]}),
                ],
                false,
            ),
            (
                "trailing_continuation_placeholder_still_drives",
                vec![
                    serde_json::json!({"role":"user","content":standalone_summary_text("done"),"_compressed_summary":true}),
                    serde_json::json!({"role":"user","content":COMPRESSION_CONTINUATION_USER_CONTENT}),
                ],
                true,
            ),
            (
                "merged_carrier_with_live_ask_does_not_drive",
                vec![
                    serde_json::json!({"role":"user","content":merged_user,"_compressed_summary":true}),
                ],
                false,
            ),
            (
                "force_user_leading_carrier_with_live_ask_does_not_drive",
                vec![
                    serde_json::json!({"role":"user","content":forced_user,"_compressed_summary":true}),
                ],
                false,
            ),
            (
                "merged_completed_assistant_carrier_drives",
                vec![
                    serde_json::json!({"role":"assistant","content":merged_assistant,"finish_reason":"stop","_compressed_summary":true}),
                ],
                true,
            ),
            (
                "merged_assistant_carrier_with_pending_calls_does_not_drive",
                vec![
                    serde_json::json!({"role":"assistant","content":merged_carrier_text("working", "done"),"tool_calls":[{"id":"call-1"}],"_compressed_summary":true}),
                ],
                false,
            ),
            ("empty_transcript_does_not_drive", vec![], false),
            (
                "no_handoff_does_not_drive",
                vec![serde_json::json!({"role":"user","content":"plain task"})],
                false,
            ),
        ];
        let oracle: Value = serde_json::from_str(include_str!(
            "../../../tools/compression-handoff-goldens.json"
        ))
        .unwrap();
        assert_eq!(
            cases.len(),
            oracle["reference_handoff"].as_array().unwrap().len()
        );
        for (name, messages, expected) in cases {
            let oracle_case = oracle["reference_handoff"]
                .as_array()
                .unwrap()
                .iter()
                .find(|row| row["case"] == name)
                .unwrap();
            assert_eq!(oracle_case["would_drive_next_model_call"], expected);
            assert_eq!(
                reference_handoff_would_drive_next_model_call(&messages),
                expected,
                "{name}"
            );
        }
    }

    #[test]
    fn test_carrier_exact_bytes() {
        let history = vec![
            make_message(1, "system", "sys prompt", None, false),
            make_message(2, "user", "user ask", None, false),
            make_message(3, "assistant", "middle work", None, false),
        ];
        let plan = plan_replacement(&history, 2, 3, "exact checkpoint body").expect("plan");
        let summary_row = plan
            .iter()
            .find(|r| r.compressed_summary)
            .expect("summary row");
        let expected_bytes = format!("{SUMMARY_PREFIX}\nexact checkpoint body\n\n{SUMMARY_END}");
        assert_eq!(summary_row.content, expected_bytes);
    }
}
