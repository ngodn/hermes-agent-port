//! Deterministic, no-LLM pre-compression tool-result pruning.
//!
//! Ports the deterministic Phase-1 prune in
//! `agent/context_compressor.py`:
//! - `ContextCompressor._prune_old_tool_results`, including its count-based
//!   proactive mode and token-budget pre-compression mode.
//! - `_summarize_tool_result` / `_summarize_tool_result_unguarded`
//! - `_truncate_tool_call_args_json`
//! - `_retire_stale_tool_result_images` and its image-shape helpers
//! - `_collect_protected_skill_names` / `_skill_view_call_sites`
//!
//! This module is pure: it never mutates its input, never touches SQLite or
//! ingress, and returns a fresh candidate list plus explicit change/reclaim
//! metadata so a caller can implement the prompt-cache no-op contract itself.
//!
//! Passes implemented (in Python order):
//!   1. Deduplicate byte-identical tool results (tail-agnostic, lossless).
//!   2. Summarize old tool results outside the protected tail.
//!   3. Truncate oversized tool_call arguments on old assistant messages.
//!
//! Pass 3.5 retires image payloads on all but the newest
//! `MAX_KEEP_TOOL_IMAGES` image-bearing tool results (tail-agnostic, lossy by
//! design).
//!
//! Pass 4 applies only in token-budget mode. It relieves an oversized protected
//! tail by demoting completed tool output and oversized call arguments while
//! preserving the most recent useful material for as long as the budget allows.
//! The prune commit/rearm/reclaim gates and their persistence stay at the caller.

use std::collections::{HashMap, HashSet};

use serde_json::{json, Value};

use crate::session_db::{encode_message_content, CompressionHistoryMessage};

/// Minimum char length for a tool result to be eligible for dedup (Pass 1) and
/// the default floor for summarization (Pass 2). Mirrors Python
/// `_PRUNE_MIN_CHARS`.
pub const PRUNE_MIN_CHARS: usize = 200;
/// Newest image-bearing tool results kept verbatim by Pass 3.5. Mirrors Python
/// `_MAX_KEEP_TOOL_IMAGES`.
pub const MAX_KEEP_TOOL_IMAGES: usize = 3;
/// skill_view results at or below this size stay verbatim (no ghost-skill
/// marker needed). Mirrors Python `_SKILL_VIEW_PRUNE_MIN_CHARS`.
pub const SKILL_VIEW_PRUNE_MIN_CHARS: usize = 5000;
/// A skill_view within this many messages of the end is "just loaded" and its
/// body is protected from ordinary demotion. Mirrors Python
/// `_SKILL_PRUNE_RECENT_WINDOW`.
pub const SKILL_PRUNE_RECENT_WINDOW: usize = 10;
/// Ghost-skill marker prefix. Mirrors Python `SKILL_PRUNED_MARKER_PREFIX`.
pub const SKILL_PRUNED_MARKER_PREFIX: &str = "[SKILL_PRUNED:";
/// Maximum count floor applied to a token-protected tail.
pub const MAX_TAIL_MESSAGE_FLOOR: usize = 8;
/// Recent messages spared by the first protected-tail pressure stage.
pub const PRESSURE_KEEP_RECENT_MESSAGES: usize = 3;
const CHARS_PER_TOKEN: usize = 4;
const IMAGE_CHAR_EQUIVALENT: usize = 6_400;

/// The generic placeholder that a prior prune may have left behind. Mirrors
/// Python `_PRUNED_TOOL_PLACEHOLDER`. Never emitted by this module, only
/// recognized as an "already pruned" guard.
const PRUNED_TOOL_PLACEHOLDER: &str = "[Old tool output cleared to save context space]";
const DUPLICATE_BACKREF: &str = "[Duplicate tool output - same content as a more recent call]";

/// Result of one prune run. The input is never mutated; `messages` is always a
/// fresh list. `changed` is false for a genuine no-op so a caller can honor the
/// standard "return the input object when nothing was reclaimed" contract.
#[derive(Debug, Clone, PartialEq)]
pub struct PruneOutcome {
    /// Candidate pruned message list. Equal to the input when `changed` is false.
    pub messages: Vec<CompressionHistoryMessage>,
    /// Whether any message field differs from the input.
    pub changed: bool,
    /// Count of demotions in the sense Python increments `pruned`: dedup
    /// back-references (Pass 1), tool-result summarizations / image strips
    /// reached by Pass 2, and image retirements (Pass 3.5). Assistant
    /// tool_call argument truncation (Pass 3) is NOT counted, matching Python.
    pub pruned_count: usize,
    /// Total characters reclaimed from the clean `content` and `tool_calls`
    /// fields (char count, not bytes). `api_content` is not counted here and
    /// is cleared only when Python's image-retirement path clears it.
    pub reclaimed_chars: usize,
}

/// Prune old tool results with the recent tail protected by message COUNT.
///
/// `protect_tail_count` messages at the end are shielded from Pass 2 and Pass 3
/// (summarization and argument truncation). Pass 1 (dedup) and Pass 3.5 (image
/// retirement) are tail-agnostic by design, exactly as in Python. `min_prune_chars`
/// raises only the Pass-2 floor; Pass 1 and Pass 3 keep their own fixed floors.
pub fn prune_old_tool_results(
    messages: &[CompressionHistoryMessage],
    protect_tail_count: usize,
    min_prune_chars: usize,
) -> PruneOutcome {
    prune_old_tool_results_inner(messages, protect_tail_count, None, min_prune_chars, false)
}

/// Prune old tool results with a token-budget protected tail.
///
/// The token budget is primary while `protect_tail_count`, capped at
/// [`MAX_TAIL_MESSAGE_FLOOR`], remains a hard minimum. `charge_all_thinking`
/// must reflect whether the active provider actually replays generic thinking
/// text on stale assistant turns. Codex replay sidecars are always charged.
pub fn prune_old_tool_results_with_budget(
    messages: &[CompressionHistoryMessage],
    protect_tail_count: usize,
    protect_tail_tokens: u64,
    min_prune_chars: usize,
    charge_all_thinking: bool,
) -> PruneOutcome {
    prune_old_tool_results_inner(
        messages,
        protect_tail_count,
        Some(protect_tail_tokens),
        min_prune_chars,
        charge_all_thinking,
    )
}

fn prune_old_tool_results_inner(
    messages: &[CompressionHistoryMessage],
    protect_tail_count: usize,
    protect_tail_tokens: Option<u64>,
    min_prune_chars: usize,
    charge_all_thinking: bool,
) -> PruneOutcome {
    if messages.is_empty() {
        return PruneOutcome {
            messages: Vec::new(),
            changed: false,
            pruned_count: 0,
            reclaimed_chars: 0,
        };
    }

    let mut result: Vec<CompressionHistoryMessage> = messages.to_vec();
    let mut pruned: usize = 0;

    // Index: tool_call_id -> (tool_name, arguments_json). Built from every
    // assistant message's tool_calls, mirroring the Python index build.
    let call_id_to_tool = build_call_id_index(&result);

    // Indices [0, boundary) are prunable. The proactive path uses the fixed
    // count boundary; full compression uses the Python token-budget walk.
    let prune_boundary = compute_prune_boundary(
        &result,
        protect_tail_count,
        protect_tail_tokens,
        charge_all_thinking,
    );

    // Pass 1: deduplicate identical tool results (tail-agnostic). Walk newest
    // first, keep the newest full copy, replace older exact duplicates with a
    // back-reference. Lossless: the unique newest copy always survives.
    //
    // Divergence from Python: Python keys on `md5(content)[:12]`; we key on the
    // exact content string. Observationally identical for real transcripts and
    // strictly safer (no 48-bit collision can ever back-reference two genuinely
    // different outputs).
    let mut seen_content: HashSet<String> = HashSet::new();
    for i in (0..result.len()).rev() {
        if role(&result[i]) != "tool" {
            continue;
        }
        // Only plain-string tool content is hashable/dedupable. Structured
        // (list / multimodal) content is skipped, matching Python.
        let Some(content) = string_content(&result[i]) else {
            continue;
        };
        if char_len(&content) < PRUNE_MIN_CHARS {
            continue;
        }
        if seen_content.contains(&content) {
            set_content(&mut result[i], DUPLICATE_BACKREF.to_string());
            pruned += 1;
        } else {
            seen_content.insert(content);
        }
    }

    // Ghost-skill defense: skills just loaded, or referenced in the protected
    // tail, keep their full skill_view bodies through Pass 2.
    let protected_skills = collect_protected_skill_names(&result, prune_boundary);

    // Pass 2: summarize old tool results outside the protected tail. The
    // return value is unused here; `pruned` is updated inside on a hit.
    for i in 0..prune_boundary {
        demote_tool_result_at(
            &mut result,
            i,
            &call_id_to_tool,
            &protected_skills,
            min_prune_chars,
            true,
            &mut pruned,
        );
    }

    // Pass 3: truncate oversized tool_call arguments on old assistant messages.
    // Does not increment `pruned` (parity with Python), but does count toward
    // `changed` and reclaimed chars via the final diff.
    for i in 0..prune_boundary {
        truncate_tool_call_args_at(&mut result, i);
    }

    // Pass 3.5: retire stale tool-result images across the whole list.
    pruned += retire_stale_tool_result_images(&mut result, MAX_KEEP_TOOL_IMAGES);

    // Pass 4: if the protected region itself exceeds 1.5x the token budget,
    // reclaim completed tool weight inside it. This intentionally overrides
    // the ordinary protected-skill guard, matching Python's escape hatch for
    // otherwise uncompressible heavy tails.
    if let Some(budget) = protect_tail_tokens.filter(|budget| *budget > 0) {
        let soft_ceiling = budget.saturating_mul(3) / 2;
        let keep_recent = PRESSURE_KEEP_RECENT_MESSAGES.min(result.len());
        let demote_end = result.len().saturating_sub(keep_recent);
        if demote_end > prune_boundary
            && protected_region_tokens(&result, prune_boundary) > soft_ceiling
        {
            for i in prune_boundary..demote_end {
                demote_tool_result_at(
                    &mut result,
                    i,
                    &call_id_to_tool,
                    &protected_skills,
                    min_prune_chars,
                    false,
                    &mut pruned,
                );
                truncate_tool_call_args_at(&mut result, i);
                if protected_region_tokens(&result, prune_boundary) <= soft_ceiling {
                    break;
                }
            }

            if protected_region_tokens(&result, prune_boundary) > soft_ceiling {
                let last_tool_idx = result.iter().rposition(|message| role(message) == "tool");
                for i in prune_boundary..result.len() {
                    if last_tool_idx == Some(i) {
                        continue;
                    }
                    if role(&result[i]) == "tool" {
                        demote_tool_result_at(
                            &mut result,
                            i,
                            &call_id_to_tool,
                            &protected_skills,
                            min_prune_chars,
                            false,
                            &mut pruned,
                        );
                    } else if role(&result[i]) == "assistant" {
                        truncate_tool_call_args_at(&mut result, i);
                    }
                }
                if let Some(last_tool_idx) = last_tool_idx.filter(|idx| *idx >= prune_boundary) {
                    if protected_region_tokens(&result, prune_boundary) > soft_ceiling {
                        demote_tool_result_at(
                            &mut result,
                            last_tool_idx,
                            &call_id_to_tool,
                            &protected_skills,
                            min_prune_chars,
                            false,
                            &mut pruned,
                        );
                    }
                }
            }
        }
    }

    // Compute change / reclaim metadata against the untouched input.
    let mut reclaimed: i64 = 0;
    let mut changed = false;
    for (before, after) in messages.iter().zip(result.iter()) {
        if before != after {
            changed = true;
        }
        let before_chars =
            char_len(&before.message.content) + before.tool_calls.as_deref().map_or(0, char_len);
        let after_chars =
            char_len(&after.message.content) + after.tool_calls.as_deref().map_or(0, char_len);
        reclaimed += before_chars as i64 - after_chars as i64;
    }

    PruneOutcome {
        messages: result,
        changed,
        pruned_count: pruned,
        reclaimed_chars: reclaimed.max(0) as usize,
    }
}

fn compute_prune_boundary(
    messages: &[CompressionHistoryMessage],
    protect_tail_count: usize,
    protect_tail_tokens: Option<u64>,
    charge_all_thinking: bool,
) -> usize {
    let Some(budget) = protect_tail_tokens.filter(|budget| *budget > 0) else {
        return messages.len().saturating_sub(protect_tail_count);
    };
    let min_protect = protect_tail_count
        .min(messages.len())
        .min(MAX_TAIL_MESSAGE_FLOOR);
    let newest_assistant = messages
        .iter()
        .rposition(|message| role(message) == "assistant");
    let mut accumulated = 0u64;
    let mut boundary = messages.len();
    for i in (0..messages.len()).rev() {
        let message_tokens = estimate_message_budget_tokens(
            &messages[i],
            charge_all_thinking || newest_assistant == Some(i),
        );
        if accumulated.saturating_add(message_tokens) > budget
            && messages.len().saturating_sub(i) >= min_protect
        {
            boundary = i;
            break;
        }
        accumulated = accumulated.saturating_add(message_tokens);
        boundary = i;
    }
    let budget_protect_count = messages.len().saturating_sub(boundary);
    let protected_count = budget_protect_count.max(min_protect);
    messages.len().saturating_sub(protected_count)
}

/// Select the verbatim tail retained by full compression.
///
/// Mirrors Python `_find_tail_cut_by_tokens`: a 1.5x soft ceiling, bounded
/// count floor, raw-budget retry when the whole region fits, tool-group
/// alignment, newest user/assistant anchors, and optional multi-user anchoring.
pub(crate) fn find_tail_cut_by_tokens(
    messages: &[CompressionHistoryMessage],
    head_end: usize,
    protect_last_n: usize,
    token_budget: u64,
    charge_all_thinking: bool,
    min_tail_user_messages: usize,
) -> usize {
    let min_tail_user_messages = min_tail_user_messages.max(1);
    let n = messages.len();
    if n == 0 {
        return 0;
    }
    let available_tail = n.saturating_sub(head_end.saturating_add(1));
    let min_tail_floor = 3.max(protect_last_n.min(MAX_TAIL_MESSAGE_FLOOR));
    let compressible_tail_cap = 3.max(available_tail.saturating_sub(2));
    let min_tail = if available_tail > 1 {
        min_tail_floor
            .min(compressible_tail_cap)
            .min(available_tail)
    } else {
        0
    };
    let soft_ceiling = token_budget.saturating_mul(3) / 2;
    let newest_assistant = messages
        .iter()
        .rposition(|message| role(message) == "assistant");
    let charge = |idx: usize| {
        estimate_message_budget_tokens(
            &messages[idx],
            charge_all_thinking || newest_assistant == Some(idx),
        )
    };

    let mut accumulated = 0u64;
    let mut cut = n;
    for i in (head_end.min(n)..n).rev() {
        let message_tokens = charge(i);
        if accumulated.saturating_add(message_tokens) > soft_ceiling
            && n.saturating_sub(i) >= min_tail
        {
            break;
        }
        accumulated = accumulated.saturating_add(message_tokens);
        cut = i;
    }

    if cut <= head_end && accumulated <= soft_ceiling && accumulated > 0 {
        let mut raw_accumulated = 0u64;
        for i in (head_end.min(n)..n).rev() {
            let message_tokens = charge(i);
            if raw_accumulated.saturating_add(message_tokens) > token_budget
                && n.saturating_sub(i) >= min_tail
            {
                cut = i;
                break;
            }
            raw_accumulated = raw_accumulated.saturating_add(message_tokens);
            cut = i;
        }
    }

    let fallback_cut = n.saturating_sub(min_tail);
    cut = cut.min(fallback_cut);
    if cut <= head_end {
        cut = fallback_cut.max(head_end.saturating_add(1));
    }
    cut = align_tool_boundary_backward(messages, cut);
    cut = anchor_last_user(messages, cut, head_end);
    cut = anchor_last_assistant(messages, cut, head_end);
    if min_tail_user_messages > 1 {
        cut = anchor_last_n_user_messages(messages, cut, head_end, min_tail_user_messages);
    }
    align_tool_boundary_forward(messages, cut.max(head_end.saturating_add(1))).min(n)
}

fn align_tool_boundary_forward(messages: &[CompressionHistoryMessage], mut idx: usize) -> usize {
    while idx < messages.len() && role(&messages[idx]) == "tool" {
        idx += 1;
    }
    idx
}

fn align_tool_boundary_backward(messages: &[CompressionHistoryMessage], mut idx: usize) -> usize {
    if idx == 0 || idx >= messages.len() {
        return idx;
    }
    let mut check = idx - 1;
    while role(&messages[check]) == "tool" {
        if check == 0 {
            return idx;
        }
        check -= 1;
    }
    if role(&messages[check]) == "assistant" && !parse_tool_calls(&messages[check]).is_empty() {
        idx = check;
    }
    idx
}

fn content_text_for_contains(msg: &CompressionHistoryMessage) -> String {
    match decoded_content(msg) {
        Value::String(text) => text,
        Value::Array(parts) => {
            let mut texts = Vec::new();
            for part in parts {
                match part {
                    Value::String(text) => texts.push(text),
                    Value::Object(object) => {
                        if let Some(text) = object.get("text").and_then(Value::as_str) {
                            texts.push(text.to_owned());
                        }
                    }
                    _ => {}
                }
            }
            texts.join("\n")
        }
        _ => String::new(),
    }
}

fn is_summary_content(message: &CompressionHistoryMessage) -> bool {
    if message.compressed_summary {
        return true;
    }
    let text = content_text_for_contains(message);
    crate::compression_prompt::is_summary_content(&text)
}

fn is_synthetic_compression_user_turn(message: &CompressionHistoryMessage) -> bool {
    if role(message) != "user" {
        return false;
    }
    if is_summary_content(message) {
        return true;
    }
    let text = content_text_for_contains(message);
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return false;
    }
    if crate::compression_prompt::is_synthetic_compression_user_content(trimmed) {
        return true;
    }

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
        || trimmed.starts_with("[System: Your previous tool call ")
}

fn is_blank_user_turn(message: &CompressionHistoryMessage) -> bool {
    if role(message) != "user" || is_summary_content(message) {
        return false;
    }
    match decoded_content(message) {
        Value::Null => true,
        Value::String(text) => text.trim().is_empty(),
        Value::Array(parts) => {
            if parts.is_empty() {
                return true;
            }
            for part in parts {
                match part {
                    Value::String(text) if !text.trim().is_empty() => return false,
                    Value::Object(object) => {
                        let is_text_type = matches!(
                            object.get("type").and_then(Value::as_str),
                            Some("text") | Some("input_text")
                        );
                        if is_text_type {
                            let Some(text) = object.get("text").and_then(Value::as_str) else {
                                return false;
                            };
                            if !text.trim().is_empty() {
                                return false;
                            }
                        } else {
                            return false;
                        }
                    }
                    _ => return false,
                }
            }
            true
        }
        _ => false,
    }
}

fn actionable_user(message: &CompressionHistoryMessage) -> bool {
    role(message) == "user" && !is_summary_content(message) && !is_blank_user_turn(message)
}

fn real_actionable_user(message: &CompressionHistoryMessage) -> bool {
    actionable_user(message) && !is_synthetic_compression_user_turn(message)
}

fn anchor_last_n_user_messages(
    messages: &[CompressionHistoryMessage],
    cut: usize,
    head_end: usize,
    n: usize,
) -> usize {
    if n <= 1 {
        return anchor_last_user(messages, cut, head_end);
    }
    let user_indices: Vec<usize> = (head_end.min(messages.len())..messages.len())
        .rev()
        .filter(|idx| real_actionable_user(&messages[*idx]))
        .collect();

    if user_indices.is_empty() {
        return cut;
    }

    let target_idx = if user_indices.len() < n {
        user_indices[user_indices.len() - 1]
    } else {
        user_indices[n - 1]
    };

    if target_idx >= cut {
        return cut;
    }

    target_idx.max(head_end.saturating_add(1))
}

fn find_turn_pair_end(messages: &[CompressionHistoryMessage], user_idx: usize) -> usize {
    let mut idx = user_idx.saturating_add(1);
    if idx >= messages.len() || role(&messages[idx]) != "assistant" {
        return idx;
    }
    idx += 1;
    while idx < messages.len() && role(&messages[idx]) == "tool" {
        idx += 1;
    }
    idx
}

fn anchor_last_user(messages: &[CompressionHistoryMessage], cut: usize, head_end: usize) -> usize {
    let Some(last_user) = (head_end.min(messages.len())..messages.len())
        .rev()
        .find(|idx| actionable_user(&messages[*idx]))
    else {
        return cut;
    };
    if last_user >= cut {
        return cut;
    }
    let adjusted = last_user.max(head_end.saturating_add(1));
    if adjusted > last_user {
        find_turn_pair_end(messages, last_user).max(head_end.saturating_add(1))
    } else {
        adjusted
    }
}

fn visible_assistant_content(message: &CompressionHistoryMessage) -> bool {
    if role(message) != "assistant" || is_summary_content(message) {
        return false;
    }
    match decoded_content(message) {
        Value::String(text) => !text.trim().is_empty(),
        Value::Array(parts) => parts.iter().any(|part| {
            part.as_object().is_some_and(|object| {
                object
                    .get("text")
                    .or_else(|| object.get("content"))
                    .and_then(Value::as_str)
                    .is_some_and(|text| !text.trim().is_empty())
            })
        }),
        _ => false,
    }
}

fn anchor_last_assistant(
    messages: &[CompressionHistoryMessage],
    cut: usize,
    head_end: usize,
) -> usize {
    let range = head_end.min(messages.len())..messages.len();
    let visible = range
        .clone()
        .rev()
        .find(|idx| visible_assistant_content(&messages[*idx]));
    let fallback = range
        .rev()
        .find(|idx| role(&messages[*idx]) == "assistant" && !is_summary_content(&messages[*idx]));
    let Some(last_assistant) = visible.or(fallback) else {
        return cut;
    };
    if last_assistant >= cut {
        return cut;
    }
    align_tool_boundary_backward(messages, last_assistant).max(head_end.saturating_add(1))
}

fn protected_region_tokens(messages: &[CompressionHistoryMessage], start: usize) -> u64 {
    messages[start.min(messages.len())..]
        .iter()
        // Python's pressure pass uses the estimator default here, which
        // conservatively charges generic thinking on every protected row.
        .fold(0u64, |total, message| {
            total.saturating_add(estimate_message_budget_tokens(message, true))
        })
}

/// Python-compatible rough token estimate used by compression budget walks.
/// ASCII is charged at roughly four characters per token. CJK, Hangul and
/// full-width code points are charged one-for-one, with the remaining text
/// charged by UTF-8 byte length.
pub(crate) fn estimate_tokens_rough(text: &str) -> u64 {
    if text.is_empty() {
        return 0;
    }
    if text.is_ascii() {
        return u64::try_from(char_len(text).saturating_add(3) / CHARS_PER_TOKEN)
            .unwrap_or(u64::MAX);
    }
    let mut dense = 0usize;
    let mut sparse_bytes = 0usize;
    for character in text.chars() {
        if is_dense_cjk(character) {
            dense = dense.saturating_add(1);
        } else {
            sparse_bytes = sparse_bytes.saturating_add(character.len_utf8());
        }
    }
    u64::try_from(dense.saturating_add(sparse_bytes.saturating_add(3) / CHARS_PER_TOKEN))
        .unwrap_or(u64::MAX)
}

fn is_dense_cjk(character: char) -> bool {
    matches!(
        character as u32,
        0x1100..=0x11ff
            | 0x2e80..=0x9fff
            | 0xa960..=0xa97f
            | 0xac00..=0xd7af
            | 0xf900..=0xfaff
            | 0xff00..=0xffef
    )
}

/// Estimate one durable message's contribution to the token-protected tail.
/// This includes hidden replay fields that do not appear in clean content.
pub(crate) fn estimate_message_budget_tokens(
    message: &CompressionHistoryMessage,
    charge_stale_thinking: bool,
) -> u64 {
    let content = decoded_content(message);
    let mut tokens = match &content {
        Value::String(text) => estimate_tokens_rough(text).saturating_add(10),
        other => u64::try_from(content_length_for_budget(other) / CHARS_PER_TOKEN)
            .unwrap_or(u64::MAX)
            .saturating_add(10),
    };
    for call in parse_tool_calls(message) {
        if call.is_object() {
            tokens = tokens.saturating_add(estimate_tokens_rough(
                &crate::python_value::python_repr(&call),
            ));
        }
    }
    for raw in [
        message.codex_reasoning_items.as_deref(),
        message.codex_message_items.as_deref(),
    ]
    .into_iter()
    .flatten()
    {
        tokens = tokens.saturating_add(
            u64::try_from(serialized_raw_length(raw) / CHARS_PER_TOKEN).unwrap_or(u64::MAX),
        );
    }
    if !charge_stale_thinking {
        return tokens;
    }

    let reasoning_content_wins = message
        .reasoning_content
        .as_deref()
        .is_some_and(|text| !text.trim().is_empty());
    if !reasoning_content_wins {
        if let Some(reasoning) = message.reasoning.as_deref() {
            tokens = tokens.saturating_add(
                u64::try_from(char_len(reasoning) / CHARS_PER_TOKEN).unwrap_or(u64::MAX),
            );
        }
    }
    if let Some(reasoning_content) = message.reasoning_content.as_deref() {
        tokens = tokens.saturating_add(
            u64::try_from(char_len(reasoning_content) / CHARS_PER_TOKEN).unwrap_or(u64::MAX),
        );
    }
    if message.reasoning.as_deref().is_none_or(str::is_empty)
        && message
            .reasoning_content
            .as_deref()
            .is_none_or(str::is_empty)
    {
        if let Some(raw) = message.reasoning_details.as_deref() {
            let details = serde_json::from_str(raw).unwrap_or_else(|_| Value::String(raw.into()));
            tokens = tokens.saturating_add(
                u64::try_from(reasoning_details_text_chars(&details) / CHARS_PER_TOKEN)
                    .unwrap_or(u64::MAX),
            );
        }
    }
    tokens
}

fn content_length_for_budget(content: &Value) -> usize {
    let Value::Array(parts) = content else {
        return if crate::python_value::truthy(content) {
            char_len(&crate::python_value::python_repr(content))
        } else {
            0
        };
    };
    parts.iter().fold(0usize, |total, part| {
        let length = match part {
            Value::String(text) => char_len(text),
            Value::Object(object)
                if matches!(
                    object.get("type").and_then(Value::as_str),
                    Some("image_url") | Some("input_image") | Some("image")
                ) =>
            {
                IMAGE_CHAR_EQUIVALENT
            }
            Value::Object(object) => object
                .get("text")
                .and_then(Value::as_str)
                .map_or(0, char_len),
            other => char_len(&crate::python_value::python_repr(other)),
        };
        total.saturating_add(length)
    })
}

fn serialized_raw_length(raw: &str) -> usize {
    let value = serde_json::from_str(raw).unwrap_or_else(|_| Value::String(raw.into()));
    match value {
        Value::Null => 0,
        Value::String(ref text) if text.is_empty() => 0,
        Value::String(ref text) => char_len(text),
        other => char_len(&sorted_json_unicode(&other)),
    }
}

fn sorted_json_unicode(value: &Value) -> String {
    match value {
        Value::Array(items) => format!(
            "[{}]",
            items
                .iter()
                .map(sorted_json_unicode)
                .collect::<Vec<_>>()
                .join(", ")
        ),
        Value::Object(object) => {
            let mut keys = object.keys().collect::<Vec<_>>();
            keys.sort_unstable();
            format!(
                "{{{}}}",
                keys.into_iter()
                    .map(|key| format!(
                        "{}: {}",
                        serde_json::to_string(key).unwrap_or_default(),
                        sorted_json_unicode(&object[key])
                    ))
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        }
        Value::String(text) => serde_json::to_string(text).unwrap_or_default(),
        Value::Number(number) => crate::python_value::python_number(number),
        Value::Bool(value) => value.to_string(),
        Value::Null => "null".into(),
    }
}

fn reasoning_details_text_chars(value: &Value) -> usize {
    match value {
        Value::String(text) => char_len(text),
        Value::Object(_) => reasoning_details_text_chars(&Value::Array(vec![value.clone()])),
        Value::Array(parts) => parts.iter().fold(0usize, |total, part| {
            let length = match part {
                Value::String(text) => char_len(text),
                Value::Object(object) => ["thinking", "text", "summary"]
                    .iter()
                    .filter_map(|key| object.get(*key).and_then(Value::as_str))
                    .map(char_len)
                    .sum(),
                _ => 0,
            };
            total.saturating_add(length)
        }),
        _ => 0,
    }
}

// ---------------------------------------------------------------------------
// Index / accessors
// ---------------------------------------------------------------------------

fn role(msg: &CompressionHistoryMessage) -> &str {
    msg.message.role.as_str()
}

/// Decode a message's clean content into a JSON value (string, array, object).
fn decoded_content(msg: &CompressionHistoryMessage) -> Value {
    msg.message.model_content()
}

/// Return the plain-string content of a message, or `None` when the content is
/// structured (multimodal list or `_multimodal` envelope) or otherwise not a
/// bare string. Mirrors Python's `isinstance(content, str)` gate.
fn string_content(msg: &CompressionHistoryMessage) -> Option<String> {
    match decoded_content(msg) {
        Value::String(s) => Some(s),
        _ => None,
    }
}

/// Overwrite the clean content with a plain string. Leaves `api_content`
/// untouched, matching Python's summarize/dedup passes (only the image-strip
/// path drops the sidecar).
fn set_content(msg: &mut CompressionHistoryMessage, content: String) {
    msg.message.content = content;
}

/// Build tool_call_id -> (name, arguments) from every assistant message.
fn build_call_id_index(
    messages: &[CompressionHistoryMessage],
) -> HashMap<String, (String, String)> {
    let mut index: HashMap<String, (String, String)> = HashMap::new();
    for msg in messages {
        if role(msg) != "assistant" {
            continue;
        }
        for tc in parse_tool_calls(msg) {
            let Some(obj) = tc.as_object() else { continue };
            let cid = obj
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let (name, args) = function_name_and_args(obj);
            index.insert(cid, (name, args));
        }
    }
    index
}

/// Parse the `tool_calls` JSON text field into an array of values (empty on
/// absent / malformed / non-array, matching Python's `msg.get("tool_calls") or []`).
fn parse_tool_calls(msg: &CompressionHistoryMessage) -> Vec<Value> {
    let Some(raw) = msg.tool_calls.as_deref() else {
        return Vec::new();
    };
    match serde_json::from_str::<Value>(raw) {
        Ok(Value::Array(items)) => items,
        _ => Vec::new(),
    }
}

/// Pull `function.name` (default "unknown") and `function.arguments` (default
/// "") from a tool_call object.
fn function_name_and_args(tc: &serde_json::Map<String, Value>) -> (String, String) {
    let fnobj = tc.get("function").and_then(Value::as_object);
    let name = fnobj
        .and_then(|f| f.get("name"))
        .and_then(Value::as_str)
        .unwrap_or("unknown")
        .to_string();
    // arguments is itself a JSON-encoded string on the wire.
    let args = fnobj
        .and_then(|f| f.get("arguments"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    (name, args)
}

// ---------------------------------------------------------------------------
// Pass 2: demotion
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
fn demote_tool_result_at(
    result: &mut [CompressionHistoryMessage],
    idx: usize,
    call_id_to_tool: &HashMap<String, (String, String)>,
    protected_skills: &HashSet<String>,
    min_prune_chars: usize,
    spare_protected_skills: bool,
    pruned: &mut usize,
) -> bool {
    if role(&result[idx]) != "tool" {
        return false;
    }
    let decoded = decoded_content(&result[idx]);

    // Image-bearing shapes share the Pass 3.5 strip policy (and drop the stale
    // api_content sidecar on rewrite).
    if is_image_bearing(&decoded) {
        if strip_images_from_tool_msg(&mut result[idx], &decoded) {
            *pruned += 1;
            return true;
        }
        return false;
    }

    // Only plain-string content past here.
    let Value::String(content) = decoded else {
        return false;
    };
    if content.is_empty() || content == PRUNED_TOOL_PLACEHOLDER {
        return false;
    }
    if content.starts_with("[Duplicate tool output") {
        return false;
    }
    // Already replaced by a prior prune/pressure pass (1-line summary).
    if content.starts_with('[') && content.contains(" chars)") && char_len(&content) < 400 {
        return false;
    }
    if content.starts_with("[screenshot removed") {
        return false;
    }
    if char_len(&content) <= min_prune_chars {
        return false;
    }

    let call_id = result[idx].tool_call_id.as_deref().unwrap_or("");
    let (tool_name, tool_args) = call_id_to_tool
        .get(call_id)
        .cloned()
        .unwrap_or_else(|| ("unknown".to_string(), String::new()));

    if spare_protected_skills && tool_name == "skill_view" && !protected_skills.is_empty() {
        let skill = parse_args(&tool_args)
            .as_object()
            .and_then(|o| o.get("name"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        if !skill.is_empty() && protected_skills.contains(&skill.to_lowercase()) {
            return false;
        }
    }

    let summary = summarize_tool_result(&tool_name, &tool_args, &content);
    set_content(&mut result[idx], summary);
    *pruned += 1;
    true
}

// ---------------------------------------------------------------------------
// Pass 3: tool_call argument truncation
// ---------------------------------------------------------------------------

/// Shrink oversized tool_call argument payloads on an assistant message.
/// Returns true when the message was modified.
fn truncate_tool_call_args_at(result: &mut [CompressionHistoryMessage], idx: usize) -> bool {
    if role(&result[idx]) != "assistant" {
        return false;
    }
    let calls = parse_tool_calls(&result[idx]);
    if calls.is_empty() {
        return false;
    }
    let mut modified = false;
    let mut new_calls: Vec<Value> = Vec::with_capacity(calls.len());
    for tc in calls {
        let mut tc = tc;
        if let Some(obj) = tc.as_object_mut() {
            if let Some(func) = obj.get_mut("function").and_then(Value::as_object_mut) {
                // Clone the arguments string out first so the immutable borrow
                // ends before the mutating insert.
                let current = match func.get("arguments") {
                    Some(Value::String(args)) => Some(args.clone()),
                    _ => None,
                };
                if let Some(args) = current {
                    // Python gate: only args strings longer than 500 chars.
                    if char_len(&args) > 500 {
                        let new_args = truncate_tool_call_args_json(&args, 200);
                        if new_args != args {
                            func.insert("arguments".to_string(), Value::String(new_args));
                            modified = true;
                        }
                    }
                }
            }
        }
        new_calls.push(tc);
    }
    if modified {
        // Re-serialize the whole tool_calls array back into the text column.
        if let Ok(serialized) = serde_json::to_string(&Value::Array(new_calls)) {
            result[idx].tool_calls = Some(serialized);
        } else {
            return false;
        }
    }
    modified
}

/// Shrink long string leaves inside a tool-call arguments JSON blob while
/// preserving JSON validity. Mirrors `_truncate_tool_call_args_json`.
///
/// Non-JSON argument strings are returned unchanged (some backends use non-JSON
/// tool arguments). Non-string leaves are preserved intact.
fn truncate_tool_call_args_json(args: &str, head_chars: usize) -> String {
    let parsed: Value = match serde_json::from_str(args) {
        Ok(v) => v,
        Err(_) => return args.to_string(),
    };
    let shrunken = shrink_json(&parsed, head_chars);
    // ensure_ascii=False parity: serde_json does not escape non-ASCII.
    serde_json::to_string(&shrunken).unwrap_or_else(|_| args.to_string())
}

fn shrink_json(value: &Value, head_chars: usize) -> Value {
    match value {
        Value::String(s) => {
            if char_len(s) > head_chars {
                Value::String(format!("{}...[truncated]", char_truncate(s, head_chars)))
            } else {
                value.clone()
            }
        }
        Value::Object(map) => {
            let mut out = serde_json::Map::with_capacity(map.len());
            for (k, v) in map {
                out.insert(k.clone(), shrink_json(v, head_chars));
            }
            Value::Object(out)
        }
        Value::Array(items) => {
            Value::Array(items.iter().map(|v| shrink_json(v, head_chars)).collect())
        }
        other => other.clone(),
    }
}

// ---------------------------------------------------------------------------
// Pass 3.5: image retirement
// ---------------------------------------------------------------------------

/// Replace image payloads on older tool results with text placeholders. Keeps
/// the newest `keep_newest` image-bearing tool messages intact. Mutates
/// `result` in place; returns the number of messages rewritten.
fn retire_stale_tool_result_images(
    result: &mut [CompressionHistoryMessage],
    keep_newest: usize,
) -> usize {
    let mut seen = 0usize;
    let mut pruned = 0usize;
    for i in (0..result.len()).rev() {
        if role(&result[i]) != "tool" {
            continue;
        }
        let decoded = decoded_content(&result[i]);
        if !tool_content_has_images(&decoded) {
            continue;
        }
        seen += 1;
        if seen <= keep_newest {
            continue;
        }
        if strip_images_from_tool_msg(&mut result[i], &decoded) {
            pruned += 1;
        }
    }
    pruned
}

/// True when a tool-result body carries embedded image bytes. Handles both the
/// unwrapped part list and the `{_multimodal: true, content: [...]}` envelope.
fn tool_content_has_images(content: &Value) -> bool {
    if let Value::Object(map) = content {
        if map.get("_multimodal").and_then(Value::as_bool) == Some(true) {
            return content_has_images(map.get("content"));
        }
    }
    content_has_images(Some(content))
}

fn content_has_images(content: Option<&Value>) -> bool {
    matches!(content, Some(Value::Array(parts)) if parts.iter().any(is_image_part))
}

fn is_image_part(part: &Value) -> bool {
    matches!(
        part.get("type").and_then(Value::as_str),
        Some("image_url") | Some("input_image") | Some("image")
    )
}

/// Replace a tool message's image payloads with text placeholders. Returns true
/// when the message was rewritten. Drops the stale `api_content` sidecar so
/// replay cannot resend the pre-rewrite bytes.
fn strip_images_from_tool_msg(msg: &mut CompressionHistoryMessage, decoded: &Value) -> bool {
    // Envelope shape collapses to a short string.
    if let Value::Object(map) = decoded {
        if map.get("_multimodal").and_then(Value::as_bool) == Some(true) {
            let summary = map
                .get("text_summary")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .unwrap_or("[screenshot removed to save context]");
            let new_content = format!("[screenshot removed] {}", char_truncate(summary, 200));
            msg.message.content = new_content;
            msg.message.api_content = None;
            return true;
        }
    }
    // Part-list shape: swap image parts for text placeholders.
    let Value::Array(parts) = decoded else {
        return false;
    };
    let mut had_image = false;
    let mut out: Vec<Value> = Vec::with_capacity(parts.len());
    for part in parts {
        if is_image_part(part) {
            had_image = true;
            out.push(json!({"type": "text", "text": "[screenshot removed to save context]"}));
        } else {
            out.push(part.clone());
        }
    }
    if !had_image {
        return false;
    }
    msg.message.content = encode_message_content(&Value::Array(out));
    msg.message.api_content = None;
    true
}

fn is_image_bearing(decoded: &Value) -> bool {
    match decoded {
        Value::Array(_) => tool_content_has_images(decoded),
        Value::Object(map) => map.get("_multimodal").and_then(Value::as_bool) == Some(true),
        _ => false,
    }
}

// ---------------------------------------------------------------------------
// Skill protection
// ---------------------------------------------------------------------------

/// Skill names whose skill_view bodies must survive Pass-2 demotion (lower-cased).
fn collect_protected_skill_names(
    messages: &[CompressionHistoryMessage],
    prune_boundary: usize,
) -> HashSet<String> {
    let total = messages.len();
    if total == 0 {
        return HashSet::new();
    }
    let recent_start = total.saturating_sub(SKILL_PRUNE_RECENT_WINDOW);
    let tail_start = prune_boundary;

    let mut tail_user_texts: Vec<String> = Vec::new();
    for msg in messages.iter().skip(tail_start) {
        if role(msg) != "user" {
            continue;
        }
        if let Value::String(text) = decoded_content(msg) {
            if !text.is_empty() {
                tail_user_texts.push(text.to_lowercase());
            }
        }
    }

    let mut protected: HashSet<String> = HashSet::new();
    for (idx, skill) in skill_view_call_sites(messages) {
        let key = skill.to_lowercase();
        if idx >= recent_start
            || idx >= tail_start
            || tail_user_texts.iter().any(|t| t.contains(&key))
        {
            protected.insert(key);
        }
    }
    protected
}

/// Yield `(message_index, skill_name)` for every skill_view tool call.
fn skill_view_call_sites(messages: &[CompressionHistoryMessage]) -> Vec<(usize, String)> {
    let mut sites: Vec<(usize, String)> = Vec::new();
    for (i, msg) in messages.iter().enumerate() {
        if role(msg) != "assistant" {
            continue;
        }
        for tc in parse_tool_calls(msg) {
            let Some(obj) = tc.as_object() else { continue };
            let (name, args) = function_name_and_args(obj);
            if name != "skill_view" || args.is_empty() {
                continue;
            }
            if let Some(skill) = parse_args(&args)
                .as_object()
                .and_then(|o| o.get("name"))
                .and_then(Value::as_str)
            {
                if !skill.is_empty() {
                    sites.push((i, skill.to_string()));
                }
            }
        }
    }
    sites
}

// ---------------------------------------------------------------------------
// Summarization
// ---------------------------------------------------------------------------

/// Parse a tool-call arguments string into a JSON value, defaulting to an empty
/// object on absent / malformed / non-object input (mirrors Python's guard).
fn parse_args(args: &str) -> Value {
    if args.is_empty() {
        return json!({});
    }
    match serde_json::from_str::<Value>(args) {
        Ok(v @ Value::Object(_)) => v,
        _ => json!({}),
    }
}

/// Informative 1-line summary of a tool call + result. Never panics; on any
/// internal issue it would fall back to the generic shape, matching the Python
/// guard wrapper (`_summarize_tool_result`).
pub fn summarize_tool_result(tool_name: &str, tool_args: &str, tool_content: &str) -> String {
    let args = parse_args(tool_args);
    let obj = args.as_object();
    let get_str = |key: &str, default: &str| -> String {
        obj.and_then(|o| o.get(key))
            .map(json_value_to_str)
            .unwrap_or_else(|| default.to_string())
    };

    let content = tool_content;
    let content_len = char_len(content);
    let line_count = if content.trim().is_empty() {
        0
    } else {
        content.matches('\n').count() + 1
    };

    match tool_name {
        "terminal" => {
            let mut cmd = str_arg(obj, "command", "");
            if char_len(&cmd) > 80 {
                cmd = format!("{}...", char_truncate(&cmd, 77));
            }
            let exit_code =
                extract_json_int(content, "exit_code", true).unwrap_or_else(|| "?".to_string());
            format!("[terminal] ran `{cmd}` -> exit {exit_code}, {line_count} lines output")
        }
        "read_file" => {
            let path = get_str("path", "?");
            let offset = obj
                .and_then(|o| o.get("offset"))
                .map(json_value_to_str)
                .unwrap_or_else(|| "1".to_string());
            format!(
                "[read_file] read {path} from line {offset} ({} chars)",
                comma(content_len)
            )
        }
        "write_file" => {
            let path = get_str("path", "?");
            let content_arg = obj.and_then(|o| o.get("content"));
            let written_lines = match content_arg {
                Some(v) if is_truthy(v) => {
                    (str_arg(obj, "content", "").matches('\n').count() + 1).to_string()
                }
                _ => "?".to_string(),
            };
            format!("[write_file] wrote to {path} ({written_lines} lines)")
        }
        "search_files" => {
            let pattern = get_str("pattern", "?");
            let path = get_str("path", ".");
            let target = get_str("target", "content");
            let count =
                extract_json_int(content, "total_count", false).unwrap_or_else(|| "?".to_string());
            format!("[search_files] {target} search for '{pattern}' in {path} -> {count} matches")
        }
        "patch" => {
            let path = get_str("path", "?");
            let mode = get_str("mode", "replace");
            format!(
                "[patch] {mode} in {path} ({} chars result)",
                comma(content_len)
            )
        }
        "browser_navigate" | "browser_click" | "browser_snapshot" | "browser_type"
        | "browser_scroll" | "browser_vision" => {
            let url = get_str_opt(obj, "url");
            let reference = get_str_opt(obj, "ref");
            let detail = if let Some(u) = url {
                format!(" {u}")
            } else if let Some(r) = reference {
                format!(" ref={r}")
            } else {
                String::new()
            };
            format!("[{tool_name}]{detail} ({} chars)", comma(content_len))
        }
        "web_search" => {
            let query = get_str("query", "?");
            format!(
                "[web_search] query='{query}' ({} chars result)",
                comma(content_len)
            )
        }
        "web_extract" => {
            let urls = obj.and_then(|o| o.get("urls"));
            let (first, extra) = web_extract_first(urls);
            let mut url_desc = first;
            if extra > 0 {
                url_desc.push_str(&format!(" (+{extra} more)"));
            }
            format!("[web_extract] {url_desc} ({} chars)", comma(content_len))
        }
        "delegate_task" => {
            let mut goal = str_arg(obj, "goal", "");
            if char_len(&goal) > 60 {
                goal = format!("{}...", char_truncate(&goal, 57));
            }
            format!(
                "[delegate_task] '{goal}' ({} chars result)",
                comma(content_len)
            )
        }
        "execute_code" => {
            let code = str_arg(obj, "code", "");
            let mut preview = char_truncate(&code, 60).replace('\n', " ");
            if char_len(&code) > 60 {
                preview.push_str("...");
            }
            format!("[execute_code] `{preview}` ({line_count} lines output)")
        }
        "skill_view" => {
            let name = get_str("name", "?");
            if content_len > SKILL_VIEW_PRUNE_MIN_CHARS {
                format!(
                    "[skill_view] name={name} ({} chars) {}",
                    comma(content_len),
                    skill_pruned_marker(&name)
                )
            } else {
                format!("[skill_view] name={name} ({} chars)", comma(content_len))
            }
        }
        "skills_list" | "skill_manage" => {
            let name = get_str("name", "?");
            format!("[{tool_name}] name={name} ({} chars)", comma(content_len))
        }
        "vision_analyze" => {
            let question = char_truncate(&str_arg(obj, "question", ""), 50);
            format!(
                "[vision_analyze] '{question}' ({} chars)",
                comma(content_len)
            )
        }
        "memory" => {
            let action = get_str("action", "?");
            let target = get_str("target", "?");
            format!("[memory] {action} on {target}")
        }
        "todo_list" => "[todo] updated task list".to_string(),
        "clarify" => summarize_clarify(content),
        "text_to_speech" => {
            format!(
                "[text_to_speech] generated audio ({} chars)",
                comma(content_len)
            )
        }
        "cronjob_manage" => {
            let action = get_str("action", "?");
            format!("[cronjob] {action}")
        }
        "process_manage" => {
            let action = get_str("action", "?");
            let sid = get_str("session_id", "?");
            format!("[process] {action} session={sid}")
        }
        _ => {
            // Generic fallback: first two args, values coerced to Python-ish str.
            let mut first_arg = String::new();
            if let Some(map) = obj {
                for (k, v) in map.iter().take(2) {
                    let sv = char_truncate(&json_value_to_str(v), 40);
                    first_arg.push_str(&format!(" {k}={sv}"));
                }
            }
            format!(
                "[{tool_name}]{first_arg} ({} chars result)",
                comma(content_len)
            )
        }
    }
}

fn summarize_clarify(content: &str) -> String {
    const PREFIX: &str = "[clarify] user responded: ";
    // One char under PRUNE_MIN_CHARS so the summary stays out of later
    // dedup/summarize passes.
    let max_summary_chars = PRUNE_MIN_CHARS - 1;
    const TRUNCATION_MARKER: &str = "...[truncated]";

    let parsed: Value = serde_json::from_str(content).unwrap_or(Value::Null);
    let response = parsed.as_object().and_then(|o| o.get("user_response"));

    let is_answer_shaped = match response {
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Array(items)) => {
            !items.is_empty()
                && items
                    .iter()
                    .all(|it| matches!(it, Value::String(s) if !s.is_empty()))
        }
        _ => false,
    };
    let resolved = is_answer_shaped
        && !is_clarify_non_response_sentinel(response.expect("answer-shaped implies Some"));

    if resolved {
        let serialized = serialize_clarify_response(response.unwrap());
        let mut summary = format!("{PREFIX}{serialized}");
        if char_len(&summary) > max_summary_chars {
            let keep = max_summary_chars - TRUNCATION_MARKER.len();
            summary = format!(
                "{}{}",
                char_truncate(&summary, keep).trim_end(),
                TRUNCATION_MARKER
            );
        }
        return summary;
    }
    "[clarify] asked user a question".to_string()
}

/// Serialize a clarify user_response the way Python's
/// `json.dumps(response, ensure_ascii=False)` would for the answer-shaped
/// domain (a string, or a list of non-empty strings). Lists use Python's
/// default `", "` item separator.
fn serialize_clarify_response(response: &Value) -> String {
    match response {
        Value::String(_) => serde_json::to_string(response).unwrap_or_default(),
        Value::Array(items) => {
            let parts: Vec<String> = items
                .iter()
                .map(|it| serde_json::to_string(it).unwrap_or_default())
                .collect();
            format!("[{}]", parts.join(", "))
        }
        other => serde_json::to_string(other).unwrap_or_default(),
    }
}

const CLARIFY_NON_RESPONSE_PREFIXES: [&str; 4] = [
    "The user did not provide a response",
    "[user did not respond",
    "[clarify prompt could not be delivered",
    "[oneshot mode:",
];

fn is_clarify_non_response_sentinel(response: &Value) -> bool {
    let is_sentinel = |s: &str| {
        let trimmed = s.trim_start();
        CLARIFY_NON_RESPONSE_PREFIXES
            .iter()
            .any(|p| trimmed.starts_with(p))
    };
    match response {
        Value::String(s) => is_sentinel(s),
        Value::Array(items) => items
            .iter()
            .any(|it| matches!(it, Value::String(s) if is_sentinel(s))),
        _ => false,
    }
}

fn skill_pruned_marker(skill_name: &str) -> String {
    format!(
        "{SKILL_PRUNED_MARKER_PREFIX} content lost in compression; \
reload with skill_view(name='{skill_name}')]"
    )
}

// ---------------------------------------------------------------------------
// Small value helpers
// ---------------------------------------------------------------------------

/// Coerce a JSON value to a string the way Python `_str_arg` does: strings pass
/// through raw; anything else is `str()`-coerced (`None` -> the caller's
/// default is applied separately).
fn str_arg(obj: Option<&serde_json::Map<String, Value>>, key: &str, default: &str) -> String {
    match obj.and_then(|o| o.get(key)) {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Null) | None => default.to_string(),
        Some(other) => json_value_to_str(other),
    }
}

fn get_str_opt(obj: Option<&serde_json::Map<String, Value>>, key: &str) -> Option<String> {
    match obj.and_then(|o| o.get(key)) {
        Some(Value::String(s)) if !s.is_empty() => Some(s.clone()),
        _ => None,
    }
}

/// Python-ish `str()` of a JSON value, used only where the reference calls
/// `str(v)` on model-supplied argument values.
fn json_value_to_str(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Bool(true) => "True".to_string(),
        Value::Bool(false) => "False".to_string(),
        Value::Null => "None".to_string(),
        Value::Number(n) => n.to_string(),
        other => other.to_string(),
    }
}

/// Python truthiness for the write_file `content` gate: non-empty string,
/// non-empty container, non-zero number, true.
fn is_truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().map(|f| f != 0.0).unwrap_or(true),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

/// Resolve web_extract's first URL and the count of extra URLs, unwrapping the
/// `{url|href: ...}` dict shape models forward from web_search.
fn web_extract_first(urls: Option<&Value>) -> (String, usize) {
    let Some(Value::Array(list)) = urls else {
        return ("?".to_string(), 0);
    };
    let extra = list.len().saturating_sub(1);
    let first = match list.first() {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Object(map)) => map
            .get("url")
            .or_else(|| map.get("href"))
            .and_then(Value::as_str)
            .unwrap_or("?")
            .to_string(),
        _ => "?".to_string(),
    };
    (first, extra)
}

/// Extract the first JSON integer field value matching `"field"\s*:\s*(-?\d+)`
/// anywhere in `content`. Returns the numeric substring (with sign when
/// `allow_negative`). Manual scan; no regex dependency.
fn extract_json_int(content: &str, field: &str, allow_negative: bool) -> Option<String> {
    let needle = format!("\"{field}\"");
    let bytes = content.as_bytes();
    let mut search_from = 0usize;
    while let Some(rel) = content[search_from..].find(&needle) {
        let mut pos = search_from + rel + needle.len();
        // skip whitespace
        while pos < bytes.len() && bytes[pos].is_ascii_whitespace() {
            pos += 1;
        }
        if pos < bytes.len() && bytes[pos] == b':' {
            pos += 1;
            while pos < bytes.len() && bytes[pos].is_ascii_whitespace() {
                pos += 1;
            }
            let start = pos;
            if allow_negative && pos < bytes.len() && bytes[pos] == b'-' {
                pos += 1;
            }
            let digits_start = pos;
            while pos < bytes.len() && bytes[pos].is_ascii_digit() {
                pos += 1;
            }
            if pos > digits_start {
                return Some(content[start..pos].to_string());
            }
        }
        // No valid number here; continue searching after this occurrence.
        search_from += rel + needle.len();
    }
    None
}

/// Char count (Unicode scalar values), matching Python `len(str)`.
fn char_len(s: &str) -> usize {
    s.chars().count()
}

/// First `n` chars (Unicode scalar values) of `s`, matching Python `s[:n]`.
fn char_truncate(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

/// Render a non-negative integer with `,` thousands separators, matching
/// Python's `f"{n:,}"`.
fn comma(n: usize) -> String {
    let digits = n.to_string();
    let bytes = digits.as_bytes();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    let len = bytes.len();
    for (i, b) in bytes.iter().enumerate() {
        if i > 0 && (len - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(*b as char);
    }
    out
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session_db::HistoryMessage;

    fn msg(
        id: i64,
        role: &str,
        content: &str,
        tool_call_id: Option<&str>,
        tool_calls: Option<&str>,
    ) -> CompressionHistoryMessage {
        CompressionHistoryMessage {
            id,
            message: HistoryMessage {
                role: role.to_string(),
                content: content.to_string(),
                api_content: None,
            },
            tool_call_id: tool_call_id.map(str::to_string),
            tool_calls: tool_calls.map(str::to_string),
            tool_name: None,
            reasoning: None,
            reasoning_content: None,
            reasoning_details: None,
            codex_reasoning_items: None,
            codex_message_items: None,
            compressed_summary: false,
        }
    }

    fn tool_row(id: i64, call_id: &str, content: &str) -> CompressionHistoryMessage {
        msg(id, "tool", content, Some(call_id), None)
    }

    fn assistant_call(
        id: i64,
        call_id: &str,
        name: &str,
        arguments: &str,
    ) -> CompressionHistoryMessage {
        let tc = json!([{
            "id": call_id,
            "type": "function",
            "function": {"name": name, "arguments": arguments}
        }])
        .to_string();
        msg(id, "assistant", "", None, Some(&tc))
    }

    fn big(prefix: &str, n: usize) -> String {
        let mut s = String::from(prefix);
        while s.len() < n {
            s.push('x');
        }
        s
    }

    #[test]
    fn no_op_input_returns_unchanged() {
        let messages = vec![
            msg(1, "user", "hi", None, None),
            msg(2, "assistant", "hello there", None, None),
        ];
        let out = prune_old_tool_results(&messages, 20, PRUNE_MIN_CHARS);
        assert!(!out.changed);
        assert_eq!(out.pruned_count, 0);
        assert_eq!(out.reclaimed_chars, 0);
        assert_eq!(out.messages, messages);
    }

    #[test]
    fn empty_input_is_noop() {
        let out = prune_old_tool_results(&[], 5, PRUNE_MIN_CHARS);
        assert!(!out.changed);
        assert!(out.messages.is_empty());
    }

    #[test]
    fn protected_tail_is_untouched() {
        // Two large read_file results; protect the last 2 messages so only the
        // first tool result is prunable.
        let content = big("FILE:", 500);
        let messages = vec![
            assistant_call(1, "c1", "read_file", r#"{"path":"a.txt"}"#),
            tool_row(2, "c1", &content),
            assistant_call(3, "c2", "read_file", r#"{"path":"b.txt"}"#),
            tool_row(4, "c2", &content), // different call, same content -> dedup target
        ];
        // protect_tail_count large enough to protect both tool rows from Pass 2.
        let out = prune_old_tool_results(&messages, 4, PRUNE_MIN_CHARS);
        // Pass 1 (tail-agnostic) still dedups the OLDER identical copy (row 2),
        // keeping the newest (row 4). Pass 2 is fully suppressed by the tail.
        assert_eq!(out.messages[1].message.content, DUPLICATE_BACKREF);
        assert_eq!(out.messages[3].message.content, content);
        // Newest identical copy untouched; older summarized-away? No: it was
        // deduped, not summarized, because dedup runs before the tail guard.
        assert!(out.changed);
    }

    #[test]
    fn duplicate_outputs_keep_newest_full_copy() {
        let content = big("DUP:", 400);
        let messages = vec![
            tool_row(1, "c1", &content),
            tool_row(2, "c2", &content),
            tool_row(3, "c3", &content),
        ];
        let out = prune_old_tool_results(&messages, 0, PRUNE_MIN_CHARS);
        assert_eq!(out.messages[0].message.content, DUPLICATE_BACKREF);
        assert_eq!(out.messages[1].message.content, DUPLICATE_BACKREF);
        // Newest copy is preserved by dedup itself (it is the "seen" anchor);
        // with protect_tail_count=0 it is then eligible for Pass 2 summary, but
        // read/tool name is unknown here so it becomes a generic summary.
        // Assert it is no longer the full duplicate string.
        assert_ne!(out.messages[2].message.content, DUPLICATE_BACKREF);
        assert!(out.pruned_count >= 2);
    }

    #[test]
    fn dedup_below_floor_is_skipped() {
        let content = "short repeated"; // < 200 chars
        let messages = vec![tool_row(1, "c1", content), tool_row(2, "c2", content)];
        let out = prune_old_tool_results(&messages, 0, PRUNE_MIN_CHARS);
        assert!(!out.changed);
    }

    #[test]
    fn large_old_tool_result_is_summarized() {
        let content = format!("{}\n{}", big("line1", 300), "line2");
        let messages = vec![
            assistant_call(1, "c1", "terminal", r#"{"command":"npm test"}"#),
            tool_row(2, "c1", &format!("{content}\n\"exit_code\": 0")),
            // tail
            msg(3, "user", "next", None, None),
            msg(4, "assistant", "ok", None, None),
        ];
        let out = prune_old_tool_results(&messages, 2, PRUNE_MIN_CHARS);
        let summary = &out.messages[1].message.content;
        assert!(summary.starts_with("[terminal] ran `npm test` -> exit 0,"));
        assert!(summary.ends_with("lines output"));
        assert!(out.reclaimed_chars > 0);
        assert!(out.pruned_count >= 1);
    }

    #[test]
    fn tool_call_identity_drives_summary() {
        // Two tool results, correct summaries come from matching call ids.
        let body = big("BODY:", 300);
        let messages = vec![
            assistant_call(1, "read1", "read_file", r#"{"path":"cfg.py","offset":5}"#),
            tool_row(2, "read1", &body),
            assistant_call(
                3,
                "srch1",
                "search_files",
                r#"{"pattern":"compress","path":"agent/","target":"content"}"#,
            ),
            tool_row(4, "srch1", &format!("{body}\n\"total_count\": 12")),
            msg(5, "user", "tail", None, None),
        ];
        let out = prune_old_tool_results(&messages, 1, PRUNE_MIN_CHARS);
        assert_eq!(
            out.messages[1].message.content,
            format!(
                "[read_file] read cfg.py from line 5 ({} chars)",
                comma(char_len(&body))
            )
        );
        assert_eq!(
            out.messages[3].message.content,
            "[search_files] content search for 'compress' in agent/ -> 12 matches"
        );
    }

    #[test]
    fn structured_image_content_is_retired() {
        // Four image-bearing tool results; keep newest MAX_KEEP_TOOL_IMAGES (3),
        // retire the oldest one.
        let img_content = encode_message_content(&json!([
            {"type": "text", "text": "screenshot"},
            {"type": "image_url", "image_url": {"url": "data:image/png;base64,AAAA"}}
        ]));
        let mut messages = Vec::new();
        for i in 0..4 {
            messages.push(tool_row(i as i64 + 1, &format!("c{i}"), &img_content));
        }
        let out = prune_old_tool_results(&messages, 20, PRUNE_MIN_CHARS);
        // Oldest (index 0) retired to placeholder; newest 3 kept.
        let decoded0 = out.messages[0].message.model_content();
        assert!(!tool_content_has_images(&decoded0));
        for row in out.messages.iter().skip(1) {
            assert!(tool_content_has_images(&row.message.model_content()));
        }
        assert_eq!(out.pruned_count, 1);
    }

    #[test]
    fn multimodal_envelope_is_collapsed() {
        let envelope = encode_message_content(&json!({
            "_multimodal": true,
            "text_summary": "a chart",
            "content": [
                {"type": "image", "source": {"data": "AAAA"}}
            ]
        }));
        let messages = vec![
            tool_row(1, "c1", &envelope),
            tool_row(2, "c2", &envelope),
            tool_row(3, "c3", &envelope),
            tool_row(4, "c4", &envelope),
        ];
        let out = prune_old_tool_results(&messages, 20, PRUNE_MIN_CHARS);
        assert_eq!(
            out.messages[0].message.content,
            "[screenshot removed] a chart"
        );
        assert!(out.messages[0].message.api_content.is_none());
    }

    #[test]
    fn tool_call_args_truncated_outside_tail() {
        let long_value = big("PATH:", 900);
        let arguments = json!({"path": long_value, "mode": "replace"}).to_string();
        assert!(arguments.len() > 500);
        let messages = vec![
            assistant_call(1, "w1", "write_file", &arguments),
            msg(2, "tool", "wrote", Some("w1"), None),
            msg(3, "user", "tail", None, None),
        ];
        let out = prune_old_tool_results(&messages, 1, PRUNE_MIN_CHARS);
        let new_calls = out.messages[0].tool_calls.as_ref().unwrap();
        assert!(new_calls.contains("...[truncated]"));
        // Still valid JSON and still valid tool-call shape.
        let parsed: Value = serde_json::from_str(new_calls).unwrap();
        let inner = parsed[0]["function"]["arguments"].as_str().unwrap();
        let inner_json: Value = serde_json::from_str(inner).unwrap();
        assert_eq!(inner_json["mode"], "replace");
        assert!(inner_json["path"]
            .as_str()
            .unwrap()
            .ends_with("...[truncated]"));
    }

    #[test]
    fn skill_view_body_protected_when_recently_loaded() {
        let big_skill = big("SKILL:", 6000);
        let messages = vec![
            assistant_call(1, "s1", "skill_view", r#"{"name":"device42"}"#),
            tool_row(2, "s1", &big_skill),
            msg(3, "user", "tail", None, None),
        ];
        // prune_boundary excludes tail; but the skill_view is within the recent
        // window so its body must survive.
        let out = prune_old_tool_results(&messages, 1, PRUNE_MIN_CHARS);
        assert_eq!(out.messages[1].message.content, big_skill);
    }

    #[test]
    fn skill_view_demoted_with_marker_when_not_protected() {
        let big_skill = big("SKILL:", 6000);
        // Pad the front so the skill_view sits OUTSIDE the recent window (10)
        // and outside the protected tail, with no tail user mention.
        let mut messages = vec![
            assistant_call(1, "s1", "skill_view", r#"{"name":"device42"}"#),
            tool_row(2, "s1", &big_skill),
        ];
        for i in 0..12 {
            messages.push(msg(100 + i, "assistant", "filler turn text", None, None));
        }
        let out = prune_old_tool_results(&messages, 2, PRUNE_MIN_CHARS);
        let summary = &out.messages[1].message.content;
        assert!(summary.starts_with("[skill_view] name=device42"));
        assert!(summary.contains(SKILL_PRUNED_MARKER_PREFIX));
    }

    #[test]
    fn idempotent_second_pass_is_noop() {
        let body = format!("{}\n\"exit_code\": 0", big("OUT:", 400));
        let messages = vec![
            assistant_call(1, "c1", "terminal", r#"{"command":"ls"}"#),
            tool_row(2, "c1", &body),
            assistant_call(3, "c2", "read_file", r#"{"path":"x.py"}"#),
            tool_row(4, "c2", &big("READ:", 500)),
            msg(5, "user", "tail", None, None),
        ];
        let first = prune_old_tool_results(&messages, 1, PRUNE_MIN_CHARS);
        assert!(first.changed);
        let second = prune_old_tool_results(&first.messages, 1, PRUNE_MIN_CHARS);
        assert!(!second.changed, "second pass should be a no-op");
        assert_eq!(second.pruned_count, 0);
        assert_eq!(second.messages, first.messages);
    }

    #[test]
    fn input_is_never_mutated() {
        let content = big("DUP:", 400);
        let messages = vec![tool_row(1, "c1", &content), tool_row(2, "c2", &content)];
        let snapshot = messages.clone();
        let _ = prune_old_tool_results(&messages, 0, PRUNE_MIN_CHARS);
        assert_eq!(messages, snapshot, "input list must be untouched");
    }

    #[test]
    fn clarify_answer_is_quoted() {
        let content = json!({"user_response": "yes, proceed with option B"}).to_string();
        let out = summarize_tool_result("clarify", "{}", &content);
        assert_eq!(
            out,
            "[clarify] user responded: \"yes, proceed with option B\""
        );
    }

    #[test]
    fn clarify_sentinel_is_generic() {
        let content = json!({"user_response": "[user did not respond within 5m]"}).to_string();
        let out = summarize_tool_result("clarify", "{}", &content);
        assert_eq!(out, "[clarify] asked user a question");
    }

    #[test]
    fn comma_formatting_matches_python() {
        assert_eq!(comma(0), "0");
        assert_eq!(comma(999), "999");
        assert_eq!(comma(1000), "1,000");
        assert_eq!(comma(1234567), "1,234,567");
    }

    #[test]
    fn extract_json_int_handles_negative_and_missing() {
        assert_eq!(
            extract_json_int("blah \"exit_code\": -1 blah", "exit_code", true),
            Some("-1".to_string())
        );
        assert_eq!(
            extract_json_int("\"total_count\" : 42", "total_count", false),
            Some("42".to_string())
        );
        assert_eq!(extract_json_int("no field here", "exit_code", true), None);
    }

    #[test]
    fn rough_estimator_matches_ascii_cjk_and_utf8_rules() {
        assert_eq!(estimate_tokens_rough(""), 0);
        assert_eq!(estimate_tokens_rough("abcde"), 2);
        assert_eq!(estimate_tokens_rough("漢字"), 2);
        assert_eq!(estimate_tokens_rough("éé"), 1);
        assert_eq!(estimate_tokens_rough("A漢é"), 2);
    }

    #[test]
    fn message_estimator_counts_full_tool_call_and_replay_sidecars() {
        let mut row = assistant_call(1, "call_0123456789", "read_file", r#"{"path":"a"}"#);
        let visible_only = estimate_tokens_rough("") + 10;
        let with_call = estimate_message_budget_tokens(&row, false);
        assert!(
            with_call > visible_only + 10,
            "full call envelope must be charged"
        );

        row.codex_reasoning_items = Some(r#"[{"encrypted_content":"xxxxxxxx"}]"#.into());
        row.codex_message_items = Some(r#"[{"type":"message","text":"界"}]"#.into());
        let with_replay = estimate_message_budget_tokens(&row, false);
        assert!(
            with_replay > with_call,
            "Codex replay sidecars must always be charged"
        );

        row.reasoning = Some("r".repeat(400));
        assert_eq!(estimate_message_budget_tokens(&row, false), with_replay);
        assert_eq!(
            estimate_message_budget_tokens(&row, true),
            with_replay + 100
        );
        row.reasoning_content = Some("c".repeat(800));
        assert_eq!(
            estimate_message_budget_tokens(&row, true),
            with_replay + 200
        );
    }

    #[test]
    fn reasoning_details_charge_text_but_not_signed_envelope() {
        let mut row = msg(1, "assistant", "", None, None);
        row.reasoning_details = Some(
            json!([{
                "thinking": "t".repeat(400),
                "signature": "s".repeat(40_000),
                "data": "d".repeat(40_000)
            }])
            .to_string(),
        );
        assert_eq!(estimate_message_budget_tokens(&row, false), 10);
        assert_eq!(estimate_message_budget_tokens(&row, true), 110);
    }

    #[test]
    fn token_boundary_uses_strict_equality_and_includes_break_row() {
        let rows = vec![
            msg(1, "user", "old", None, None),
            tool_row(2, "c1", &big("TOOL:", 500)),
            msg(3, "assistant", "a", None, None),
            msg(4, "user", "a", None, None),
        ];
        // The last two rows cost exactly 22 tokens. Equality continues the
        // walk, then the oversized tool becomes the protected break row.
        assert_eq!(compute_prune_boundary(&rows, 2, Some(22), false), 1);
        // One token less breaks on the second-newest row instead.
        assert_eq!(compute_prune_boundary(&rows, 2, Some(21), false), 2);
    }

    #[test]
    fn token_boundary_caps_count_floor_at_eight() {
        let rows = (0..20)
            .map(|index| msg(index, "user", "x", None, None))
            .collect::<Vec<_>>();
        assert_eq!(compute_prune_boundary(&rows, 20, Some(1), false), 12);
    }

    #[test]
    fn pressure_pass_demotes_protected_skill_and_keeps_marker() {
        let skill = big("# fresh-skill instructions\n", 60_000);
        let rows = vec![
            msg(1, "user", "filler", None, None),
            msg(2, "assistant", "filler", None, None),
            assistant_call(3, "skill", "skill_view", r#"{"name":"fresh-skill"}"#),
            tool_row(4, "skill", &skill),
            msg(5, "user", "active ask", None, None),
        ];
        let out = prune_old_tool_results_with_budget(&rows, 4, 100, PRUNE_MIN_CHARS, false);
        assert!(out.changed);
        assert!(out.pruned_count >= 1);
        assert!(out.messages[3]
            .message
            .content
            .contains("[SKILL_PRUNED: content lost in compression; reload with skill_view(name='fresh-skill')]"));
    }

    #[test]
    fn pressure_pass_uses_newest_tool_as_absolute_last_resort() {
        let rows = vec![
            assistant_call(1, "a", "read_file", r#"{"path":"a"}"#),
            tool_row(2, "a", &big("A:", 4_000)),
            assistant_call(3, "b", "read_file", r#"{"path":"b"}"#),
            tool_row(4, "b", &big("B:", 40_000)),
            msg(5, "user", "active", None, None),
        ];
        let out = prune_old_tool_results_with_budget(&rows, 5, 100, PRUNE_MIN_CHARS, false);
        assert!(out.messages[1].message.content.starts_with("[read_file]"));
        assert!(out.messages[3].message.content.starts_with("[read_file]"));
        assert_eq!(out.messages[4].message.content, "active");
    }

    #[test]
    fn summary_tail_cut_matches_python_budget_walk_cases() {
        let mut heavy = Vec::new();
        for index in 0..20 {
            let calls = (0..5)
                .map(|_| {
                    json!({
                        "id": format!("call_{index:02}_{}", "a".repeat(24)),
                        "type": "function",
                        "function": {"name": "read_file", "arguments": "{\"path\":\"a\"}"}
                    })
                })
                .collect::<Vec<_>>();
            heavy.push(msg(
                index + 2,
                "assistant",
                "",
                None,
                Some(&Value::Array(calls).to_string()),
            ));
        }
        let mut messages = vec![msg(1, "user", "start", None, None)];
        messages.extend(heavy);
        assert_eq!(find_tail_cut_by_tokens(&messages, 1, 20, 680, false, 1), 12);

        let alternating = (0..12)
            .map(|index| {
                msg(
                    index + 1,
                    if index % 2 == 0 { "user" } else { "assistant" },
                    &format!("m{index}"),
                    None,
                    None,
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(
            find_tail_cut_by_tokens(&alternating, 2, 20, 1_000_000, false, 1),
            5
        );
    }

    #[test]
    fn summary_tail_cut_keeps_complete_tool_group_and_latest_replies() {
        let rows = vec![
            msg(1, "user", "u0", None, None),
            msg(2, "assistant", "a0", None, None),
            msg(3, "user", "u1", None, None),
            assistant_call(4, "c", "read_file", "{}"),
            tool_row(5, "c", &big("X:", 1_000)),
            msg(6, "assistant", "final", None, None),
            msg(7, "user", "u2", None, None),
            msg(8, "assistant", "a2", None, None),
        ];
        assert_eq!(find_tail_cut_by_tokens(&rows, 2, 20, 20, false, 1), 3);
    }

    fn golden_row(id: i64, value: &Value) -> CompressionHistoryMessage {
        let raw_json = |key: &str| {
            value
                .get(key)
                .filter(|field| !field.is_null())
                .map(Value::to_string)
        };
        CompressionHistoryMessage {
            id,
            message: HistoryMessage {
                role: value["role"].as_str().unwrap_or_default().into(),
                content: encode_message_content(value.get("content").unwrap_or(&Value::Null)),
                api_content: value
                    .get("api_content")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
            },
            tool_call_id: value
                .get("tool_call_id")
                .and_then(Value::as_str)
                .map(str::to_owned),
            tool_calls: raw_json("tool_calls"),
            tool_name: value
                .get("tool_name")
                .or_else(|| value.get("name"))
                .and_then(Value::as_str)
                .map(str::to_owned),
            reasoning: value
                .get("reasoning")
                .and_then(Value::as_str)
                .map(str::to_owned),
            reasoning_content: value
                .get("reasoning_content")
                .and_then(Value::as_str)
                .map(str::to_owned),
            reasoning_details: raw_json("reasoning_details"),
            codex_reasoning_items: raw_json("codex_reasoning_items"),
            codex_message_items: raw_json("codex_message_items"),
            compressed_summary: value
                .get("_compressed_summary")
                .is_some_and(crate::python_value::truthy),
        }
    }

    fn golden_rows(values: &[Value]) -> Vec<CompressionHistoryMessage> {
        values
            .iter()
            .enumerate()
            .map(|(index, value)| golden_row(index as i64 + 1, value))
            .collect()
    }

    fn assert_golden_rows(
        actual: &[CompressionHistoryMessage],
        expected: &[CompressionHistoryMessage],
        case: &Value,
    ) {
        assert_eq!(actual.len(), expected.len(), "row count case {case}");
        for (actual, expected) in actual.iter().zip(expected) {
            assert_eq!(actual.id, expected.id, "id case {case}");
            assert_eq!(
                actual.message.role, expected.message.role,
                "role case {case}"
            );
            assert_eq!(
                actual.message.model_content(),
                expected.message.model_content(),
                "content case {case}"
            );
            assert_eq!(
                actual.message.api_content, expected.message.api_content,
                "api_content case {case}"
            );
            assert_eq!(
                actual.tool_call_id, expected.tool_call_id,
                "call id case {case}"
            );
            assert_eq!(actual.tool_calls, expected.tool_calls, "calls case {case}");
            assert_eq!(
                actual.tool_name, expected.tool_name,
                "tool name case {case}"
            );
            assert_eq!(
                actual.reasoning, expected.reasoning,
                "reasoning case {case}"
            );
            assert_eq!(
                actual.reasoning_content, expected.reasoning_content,
                "reasoning content case {case}"
            );
            assert_eq!(
                actual.reasoning_details, expected.reasoning_details,
                "reasoning details case {case}"
            );
            assert_eq!(
                actual.codex_reasoning_items, expected.codex_reasoning_items,
                "Codex reasoning case {case}"
            );
            assert_eq!(
                actual.codex_message_items, expected.codex_message_items,
                "Codex messages case {case}"
            );
        }
    }

    #[test]
    fn token_budget_estimator_and_prune_match_python_goldens() {
        let cases: Value = serde_json::from_str(include_str!(
            "../../../tools/token-budget-prune-goldens.json"
        ))
        .unwrap();
        for case in cases["estimator"].as_array().unwrap() {
            let row = golden_row(1, &case["message"]);
            let actual = estimate_message_budget_tokens(
                &row,
                case["charge_stale_thinking"].as_bool().unwrap(),
            );
            assert_eq!(
                actual,
                case["expected_tokens"].as_u64().unwrap(),
                "estimator case {}",
                case["name"]
            );
        }
        for case in cases["prune"].as_array().unwrap() {
            let input = golden_rows(case["input"].as_array().unwrap());
            let expected = golden_rows(case["expected_output"].as_array().unwrap());
            let protect_count = case["protect_tail_count"].as_u64().unwrap() as usize;
            let min_chars = case["min_prune_chars"].as_u64().unwrap() as usize;
            let charge_all = crate::reasoning_replay::needs_echo(
                case["route"]["provider"].as_str().unwrap_or_default(),
                case["route"]["model"].as_str().unwrap_or_default(),
                case["route"]["base_url"].as_str().unwrap_or_default(),
            );
            let out = if let Some(budget) = case["protect_tail_tokens"].as_u64() {
                prune_old_tool_results_with_budget(
                    &input,
                    protect_count,
                    budget,
                    min_chars,
                    charge_all,
                )
            } else {
                prune_old_tool_results(&input, protect_count, min_chars)
            };
            assert_golden_rows(&out.messages, &expected, &case["name"]);
            assert_eq!(
                out.pruned_count,
                case["expected_pruned"].as_u64().unwrap() as usize,
                "pruned count case {}",
                case["name"]
            );
        }
        for case in cases["tail_cut"].as_array().unwrap() {
            let input = golden_rows(case["input"].as_array().unwrap());
            let charge_all = crate::reasoning_replay::needs_echo(
                case["route"]["provider"].as_str().unwrap_or_default(),
                case["route"]["model"].as_str().unwrap_or_default(),
                case["route"]["base_url"].as_str().unwrap_or_default(),
            );
            let actual = find_tail_cut_by_tokens(
                &input,
                case["head_end"].as_u64().unwrap() as usize,
                case["protect_last_n"].as_u64().unwrap() as usize,
                case["token_budget"].as_u64().unwrap(),
                charge_all,
                1,
            );
            assert_eq!(
                actual,
                case["expected_cut"].as_u64().unwrap() as usize,
                "tail cut case {}",
                case["name"]
            );
        }
    }

    #[test]
    fn default_single_user_anchor_keeps_its_original_actionable_definition() {
        let continuation = msg(
            1,
            "user",
            crate::compression_prompt::COMPRESSION_CONTINUATION_USER_CONTENT,
            None,
            None,
        );
        assert!(actionable_user(&continuation));
        assert!(!real_actionable_user(&continuation));
    }

    #[test]
    fn test_multi_user_tail_anchoring_table() {
        let mut messages = vec![
            msg(1, "user", "head prompt", None, None),
            msg(2, "assistant", "head reply", None, None),
        ];
        for i in 1..=4 {
            messages.push(msg(
                (i * 2 + 1) as i64,
                "user",
                &format!("user {i}"),
                None,
                None,
            ));
            messages.push(msg(
                (i * 2 + 2) as i64,
                "assistant",
                &big("A", 3000),
                None,
                None,
            ));
        }

        let head_end = 1;
        struct TestCase {
            min_users: usize,
            expected_cut: usize,
            name: &'static str,
        }

        let cases = vec![
            TestCase {
                min_users: 1,
                expected_cut: 8,
                name: "N=1 anchors to last user (index 8)",
            },
            TestCase {
                min_users: 2,
                expected_cut: 6,
                name: "N=2 pulls back monotonically to 2nd-to-last user (index 6)",
            },
            TestCase {
                min_users: 3,
                expected_cut: 4,
                name: "N=3 pulls back monotonically to 3rd-to-last user (index 4)",
            },
            TestCase {
                min_users: 4,
                expected_cut: 2,
                name: "N=4 pulls back monotonically to 4th-to-last user (index 2)",
            },
            TestCase {
                min_users: 10,
                expected_cut: 2,
                name: "N=10 clamps to earliest available user above head_end (index 2)",
            },
        ];

        for case in cases {
            let cut =
                anchor_last_n_user_messages(&messages, messages.len(), head_end, case.min_users);
            assert_eq!(cut, case.expected_cut, "failed: {}", case.name);
        }
    }

    #[test]
    fn test_blank_and_synthetic_user_rows_do_not_consume_slots() {
        let messages = vec![
            msg(1, "user", "head", None, None),
            msg(2, "assistant", "head reply", None, None),
            msg(3, "user", "real oldest", None, None),
            msg(4, "assistant", "reply 1", None, None),
            msg(5, "user", "real middle", None, None),
            msg(6, "assistant", "reply 2", None, None),
            msg(7, "user", "", None, None),
            msg(8, "assistant", "reply 3", None, None),
            msg(9, "user", "   ", None, None),
            msg(10, "assistant", "reply 4", None, None),
            msg(
                11,
                "user",
                crate::compression_prompt::COMPRESSION_CONTINUATION_USER_CONTENT,
                None,
                None,
            ),
            msg(12, "assistant", "reply 5", None, None),
            msg(
                13,
                "user",
                "[IMPORTANT: Background process 42 finished]",
                None,
                None,
            ),
            msg(14, "assistant", "reply 6", None, None),
            msg(
                15,
                "user",
                "[Your active task list was preserved across context compression]\n- task 1",
                None,
                None,
            ),
            msg(16, "assistant", "reply 7", None, None),
            msg(17, "user", "real latest", None, None),
            msg(18, "assistant", "final reply", None, None),
        ];

        let cut = anchor_last_n_user_messages(&messages, messages.len(), 1, 3);
        assert_eq!(cut, 2);
    }

    #[test]
    fn test_multimodal_blank_user_check() {
        let mut empty_arr_row = msg(1, "user", "", None, None);
        empty_arr_row.message.content =
            crate::session_db::encode_message_content(&Value::Array(Vec::new()));
        assert!(is_blank_user_turn(&empty_arr_row));

        let mut whitespace_text_arr = msg(2, "user", "", None, None);
        whitespace_text_arr.message.content = crate::session_db::encode_message_content(&json!([
            {"type": "text", "text": "   "}
        ]));
        assert!(is_blank_user_turn(&whitespace_text_arr));

        let mut image_arr = msg(3, "user", "", None, None);
        image_arr.message.content = crate::session_db::encode_message_content(&json!([
            {"type": "image_url", "image_url": {"url": "https://example.com/image.png"}}
        ]));
        assert!(!is_blank_user_turn(&image_arr));
    }

    #[test]
    fn test_tool_group_integrity_across_tail_boundary() {
        let messages = vec![
            msg(1, "user", "start", None, None),
            assistant_call(2, "call_1", "read_file", "{}"),
            tool_row(3, "call_1", &big("RESULT", 1000)),
            msg(4, "user", "user 3rd last", None, None),
            msg(5, "assistant", "reply 3rd last", None, None),
            msg(6, "user", "user 2nd last", None, None),
            msg(7, "assistant", "reply 2nd last", None, None),
            msg(8, "user", "user last", None, None),
            msg(9, "assistant", "reply last", None, None),
        ];
        let head_end = 0;
        let cut = anchor_last_n_user_messages(&messages, messages.len(), head_end, 3);
        assert_eq!(cut, 3);
        assert_eq!(messages[cut].message.role, "user");
        assert_eq!(messages[cut].message.content, "user 3rd last");
    }
}
