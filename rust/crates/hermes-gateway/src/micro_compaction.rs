//! Opt-in rolling post-turn compaction for one durable conversation.

use serde_json::{json, Value};

use crate::session_db::CompressionHistoryMessage;

const MAX_CONSECUTIVE_FAILURES: u32 = 3;
const HISTORICAL_TASK_HEADING: &str = "## Historical Task Snapshot";

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct MicroCompactionState {
    pub rolling_summary: String,
    pub cursor: usize,
    pub consecutive_failures: u32,
    pub last_failure_cursor: Option<usize>,
    pub turns_since_pass: usize,
    supersedable_marker_id: Option<i64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum MicroCompactionWork {
    Summarize {
        start: usize,
        end: usize,
        existing_summary: String,
        exchange_text: String,
        supersede_marker_id: Option<i64>,
    },
    Defrag {
        marker_index: usize,
        old_summary: String,
        cursor_after_commit: usize,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct MicroCompactionRow {
    pub source_id: Option<i64>,
    pub retained_ids: Vec<i64>,
    pub role: String,
    pub content: String,
    pub api_content: Option<String>,
    pub compressed_summary: bool,
    plain_text: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct MicroCompactionPublication {
    pub rows: Vec<MicroCompactionRow>,
    pub rolling_summary: String,
    pub cursor_after_commit: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct MicroCompactionConfig {
    pub protect_head: usize,
    pub protect_last: usize,
    pub tail_token_budget: u64,
    pub charge_all_thinking: bool,
    pub every_n_turns: usize,
    pub defrag_threshold_tokens: u64,
}

#[cfg(test)]
impl MicroCompactionRow {
    pub(crate) fn from_source_for_test(message: &CompressionHistoryMessage) -> Self {
        row_from_source(message)
    }

    pub(crate) fn summary_for_test(summary: &str) -> Self {
        Self {
            source_id: None,
            retained_ids: Vec::new(),
            role: "assistant".into(),
            content: render_marker(summary),
            api_content: None,
            compressed_summary: true,
            plain_text: true,
        }
    }
}

pub(crate) fn prepare(
    messages: &[CompressionHistoryMessage],
    config: MicroCompactionConfig,
    state: &mut MicroCompactionState,
) -> Option<MicroCompactionWork> {
    let every_n_turns = config.every_n_turns.max(1);
    if every_n_turns > 1 {
        state.turns_since_pass = state.turns_since_pass.saturating_add(1);
        if state.turns_since_pass < every_n_turns {
            return None;
        }
        state.turns_since_pass = 0;
    }
    if messages.len() < 4 {
        return None;
    }

    let head_end = align_boundary_forward(messages, config.protect_head.min(messages.len()));
    let tail_start = crate::tool_result_prune::find_tail_cut_by_tokens(
        messages,
        head_end,
        config.protect_last,
        config.tail_token_budget,
        config.charge_all_thinking,
    );
    if head_end >= tail_start {
        return None;
    }

    let marker = (head_end..tail_start)
        .rev()
        .find(|index| is_context_summary(&messages[*index]));
    let marker_id = marker.map(|index| messages[index].id);
    let cursor_is_valid = state.cursor > head_end && state.cursor < tail_start;
    if cursor_is_valid {
        if marker_id != state.supersedable_marker_id {
            state.supersedable_marker_id = marker.and_then(|index| {
                marker_contains_rolling_summary(&messages[index], &state.rolling_summary)
                    .then_some(messages[index].id)
            });
        }
    } else {
        if let Some(marker_index) = marker {
            let recovered = rolling_summary_from_marker(&messages[marker_index].message.content);
            if state.rolling_summary.trim().is_empty() && recovered.is_empty() {
                state.rolling_summary.clear();
                state.supersedable_marker_id = None;
            } else if state.rolling_summary.trim().is_empty() {
                state.rolling_summary = recovered;
                state.supersedable_marker_id = marker_id;
            } else {
                state.supersedable_marker_id = marker_contains_rolling_summary(
                    &messages[marker_index],
                    &state.rolling_summary,
                )
                .then_some(messages[marker_index].id);
            }
            state.cursor = marker_index.saturating_add(1);
        } else {
            state.rolling_summary.clear();
            state.supersedable_marker_id = None;
            state.cursor = head_end;
        }
    }

    let (start, end) = find_one_exchange(messages, state.cursor, tail_start)?;
    if crate::tool_result_prune::estimate_tokens_rough(&state.rolling_summary)
        >= config.defrag_threshold_tokens.max(1)
    {
        if let Some(marker_index) =
            marker.filter(|index| state.supersedable_marker_id == Some(messages[*index].id))
        {
            return Some(MicroCompactionWork::Defrag {
                marker_index,
                old_summary: state.rolling_summary.clone(),
                cursor_after_commit: state.cursor,
            });
        }
    }

    Some(MicroCompactionWork::Summarize {
        start,
        end,
        existing_summary: state.rolling_summary.clone(),
        exchange_text: serialize_exchange(&messages[start..end]),
        supersede_marker_id: state.supersedable_marker_id,
    })
}

pub(crate) fn build_publication(
    messages: &[CompressionHistoryMessage],
    work: &MicroCompactionWork,
    updated_summary: &str,
) -> Option<MicroCompactionPublication> {
    let updated_summary = updated_summary
        .trim_matches(crate::python_value::python_whitespace)
        .trim();
    if updated_summary.is_empty() {
        return None;
    }
    match work {
        MicroCompactionWork::Defrag {
            marker_index,
            cursor_after_commit,
            ..
        } => {
            if *marker_index >= messages.len() {
                return None;
            }
            let mut rows = messages.iter().map(row_from_source).collect::<Vec<_>>();
            rows[*marker_index].content = render_marker(updated_summary);
            rows[*marker_index].api_content = None;
            rows[*marker_index].compressed_summary = true;
            Some(MicroCompactionPublication {
                rows,
                rolling_summary: updated_summary.into(),
                cursor_after_commit: *cursor_after_commit,
            })
        }
        MicroCompactionWork::Summarize {
            start,
            end,
            existing_summary,
            supersede_marker_id,
            ..
        } => {
            if *start >= *end || *end > messages.len() {
                return None;
            }
            let supersede = !existing_summary.trim().is_empty();
            let mut rows = Vec::with_capacity(messages.len().saturating_sub(end - start) + 1);
            for (index, message) in messages.iter().enumerate() {
                if index == *start {
                    rows.push(MicroCompactionRow {
                        source_id: None,
                        retained_ids: Vec::new(),
                        role: "assistant".into(),
                        content: render_marker(updated_summary),
                        api_content: None,
                        compressed_summary: true,
                        plain_text: true,
                    });
                }
                if (*start..*end).contains(&index) {
                    continue;
                }
                if supersede && *supersede_marker_id == Some(message.id) {
                    continue;
                }
                rows.push(row_from_source(message));
            }
            merge_adjacent_users(&mut rows);
            let marker_index = rows.iter().rposition(|row| row.source_id.is_none())?;
            Some(MicroCompactionPublication {
                rows,
                rolling_summary: updated_summary.into(),
                cursor_after_commit: marker_index.saturating_add(1),
            })
        }
    }
}

pub(crate) fn commit_success(
    state: &mut MicroCompactionState,
    publication: &MicroCompactionPublication,
) {
    state.rolling_summary = publication.rolling_summary.clone();
    state.cursor = publication.cursor_after_commit;
    state.consecutive_failures = 0;
    state.last_failure_cursor = None;
    // Publication clones rows to fresh durable ids. Resolve the marker id from
    // the next exact snapshot instead of retaining stale identity.
    state.supersedable_marker_id = None;
}

pub(crate) fn record_failure(state: &mut MicroCompactionState, work: &MicroCompactionWork) {
    let MicroCompactionWork::Summarize { start, end, .. } = work else {
        return;
    };
    if state.last_failure_cursor == Some(*start) {
        state.consecutive_failures = state.consecutive_failures.saturating_add(1);
    } else {
        state.consecutive_failures = 1;
        state.last_failure_cursor = Some(*start);
    }
    if state.consecutive_failures >= MAX_CONSECUTIVE_FAILURES {
        state.cursor = *end;
        state.consecutive_failures = 0;
        state.last_failure_cursor = None;
    }
}

pub(crate) fn build_prompt(existing_summary: &str, exchange_text: &str) -> Vec<Value> {
    let summary = if existing_summary.trim().is_empty() {
        "(No previous summary yet.)"
    } else {
        existing_summary
    };
    let prompt = format!(
        "You are a summarization agent creating a compact record of an ongoing conversation. You are given a running summary and the next exchange from the conversation. Merge the exchange's key decisions, requirements, file paths, and open questions into the summary. Preserve the summary's structure. Drop resolved details that are no longer relevant. Add new decisions, file paths, and open questions.\n\nNEVER include API keys, tokens, passwords, secrets, credentials, or connection strings in the summary. Replace any that appear with [REDACTED].\n\n## Current Running Summary\n{}\n\n## Next Exchange to Merge\n{}\n\nReturn ONLY the updated summary text, no preamble or explanation. Do not include this instruction block in your output.",
        crate::compression_redact::redact(summary),
        crate::compression_redact::redact(exchange_text),
    );
    vec![
        json!({"role":"system", "content":"You are a conversation summarization assistant."}),
        json!({"role":"user", "content":prompt}),
    ]
}

fn align_boundary_forward(messages: &[CompressionHistoryMessage], mut index: usize) -> usize {
    while index < messages.len() && messages[index].message.role == "tool" {
        index += 1;
    }
    index
}

fn find_one_exchange(
    messages: &[CompressionHistoryMessage],
    start: usize,
    tail_start: usize,
) -> Option<(usize, usize)> {
    let mut index = start;
    while index < tail_start && index < messages.len() {
        if messages[index].message.role == "assistant" && !is_context_summary(&messages[index]) {
            break;
        }
        index += 1;
    }
    if index >= tail_start || index >= messages.len() {
        return None;
    }
    let exchange_start = index;
    index += 1;
    while index < tail_start && index < messages.len() {
        let message = &messages[index];
        if !matches!(message.message.role.as_str(), "assistant" | "tool")
            || is_context_summary(message)
        {
            break;
        }
        index += 1;
    }
    if index <= exchange_start || index >= messages.len() {
        return None;
    }
    (!matches!(messages[index].message.role.as_str(), "assistant" | "tool"))
        .then_some((exchange_start, index))
}

fn is_context_summary(message: &CompressionHistoryMessage) -> bool {
    message.compressed_summary
        || message
            .message
            .content
            .trim_start()
            .starts_with(crate::compression_prompt::SUMMARY_PREFIX)
}

fn rolling_summary_from_marker(content: &str) -> String {
    let mut body = content;
    if let Some(index) = body.rfind(HISTORICAL_TASK_HEADING) {
        body = &body[index + HISTORICAL_TASK_HEADING.len()..];
    }
    if let Some(index) = body.find(crate::compression_prompt::SUMMARY_END) {
        body = &body[..index];
    }
    body.trim_matches(crate::python_value::python_whitespace)
        .to_owned()
}

fn marker_contains_rolling_summary(
    message: &CompressionHistoryMessage,
    rolling_summary: &str,
) -> bool {
    message.message.role == "assistant"
        && !rolling_summary.trim().is_empty()
        && rolling_summary_from_marker(&message.message.content) == rolling_summary.trim()
}

fn serialize_exchange(messages: &[CompressionHistoryMessage]) -> String {
    messages
        .iter()
        .map(|message| {
            let role = message.message.role.as_str();
            let mut content = summary_content_text(&message.message.model_content());
            content = crate::compression_redact::redact(&content);
            content = replace_media_directives(&content);
            if role == "assistant" {
                content = crate::visible_response::strip(&content);
            }
            content = truncate_summary_content(&content);
            match role {
                "tool" => format!(
                    "[TOOL RESULT {}]: {}",
                    message.tool_call_id.as_deref().unwrap_or(""),
                    content
                ),
                "assistant" => {
                    let mut rendered = content;
                    let calls = message
                        .tool_calls
                        .as_deref()
                        .and_then(|raw| serde_json::from_str::<Value>(raw).ok())
                        .and_then(|value| value.as_array().cloned())
                        .unwrap_or_default();
                    if !calls.is_empty() {
                        let calls = calls
                            .iter()
                            .map(|call| {
                                let function = call.get("function").and_then(Value::as_object);
                                let name = function
                                    .and_then(|function| function.get("name"))
                                    .and_then(Value::as_str)
                                    .unwrap_or("?");
                                let arguments = function
                                    .and_then(|function| function.get("arguments"))
                                    .map(|value| {
                                        value.as_str().map(str::to_owned).unwrap_or_else(|| {
                                            crate::python_value::python_repr(value)
                                        })
                                    })
                                    .unwrap_or_default();
                                let arguments = crate::compression_redact::redact(&arguments);
                                let arguments = if arguments.chars().count() > 1_500 {
                                    truncate_chars(&arguments, 1_200, 0)
                                } else {
                                    arguments
                                };
                                format!("  {name}({arguments})")
                            })
                            .collect::<Vec<_>>()
                            .join("\n");
                        rendered.push_str(&format!("\n[Tool calls:\n{calls}\n]"));
                    }
                    format!("[ASSISTANT]: {rendered}")
                }
                other => format!("[{}]: {}", other.to_uppercase(), content),
            }
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

fn summary_content_text(content: &Value) -> String {
    let Value::Array(parts) = content else {
        return match content {
            Value::Null => String::new(),
            Value::String(text) => text.clone(),
            other => crate::python_value::python_repr(other),
        };
    };
    parts
        .iter()
        .filter_map(|part| match part {
            Value::String(text) => Some(text.clone()),
            Value::Object(object) => match object.get("type").and_then(Value::as_str) {
                Some("text") => Some(
                    object
                        .get("text")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_owned(),
                ),
                Some("image" | "image_url" | "input_image") => {
                    let url = object
                        .get("image_url")
                        .and_then(|value| {
                            value
                                .as_str()
                                .or_else(|| value.get("url").and_then(Value::as_str))
                        })
                        .or_else(|| object.get("url").and_then(Value::as_str))
                        .unwrap_or("");
                    Some(
                        if url.starts_with("http://") || url.starts_with("https://") {
                            format!("[image: {url}]")
                        } else {
                            "[image]".into()
                        },
                    )
                }
                kind => Some(format!("[{}]", kind.unwrap_or("attachment"))),
            },
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn replace_media_directives(content: &str) -> String {
    let mut output = String::with_capacity(content.len());
    let mut rest = content;
    while let Some(start) = rest.find("MEDIA:") {
        output.push_str(&rest[..start]);
        let directive = &rest[start..];
        let end = directive
            .char_indices()
            .find_map(|(index, character)| character.is_whitespace().then_some(index))
            .unwrap_or(directive.len());
        output.push_str("[media attachment]");
        rest = &directive[end..];
    }
    output.push_str(rest);
    output
}

fn truncate_summary_content(content: &str) -> String {
    if content.chars().count() <= 6_000 {
        content.to_owned()
    } else {
        truncate_chars(content, 4_000, 1_500)
    }
}

fn truncate_chars(content: &str, head: usize, tail: usize) -> String {
    let chars = content.chars().collect::<Vec<_>>();
    if chars.len() <= head.saturating_add(tail) || tail == 0 && chars.len() <= head {
        return content.to_owned();
    }
    if tail == 0 {
        return format!(
            "{}...",
            chars[..head.min(chars.len())].iter().collect::<String>()
        );
    }
    format!(
        "{}\n...[truncated]...\n{}",
        chars[..head].iter().collect::<String>(),
        chars[chars.len() - tail..].iter().collect::<String>()
    )
}

fn render_marker(summary: &str) -> String {
    format!(
        "{}\n\n{}\n{}\n\n{}",
        crate::compression_prompt::SUMMARY_PREFIX,
        HISTORICAL_TASK_HEADING,
        summary.trim(),
        crate::compression_prompt::SUMMARY_END,
    )
}

fn row_from_source(message: &CompressionHistoryMessage) -> MicroCompactionRow {
    MicroCompactionRow {
        source_id: Some(message.id),
        retained_ids: vec![message.id],
        role: message.message.role.clone(),
        content: message.message.content.clone(),
        api_content: message.message.api_content.clone(),
        compressed_summary: message.compressed_summary,
        plain_text: matches!(message.message.model_content(), Value::String(_)),
    }
}

fn merge_adjacent_users(rows: &mut Vec<MicroCompactionRow>) {
    let mut merged = Vec::<MicroCompactionRow>::with_capacity(rows.len());
    for row in rows.drain(..) {
        let previous = merged.last_mut();
        if let Some(previous) = previous.filter(|previous| {
            previous.role == "user"
                && row.role == "user"
                && !previous.compressed_summary
                && !row.compressed_summary
                && previous.plain_text
                && row.plain_text
        }) {
            previous.content = match (previous.content.is_empty(), row.content.is_empty()) {
                (false, false) => format!("{}\n\n{}", previous.content, row.content),
                (true, _) => row.content,
                (_, true) => previous.content.clone(),
            };
            previous.api_content = None;
            previous.retained_ids.extend(row.retained_ids);
            continue;
        }
        merged.push(row);
    }
    *rows = merged;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session_db::{CompressionHistoryMessage, HistoryMessage};
    use serde_json::json;

    fn row(id: i64, role: &str, content: &str) -> CompressionHistoryMessage {
        CompressionHistoryMessage {
            id,
            message: HistoryMessage {
                role: role.into(),
                content: content.into(),
                api_content: None,
            },
            tool_call_id: None,
            tool_calls: None,
            tool_name: None,
            reasoning: None,
            reasoning_content: None,
            reasoning_details: None,
            codex_reasoning_items: None,
            codex_message_items: None,
            compressed_summary: false,
        }
    }

    fn conversation() -> Vec<CompressionHistoryMessage> {
        let mut rows = vec![row(1, "user", "question 0")];
        let mut call = row(2, "assistant", "");
        call.tool_calls = Some(
            json!([{"id":"c0","type":"function","function":{
                "name":"read_file","arguments":"{\"path\":\"src/lib.rs\"}"
            }}])
            .to_string(),
        );
        rows.push(call);
        let mut tool = row(3, "tool", "result 0");
        tool.tool_call_id = Some("c0".into());
        tool.tool_name = Some("read_file".into());
        rows.push(tool);
        rows.extend([
            row(4, "assistant", "answer 0"),
            row(5, "user", "question 1"),
            row(6, "assistant", "answer 1"),
            row(7, "user", "question 2"),
            row(8, "assistant", "answer 2"),
            row(9, "user", "question 3"),
            row(10, "assistant", "answer 3"),
        ]);
        rows
    }

    fn config(every_n_turns: usize, defrag_threshold_tokens: u64) -> MicroCompactionConfig {
        MicroCompactionConfig {
            protect_head: 1,
            protect_last: 2,
            tail_token_budget: 30,
            charge_all_thinking: false,
            every_n_turns,
            defrag_threshold_tokens,
        }
    }

    fn durable_rows(publication: &MicroCompactionPublication) -> Vec<CompressionHistoryMessage> {
        publication
            .rows
            .iter()
            .enumerate()
            .map(|(index, candidate)| CompressionHistoryMessage {
                id: 100 + i64::try_from(index).unwrap(),
                message: HistoryMessage {
                    role: candidate.role.clone(),
                    content: candidate.content.clone(),
                    api_content: candidate.api_content.clone(),
                },
                tool_call_id: candidate.source_id.and_then(|source_id| {
                    conversation()
                        .into_iter()
                        .find(|row| row.id == source_id)
                        .and_then(|row| row.tool_call_id)
                }),
                tool_calls: candidate.source_id.and_then(|source_id| {
                    conversation()
                        .into_iter()
                        .find(|row| row.id == source_id)
                        .and_then(|row| row.tool_calls)
                }),
                tool_name: None,
                reasoning: None,
                reasoning_content: None,
                reasoning_details: None,
                codex_reasoning_items: None,
                codex_message_items: None,
                compressed_summary: candidate.compressed_summary,
            })
            .collect()
    }

    #[test]
    fn one_due_pass_absorbs_one_complete_tool_exchange_and_keeps_user_bytes() {
        let messages = conversation();
        let mut state = MicroCompactionState::default();
        let work = prepare(&messages, config(1, 2_000), &mut state)
            .expect("one old exchange should be eligible");
        assert!(matches!(
            work,
            MicroCompactionWork::Summarize {
                start: 1,
                end: 4,
                ..
            }
        ));

        let publication = build_publication(&messages, &work, "rolling answer zero")
            .expect("nonempty summary should produce a publication");
        assert_eq!(
            publication
                .rows
                .iter()
                .filter(|row| row.compressed_summary)
                .count(),
            1
        );
        assert!(publication
            .rows
            .iter()
            .any(|row| row.content == "question 0"));
        assert!(!publication.rows.iter().any(|row| row.content == "answer 0"));
        assert!(publication
            .rows
            .iter()
            .any(|row| row.content.contains("rolling answer zero")));
        assert!(publication
            .rows
            .windows(2)
            .all(|pair| { !(pair[0].role == pair[1].role && pair[0].role != "tool") }));
    }

    #[test]
    fn cadence_and_poison_exchange_failure_are_bounded() {
        let messages = conversation();
        let mut state = MicroCompactionState::default();
        assert!(prepare(&messages, config(3, 2_000), &mut state).is_none());
        assert!(prepare(&messages, config(3, 2_000), &mut state).is_none());
        let work =
            prepare(&messages, config(3, 2_000), &mut state).expect("third completed turn is due");
        let (start, end) = match work {
            MicroCompactionWork::Summarize { start, end, .. } => (start, end),
            MicroCompactionWork::Defrag { .. } => panic!("fresh state cannot defrag"),
        };
        for _ in 0..3 {
            record_failure(&mut state, &work);
        }
        assert_eq!(state.cursor, end);
        assert_eq!(state.consecutive_failures, 0);
        assert_eq!(state.last_failure_cursor, None);
        assert!(end > start);
    }

    #[test]
    fn successive_pass_supersedes_marker_and_preserves_all_user_text() {
        let original = conversation();
        let mut state = MicroCompactionState::default();
        let first = prepare(&original, config(1, 2_000), &mut state).unwrap();
        let first_publication = build_publication(&original, &first, "summary one").unwrap();
        commit_success(&mut state, &first_publication);
        let durable = durable_rows(&first_publication);

        let second = prepare(&durable, config(1, 2_000), &mut state).unwrap();
        assert!(matches!(
            &second,
            MicroCompactionWork::Summarize { existing_summary, .. }
                if existing_summary == "summary one"
        ));
        let second_publication = build_publication(&durable, &second, "summary two").unwrap();
        assert_eq!(
            second_publication
                .rows
                .iter()
                .filter(|row| row.compressed_summary)
                .count(),
            1
        );
        let all_user_text = second_publication
            .rows
            .iter()
            .filter(|row| row.role == "user")
            .map(|row| row.content.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        for question in ["question 0", "question 1", "question 2", "question 3"] {
            assert!(all_user_text.contains(question), "missing {question}");
        }
        assert!(second_publication
            .rows
            .windows(2)
            .all(|pair| { !(pair[0].role == pair[1].role && pair[0].role != "tool") }));
    }

    #[test]
    fn resume_rehydrates_marker_and_defrag_rewrites_it_without_a_splice() {
        let original = conversation();
        let mut first_state = MicroCompactionState::default();
        let work = prepare(&original, config(1, 2_000), &mut first_state).unwrap();
        let first = build_publication(&original, &work, "summary to recover").unwrap();
        let durable = durable_rows(&first);
        let marker_index = durable
            .iter()
            .position(|row| row.compressed_summary)
            .unwrap();

        let mut resumed = MicroCompactionState::default();
        let defrag = prepare(&durable, config(1, 1), &mut resumed).unwrap();
        assert_eq!(resumed.rolling_summary, "summary to recover");
        assert!(matches!(
            defrag,
            MicroCompactionWork::Defrag { marker_index: found, .. } if found == marker_index
        ));
        let rewritten = build_publication(&durable, &defrag, "short summary").unwrap();
        assert_eq!(rewritten.rows.len(), durable.len());
        assert_eq!(
            rewritten.rows[marker_index].source_id,
            Some(durable[marker_index].id)
        );
        assert!(rewritten.rows[marker_index]
            .content
            .contains("short summary"));

        let mut extended = durable.clone();
        extended.push(row(200, "user", "question 4"));
        extended.push(row(201, "assistant", "answer 4"));
        let mut advanced = MicroCompactionState {
            rolling_summary: "summary to recover".into(),
            cursor: marker_index + 3,
            supersedable_marker_id: Some(durable[marker_index].id),
            ..Default::default()
        };
        let defrag = prepare(&extended, config(1, 1), &mut advanced).unwrap();
        let rewritten = build_publication(&extended, &defrag, "shorter summary").unwrap();
        assert_eq!(rewritten.cursor_after_commit, marker_index + 3);
    }

    #[test]
    fn empty_marker_rehydration_never_drops_that_marker() {
        let original = conversation();
        let mut first_state = MicroCompactionState::default();
        let work = prepare(&original, config(1, 2_000), &mut first_state).unwrap();
        let first = build_publication(&original, &work, "summary one").unwrap();
        let mut durable = durable_rows(&first);
        let marker = durable
            .iter_mut()
            .find(|row| row.compressed_summary)
            .unwrap();
        marker.message.content.clear();
        let mut resumed = MicroCompactionState::default();
        let next = prepare(&durable, config(1, 2_000), &mut resumed).unwrap();
        let publication = build_publication(&durable, &next, "new isolated summary").unwrap();
        assert_eq!(
            publication
                .rows
                .iter()
                .filter(|row| row.compressed_summary)
                .count(),
            2
        );
    }

    #[test]
    fn exchange_serializer_strips_reasoning_media_and_bounds_large_rows() {
        let mut assistant = row(
            1,
            "assistant",
            "<think>discard this scratch</think>kept answer MEDIA:/tmp/private.png",
        );
        assistant.tool_calls = Some(
            json!([{"id":"call","type":"function","function":{
                "name":"terminal","arguments":"{\"command\":\"cargo test\"}"
            }}])
            .to_string(),
        );
        let mut tool = row(2, "tool", &format!("{}TAIL", "x".repeat(7_000)));
        tool.tool_call_id = Some("call".into());
        let serialized = serialize_exchange(&[assistant, tool]);
        assert!(!serialized.contains("discard this scratch"));
        assert!(!serialized.contains("MEDIA:/tmp/private.png"));
        assert!(serialized.contains("[media attachment]"));
        assert!(serialized.contains("terminal({\"command\":\"cargo test\"})"));
        assert!(serialized.contains("...[truncated]..."));
        assert!(serialized.ends_with("TAIL"));
    }

    #[test]
    fn resumed_batch_marker_is_adopted_before_it_is_superseded() {
        let mut batch = row(
            1,
            "user",
            &format!(
                "{}\n\n{}\nbatch history\n\n{}",
                crate::compression_prompt::SUMMARY_PREFIX,
                HISTORICAL_TASK_HEADING,
                crate::compression_prompt::SUMMARY_END
            ),
        );
        batch.compressed_summary = true;
        let messages = vec![
            batch,
            row(2, "assistant", crate::compression_prompt::SUMMARY_ACK),
            row(3, "user", "question 1"),
            row(4, "assistant", "answer 1"),
            row(5, "user", "question 2"),
            row(6, "assistant", "answer 2"),
            row(7, "user", "question 3"),
            row(8, "assistant", "answer 3"),
        ];
        let mut state = MicroCompactionState::default();
        let work = prepare(
            &messages,
            MicroCompactionConfig {
                protect_head: 0,
                ..config(1, 2_000)
            },
            &mut state,
        )
        .unwrap();
        assert_eq!(state.rolling_summary, "batch history");
        let publication = build_publication(&messages, &work, "batch plus ack").unwrap();
        assert_eq!(publication.rows[0].role, "assistant");
        assert_eq!(
            publication
                .rows
                .iter()
                .filter(|row| row.compressed_summary)
                .count(),
            1
        );
        assert!(publication.rows[0].content.contains("batch plus ack"));
    }

    #[test]
    fn stale_rolling_state_never_supersedes_an_unabsorbed_batch_marker() {
        let mut batch = row(
            1,
            "user",
            &format!(
                "{}\n\n{}\nbatch history that is not in stale state\n\n{}",
                crate::compression_prompt::SUMMARY_PREFIX,
                HISTORICAL_TASK_HEADING,
                crate::compression_prompt::SUMMARY_END
            ),
        );
        batch.compressed_summary = true;
        let messages = vec![
            batch,
            row(2, "assistant", crate::compression_prompt::SUMMARY_ACK),
            row(3, "user", "question 1"),
            row(4, "assistant", "answer 1"),
            row(5, "user", "question 2"),
            row(6, "assistant", "answer 2"),
            row(7, "user", "question 3"),
            row(8, "assistant", "answer 3"),
        ];
        let mut state = MicroCompactionState {
            rolling_summary: "stale micro-only history".into(),
            cursor: 2,
            ..Default::default()
        };
        let work = prepare(
            &messages,
            MicroCompactionConfig {
                protect_head: 0,
                ..config(1, 1)
            },
            &mut state,
        )
        .unwrap();
        assert!(matches!(
            work,
            MicroCompactionWork::Summarize {
                supersede_marker_id: None,
                ..
            }
        ));
        let publication = build_publication(&messages, &work, "new micro summary").unwrap();
        assert!(publication.rows.iter().any(|row| {
            row.role == "user"
                && row
                    .content
                    .contains("batch history that is not in stale state")
        }));
        assert_eq!(
            publication
                .rows
                .iter()
                .filter(|row| row.compressed_summary)
                .count(),
            2
        );
    }
}
