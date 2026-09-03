//! Gateway response filtering helpers.
//!
// Public API is ahead of its callers while the delivery path is ported.
#![allow(dead_code)]
//!
//! Port of `gateway/response_filters.py`. These operate at the gateway
//! boundary: they decide whether a completed agent turn should be *delivered*
//! to the chat, not what is persisted in conversation history. All pure
//! functions of the response text.

use serde_json::Value;
use unicode_general_category::{get_general_category, GeneralCategory};

/// Canonical model-emitted control token for intentional silence.
pub const SILENT_REPLY_TOKEN: &str = "NO_REPLY";

/// Exact whole-response markers meaning "the agent intentionally chose not to
/// reply". Kept small and explicit; arbitrary empty output is an error/empty
/// path, not silence.
const LIVE_GATEWAY_SILENT_MARKERS: [&str; 4] = ["[SILENT]", "SILENT", "NO_REPLY", "NO REPLY"];

fn is_marker(candidate: &str) -> bool {
    LIVE_GATEWAY_SILENT_MARKERS.contains(&candidate)
}

/// True for any Unicode punctuation (general category starting with `P`),
/// matching Python's `unicodedata.category(c).startswith("P")`.
fn is_punctuation(c: char) -> bool {
    matches!(
        get_general_category(c),
        GeneralCategory::ConnectorPunctuation
            | GeneralCategory::DashPunctuation
            | GeneralCategory::OpenPunctuation
            | GeneralCategory::ClosePunctuation
            | GeneralCategory::InitialPunctuation
            | GeneralCategory::FinalPunctuation
            | GeneralCategory::OtherPunctuation
    )
}

fn canonical_silence_candidate(text: &str) -> String {
    text.trim()
        .to_uppercase()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Strip stray edge punctuation without erasing marker structure. Square
/// brackets stay structural so a malformed `[SILENT` does not become `SILENT`.
fn strip_edge_silence_punctuation(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut start = 0usize;
    let mut end = chars.len();
    while start < end && chars[start] != '[' && chars[start] != ']' && is_punctuation(chars[start]) {
        start += 1;
    }
    while end > start
        && chars[end - 1] != '['
        && chars[end - 1] != ']'
        && is_punctuation(chars[end - 1])
    {
        end -= 1;
    }
    chars[start..end].iter().collect::<String>().trim().to_string()
}

fn canonical_silence_candidates(text: &str) -> Vec<String> {
    let trimmed = text.trim();
    let exact = canonical_silence_candidate(text);
    let stripped = strip_edge_silence_punctuation(trimmed);
    if stripped == trimmed {
        return vec![exact];
    }
    let fallback = canonical_silence_candidate(&stripped);
    vec![exact, fallback]
}

/// True only when `response` is exactly a silence marker. Substantive prose
/// that merely mentions `NO_REPLY`/`[SILENT]` is delivered normally; a blank
/// response is not silence (handled by the empty-response path).
pub fn is_intentional_silence_response(response: &str) -> bool {
    let stripped = response.trim();
    if stripped.is_empty() || stripped.chars().count() > 64 {
        return false;
    }
    canonical_silence_candidates(stripped)
        .iter()
        .any(|c| is_marker(c))
}

/// Loose silence matcher for autonomous lanes (cron, webhook). Suppresses when
/// a marker is the whole response, sits on its own first or last line, or the
/// bracketed sentinel opens the response (`[SILENT] No changes detected`). A
/// token buried mid-sentence in a genuine report is still delivered.
pub fn is_autonomous_silence_response(response: &str) -> bool {
    let stripped = response.trim();
    if stripped.is_empty() {
        return false;
    }
    let is_token = |line: &str| is_marker(&canonical_silence_candidate(line));

    if is_token(stripped) {
        return true;
    }
    let lines: Vec<&str> = stripped.lines().filter(|l| !l.trim().is_empty()).collect();
    if let (Some(first), Some(last)) = (lines.first(), lines.last()) {
        if is_token(first) || is_token(last) {
            return true;
        }
    }
    if stripped.to_uppercase().starts_with("[SILENT]") {
        return true;
    }
    false
}

/// Silence markers suppress delivery only for successful agent turns. Mirrors
/// the Python `agent_result: dict | None`: a missing/non-object result, or one
/// whose `failed` is truthy, is never silence.
pub fn is_intentional_silence_agent_result(agent_result: Option<&Value>, response: &str) -> bool {
    let Some(Value::Object(map)) = agent_result else {
        return false;
    };
    if map.get("failed").is_some_and(is_truthy) {
        return false;
    }
    is_intentional_silence_response(response)
}

/// True while `text` could still resolve to a silence marker. The streaming
/// path accumulates the reply delta-by-delta; a buffer whose canonical form is
/// a non-empty prefix of a marker (e.g. `NO` toward `NO_REPLY`) is held back so
/// a raw marker is never shown and then retracted. Diverged prose and anything
/// over the cap return false so normal streaming resumes.
pub fn is_partial_silence_marker(text: &str) -> bool {
    let stripped = text.trim();
    if stripped.is_empty() || stripped.chars().count() > 64 {
        return false;
    }
    canonical_silence_candidates(stripped).iter().any(|candidate| {
        !candidate.is_empty()
            && LIVE_GATEWAY_SILENT_MARKERS
                .iter()
                .any(|marker| marker.starts_with(candidate.as_str()))
    })
}

/// Python truthiness for the `failed` flag (bool, non-zero number, non-empty
/// string/array/object all count as failed).
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn exact_markers_are_silence() {
        for m in ["NO_REPLY", "[SILENT]", "SILENT", "NO REPLY", "  no_reply  "] {
            assert!(is_intentional_silence_response(m), "{m:?}");
        }
    }

    #[test]
    fn prose_mentioning_marker_is_delivered() {
        assert!(!is_intentional_silence_response(
            "I considered NO_REPLY but here is the answer."
        ));
        assert!(!is_intentional_silence_response("Silent retry succeeded"));
        assert!(!is_intentional_silence_response(""));
    }

    #[test]
    fn edge_punctuation_stripped_but_brackets_structural() {
        assert!(is_intentional_silence_response(".NO_REPLY"));
        assert!(is_intentional_silence_response("*NO_REPLY*"));
        // Malformed "[SILENT" must NOT collapse to "SILENT".
        assert!(!is_intentional_silence_response("[SILENT"));
    }

    #[test]
    fn autonomous_is_looser() {
        assert!(is_autonomous_silence_response("[SILENT] No changes detected"));
        assert!(is_autonomous_silence_response("2 deals filtered\n\n[SILENT]"));
        assert!(is_autonomous_silence_response("[SILENT]\nleading note ignored"));
        assert!(!is_autonomous_silence_response("Silent retry succeeded"));
    }

    #[test]
    fn agent_result_gates_on_failure() {
        let resp = "NO_REPLY";
        assert!(is_intentional_silence_agent_result(
            Some(&json!({"failed": false})),
            resp
        ));
        assert!(!is_intentional_silence_agent_result(
            Some(&json!({"failed": true})),
            resp
        ));
        assert!(!is_intentional_silence_agent_result(None, resp));
        // Object without a `failed` key proceeds to the response check.
        assert!(is_intentional_silence_agent_result(Some(&json!({})), resp));
    }

    #[test]
    fn partial_prefixes_are_held() {
        assert!(is_partial_silence_marker("NO"));
        assert!(is_partial_silence_marker("no_re"));
        assert!(is_partial_silence_marker("NO_REPLY"));
        // Diverged prose resumes streaming immediately.
        assert!(!is_partial_silence_marker("Nope, here goes"));
        assert!(!is_partial_silence_marker(""));
    }
}
