//! Pre-call tool_call/tool_result pairing repair.
//!
//! Port of `agent.agent_runtime_helpers.sanitize_api_messages` plus the shared
//! alias helpers it leans on from `agent.message_sanitization`
//! (`_expand_tool_id_variants`, `tool_call_id_variants`,
//! `tool_result_id_variants`, `coalesce_tool_call_id`). Every pass runs on a
//! clone of the caller's messages so the stored trajectory stays byte-stable;
//! only the wire copy is repaired.
//!
//! The passes run in the reference order: role allowlist, empty-content healing
//! (delegated to [`crate::message_repair::heal_empty_non_final`]), empty/invalid
//! tool_calls normalization, blank function-name repair, missing-id result drop,
//! positional pairing with deterministic missing stubs, alias-group dedup with
//! re-arming, and tool-result name alignment.
use crate::message_repair::heal_empty_non_final;
use crate::python_value::{python_whitespace, truthy};
use serde_json::{json, Value};
use std::collections::{BTreeSet, HashMap, HashSet};

const VALID_API_ROLES: [&str; 6] = [
    "system",
    "user",
    "assistant",
    "tool",
    "function",
    "developer",
];

const EMPTY_NAME_SENTINEL: &str = "invalid_tool_call";

// ---------------------------------------------------------------------------
// Shared alias expansion / coalesce helpers (agent.message_sanitization).
// ---------------------------------------------------------------------------

/// Return every wire spelling of one or more tool-call identifiers.
///
/// Responses bridges may expose the pairing id and response-item id separately,
/// or encode both as `call_id|response_item_id`. The values are aliases for one
/// call, not distinct calls. Non-string entries are skipped.
fn expand_tool_id_variants(values: &[&Value]) -> BTreeSet<String> {
    let mut variants: BTreeSet<String> = BTreeSet::new();
    for raw in values {
        let Some(text) = raw.as_str() else {
            continue;
        };
        let value = text.trim_matches(python_whitespace);
        if value.is_empty() {
            continue;
        }
        variants.insert(value.to_string());
        if value.contains('|') {
            for part in value.split('|') {
                let part = part.trim_matches(python_whitespace);
                if !part.is_empty() {
                    variants.insert(part.to_string());
                }
            }
        }
    }
    variants
}

/// All pairing-id variants carried by a tool-call entry.
fn tool_call_id_variants(tc: &Value) -> BTreeSet<String> {
    expand_tool_id_variants(&[&tc["call_id"], &tc["id"], &tc["response_item_id"]])
}

/// All matching variants for a role=tool `tool_call_id`.
fn tool_result_id_variants(tool_call_id: &Value) -> BTreeSet<String> {
    expand_tool_id_variants(&[tool_call_id])
}

/// The effective call ID from a tool_call entry (`call_id` preferred, then
/// `id`, composite `call|item` collapsed to its call half). Returns `""` when
/// neither pairing field is a non-blank string.
fn coalesce_tool_call_id(tc: &Value) -> String {
    for key in ["call_id", "id"] {
        let Some(raw) = tc.get(key).and_then(Value::as_str) else {
            continue;
        };
        let value = raw.trim_matches(python_whitespace);
        if value.is_empty() {
            continue;
        }
        let head = value
            .split('|')
            .next()
            .unwrap_or("")
            .trim_matches(python_whitespace);
        return if head.is_empty() {
            value.to_string()
        } else {
            head.to_string()
        };
    }
    String::new()
}

/// Mirror of `AIAgent._get_tool_call_name_static`: `function.name` when it is a
/// truthy value, else the empty string. Best-effort. callers fall back to "".
fn tool_call_name(tc: &Value) -> Value {
    if let Some(function) = tc.get("function") {
        if function.is_object() {
            let name = function
                .get("name")
                .cloned()
                .unwrap_or_else(|| Value::String(String::new()));
            return if truthy(&name) {
                name
            } else {
                Value::String(String::new())
            };
        }
    }
    Value::String(String::new())
}

/// `(value or "").strip()` for a JSON scalar. Non-string values become "".
fn stripped_str(value: &Value) -> String {
    match value.as_str() {
        Some(text) => text.trim_matches(python_whitespace).to_string(),
        None => String::new(),
    }
}

fn role_of(message: &Value) -> Option<&str> {
    message.get("role").and_then(Value::as_str)
}

// ---------------------------------------------------------------------------
// Positional pairing helper.
// ---------------------------------------------------------------------------

// One declared assistant tool_call awaiting its positional result: the
// alias-group key (smallest variant), a clone of the call, and its full variant
// set. A Vec keeps insertion order so the `next matching` walk mirrors the
// reference dict iteration.
type DeclaredCall = (String, Value, BTreeSet<String>);

/// Emit a deterministic stub result for every declared call still unanswered,
/// ordered by sorted alias-group key, then clear the pending set.
fn flush_unanswered_stubs(paired: &mut Vec<Value>, declared: &mut Vec<DeclaredCall>) {
    let mut order: Vec<usize> = (0..declared.len()).collect();
    order.sort_by(|&a, &b| declared[a].0.cmp(&declared[b].0));
    for index in order {
        let (key, tc, _variants) = &declared[index];
        let coalesced = coalesce_tool_call_id(tc);
        let cid = if coalesced.is_empty() {
            key.clone()
        } else {
            coalesced
        };
        paired.push(json!({
            "role": "tool",
            "name": tool_call_name(tc),
            "content": "[Result unavailable — see context summary above]",
            "tool_call_id": cid,
        }));
    }
    declared.clear();
}

// ---------------------------------------------------------------------------
// Public entry.
// ---------------------------------------------------------------------------

/// Fix orphaned tool_call / tool_result pairs before every LLM call. Runs on a
/// clone of `messages` and returns the repaired wire copy.
pub fn sanitize(messages: &[Value]) -> Vec<Value> {
    let mut messages: Vec<Value> = messages.to_vec();

    // --- Role allowlist: drop messages with roles the API won't accept ---
    messages.retain(|msg| matches!(role_of(msg), Some(role) if VALID_API_ROLES.contains(&role)));

    // --- Heal empty-content non-final messages (self-recovery) ---
    // Done first so a substituted turn participates normally in the tool-pair
    // and dedup passes below.
    heal_empty_non_final(&mut messages);

    // --- Drop empty / malformed tool_calls arrays on assistant messages ---
    // An assistant message carrying `tool_calls: []` (or a non-list value) is
    // semantically identical to one with no tool calls, but strict providers
    // reject the empty array outright. Shallow-remove the key so stored history
    // stays byte-stable (we already work on a clone here).
    for msg in messages.iter_mut() {
        let is_assistant = role_of(msg) == Some("assistant");
        let has_key = msg.get("tool_calls").is_some();
        let non_empty_list = msg
            .get("tool_calls")
            .and_then(Value::as_array)
            .is_some_and(|calls| !calls.is_empty());
        if is_assistant && has_key && !non_empty_list {
            if let Some(object) = msg.as_object_mut() {
                object.shift_remove("tool_calls");
            }
        }
    }

    // --- Repair tool_calls whose function.name is empty/missing ---
    // Rename a blank name to a non-empty sentinel so the call and its result
    // stay paired. dropping the call would orphan its anti-priming result.
    for msg in messages.iter_mut() {
        if role_of(msg) != Some("assistant") {
            continue;
        }
        let Some(tcs) = msg.get_mut("tool_calls").and_then(Value::as_array_mut) else {
            continue;
        };
        if tcs.is_empty() {
            continue;
        }
        for tc in tcs.iter_mut() {
            let blank = {
                let name = tc
                    .get("function")
                    .filter(|function| function.is_object())
                    .and_then(|function| function.get("name"));
                !matches!(name, Some(Value::String(text)) if !text.trim_matches(python_whitespace).is_empty())
            };
            if !blank {
                continue;
            }
            let fn_is_object = tc.get("function").is_some_and(Value::is_object);
            if fn_is_object {
                if let Some(function) = tc.get_mut("function").and_then(Value::as_object_mut) {
                    function.insert(
                        "name".to_string(),
                        Value::String(EMPTY_NAME_SENTINEL.to_string()),
                    );
                }
            } else if let Some(object) = tc.as_object_mut() {
                object.insert(
                    "function".to_string(),
                    json!({"name": EMPTY_NAME_SENTINEL, "arguments": "{}"}),
                );
            }
        }
    }

    // --- Drop tool results with a missing/empty tool_call_id ---
    // A result with no `tool_call_id` at all is a schema violation strict
    // providers reject outright. keep the explicit filter so the guarantee does
    // not silently depend on the positional walk's internals.
    messages.retain(|msg| {
        !(role_of(msg) == Some("tool") && stripped_str(&msg["tool_call_id"]).is_empty())
    });

    // --- Positional tool_call <-> tool_result pairing ---
    // Strict providers enforce the POSITIONAL invariant: an assistant message
    // carrying tool_calls must be IMMEDIATELY followed by tool messages covering
    // every tool_call_id. A rolling walk drops positional orphans and injects a
    // stub for every declared id the immediately-following run leaves unanswered.
    let mut paired: Vec<Value> = Vec::with_capacity(messages.len());
    let mut declared: Vec<DeclaredCall> = Vec::new();
    for msg in messages.into_iter() {
        match role_of(&msg) {
            Some("assistant") => {
                // A new assistant turn closes the previous tool-result run.
                flush_unanswered_stubs(&mut paired, &mut declared);
                if let Some(tcs) = msg.get("tool_calls").and_then(Value::as_array) {
                    for tc in tcs {
                        let variants = tool_call_id_variants(tc);
                        if variants.is_empty() {
                            continue;
                        }
                        // Key on a stable representative of the alias group so a
                        // result matching ANY spelling can consume the call.
                        let key = variants.iter().next().cloned().unwrap();
                        if let Some(slot) = declared.iter_mut().find(|entry| entry.0 == key) {
                            slot.1 = tc.clone();
                            slot.2 = variants;
                        } else {
                            declared.push((key, tc.clone(), variants));
                        }
                    }
                }
                paired.push(msg);
            }
            Some("tool") => {
                let result_variants = tool_result_id_variants(&msg["tool_call_id"]);
                let matched = declared.iter().position(|(_key, _tc, variants)| {
                    variants
                        .iter()
                        .any(|variant| result_variants.contains(variant))
                });
                if let Some(pos) = matched {
                    paired.push(msg);
                    // Consume so a duplicate result reusing the id is dropped.
                    declared.remove(pos);
                }
                // else: positionally orphaned result. dropped.
            }
            other => {
                if other == Some("user") {
                    // A user turn closes the tool-result run.
                    flush_unanswered_stubs(&mut paired, &mut declared);
                }
                paired.push(msg);
            }
        }
    }
    // The transcript may end right after an unanswered assistant turn.
    flush_unanswered_stubs(&mut paired, &mut declared);
    let messages = paired;

    // --- Deduplicate tool_call_ids ---
    // (a) collapse duplicate tool_calls WITHIN an assistant turn
    // (b) drop tool results that answer no OUTSTANDING tool call
    // Outstanding-call (not seen-forever) semantics keep both protections while
    // tolerating providers that reuse one constant id across calls. Variant-group
    // tracking makes answering or deduping one spelling consume its siblings too.
    let mut seen_assistant: HashSet<String> = HashSet::new();
    let mut outstanding: HashSet<String> = HashSet::new();
    let mut outstanding_groups: HashMap<usize, BTreeSet<String>> = HashMap::new();
    let mut variant_to_group: HashMap<String, usize> = HashMap::new();
    let mut next_group_id: usize = 0;
    let mut deduped: Vec<Value> = Vec::with_capacity(messages.len());
    for msg in messages.into_iter() {
        let role = role_of(&msg).map(str::to_string);
        if role.as_deref() == Some("assistant") && truthy(&msg["tool_calls"]) {
            let tcs = msg["tool_calls"].as_array().cloned().unwrap_or_default();
            let mut kept: Vec<Value> = Vec::new();
            for tc in &tcs {
                let variants = tool_call_id_variants(tc);
                if !variants.is_empty()
                    && variants
                        .iter()
                        .any(|variant| seen_assistant.contains(variant))
                {
                    continue;
                }
                if !variants.is_empty() {
                    let group_id = next_group_id;
                    next_group_id += 1;
                    outstanding_groups.insert(group_id, variants.clone());
                    for variant in &variants {
                        seen_assistant.insert(variant.clone());
                        outstanding.insert(variant.clone());
                        variant_to_group.entry(variant.clone()).or_insert(group_id);
                    }
                }
                kept.push(tc.clone());
            }
            let mut msg = msg;
            if !kept.is_empty() {
                msg["tool_calls"] = Value::Array(kept);
            } else if !tcs.is_empty() {
                if let Some(object) = msg.as_object_mut() {
                    object.shift_remove("tool_calls");
                }
            }
            deduped.push(msg);
        } else if role.as_deref() == Some("tool") {
            let result_variants = tool_result_id_variants(&msg["tool_call_id"]);
            let mut candidate_groups: BTreeSet<usize> = BTreeSet::new();
            for variant in &result_variants {
                if let Some(&group_id) = variant_to_group.get(variant) {
                    if outstanding.contains(variant) {
                        candidate_groups.insert(group_id);
                    }
                }
            }
            if !result_variants.is_empty() && candidate_groups.is_empty() {
                continue;
            }
            if let Some(&group_id) = candidate_groups.iter().next() {
                // Answered: consume EVERY variant of the matched call so a second
                // result replaying any sibling spelling is still caught, and the
                // ids stay re-armable by the next assistant call that reuses them.
                let group_variants = outstanding_groups.remove(&group_id).unwrap_or_default();
                for variant in &group_variants {
                    outstanding.remove(variant);
                    seen_assistant.remove(variant);
                    if variant_to_group.get(variant) == Some(&group_id) {
                        variant_to_group.remove(variant);
                    }
                }
            }
            deduped.push(msg);
        } else {
            deduped.push(msg);
        }
    }
    let mut messages = deduped;

    // --- Align each tool result's name with the call it answers ---
    // Google matches functionResponse.name against functionCall.name. rewrite a
    // present, disagreeing name. leave an absent name absent so clean transcripts
    // pass through byte-identical for prompt caching.
    let mut call_names: HashMap<String, Value> = HashMap::new();
    for msg in &messages {
        if role_of(msg) != Some("assistant") {
            continue;
        }
        if let Some(tcs) = msg.get("tool_calls").and_then(Value::as_array) {
            for tc in tcs {
                let cid = coalesce_tool_call_id(tc);
                let cid = cid.trim_matches(python_whitespace).to_string();
                let name = tool_call_name(tc);
                if !cid.is_empty() && truthy(&name) {
                    call_names.insert(cid, name);
                }
            }
        }
    }
    for msg in messages.iter_mut() {
        if role_of(msg) != Some("tool") {
            continue;
        }
        let cid = stripped_str(&msg["tool_call_id"]);
        let Some(expected) = call_names.get(&cid).cloned() else {
            continue;
        };
        let current = msg.get("name").cloned().unwrap_or(Value::Null);
        if truthy(&expected) && truthy(&current) && current != expected {
            msg["name"] = expected;
        }
    }

    messages
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expand_splits_composite_bridge_ids() {
        let variants = expand_tool_id_variants(&[&json!("call_x|fc_y"), &json!("  "), &json!(7)]);
        let got: Vec<&String> = variants.iter().collect();
        assert_eq!(got, vec!["call_x", "call_x|fc_y", "fc_y"]);
    }

    #[test]
    fn coalesce_prefers_call_id_and_collapses_composite() {
        assert_eq!(
            coalesce_tool_call_id(&json!({"call_id": "call_a", "id": "fc_b"})),
            "call_a"
        );
        assert_eq!(
            coalesce_tool_call_id(&json!({"id": " call_x|fc_y "})),
            "call_x"
        );
        assert_eq!(coalesce_tool_call_id(&json!({"id": "|only"})), "|only");
        assert_eq!(coalesce_tool_call_id(&json!({"other": "x"})), "");
    }

    #[test]
    fn drops_invalid_role_and_orphan_result() {
        let out = sanitize(&[
            json!({"role": "ghost", "content": "x"}),
            json!({"role": "user", "content": "hi"}),
            json!({"role": "tool", "tool_call_id": "call_1", "content": "orphan"}),
        ]);
        assert_eq!(out, vec![json!({"role": "user", "content": "hi"})]);
    }

    #[test]
    fn injects_stub_for_unanswered_call() {
        let out = sanitize(&[json!({
            "role": "assistant",
            "content": "",
            "tool_calls": [{"id": "call_1", "function": {"name": "search", "arguments": "{}"}}],
        })]);
        assert_eq!(out.len(), 2);
        assert_eq!(out[1]["role"], "tool");
        assert_eq!(out[1]["name"], "search");
        assert_eq!(out[1]["tool_call_id"], "call_1");
        assert_eq!(
            out[1]["content"],
            "[Result unavailable — see context summary above]"
        );
    }

    #[test]
    fn blank_function_name_gets_sentinel() {
        let out = sanitize(&[
            json!({
                "role": "assistant",
                "content": "",
                "tool_calls": [{"id": "call_1", "function": {"name": "  ", "arguments": "{}"}}],
            }),
            json!({"role": "tool", "tool_call_id": "call_1", "content": "ok"}),
        ]);
        assert_eq!(
            out[0]["tool_calls"][0]["function"]["name"],
            EMPTY_NAME_SENTINEL
        );
    }

    #[test]
    fn realigns_result_name_with_call() {
        let out = sanitize(&[
            json!({
                "role": "assistant",
                "content": "",
                "tool_calls": [{"id": "call_1", "function": {"name": "real_tool", "arguments": "{}"}}],
            }),
            json!({"role": "tool", "tool_call_id": "call_1", "name": "tool_call", "content": "ok"}),
        ]);
        assert_eq!(out[1]["name"], "real_tool");
    }

    #[test]
    fn python_oracle_pairing_cases() {
        let rows: Value =
            serde_json::from_str(include_str!("../../../tools/tool-pairing-goldens.json")).unwrap();
        for row in rows.as_array().unwrap() {
            let messages = row["messages"].as_array().unwrap();
            let original = messages.clone();
            assert_eq!(
                sanitize(messages),
                *row["expected"].as_array().unwrap(),
                "{row}"
            );
            // The public entry works on a clone. the caller's input is untouched.
            assert_eq!(*messages, original);
        }
    }
}
