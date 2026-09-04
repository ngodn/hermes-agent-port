//! Port of gateway/media_repair.py.
//!
// Public API is ahead of its callers (the delivery turn path wires it).
#![allow(dead_code)]
//!
//! Repair model-mangled `computer_use` screenshot paths in final responses.
//! `computer_use` persists a bounded screenshot into the image cache and tells
//! the model its absolute path; some models rewrite a Windows path into a
//! POSIX-looking one when emitting an explicit `MEDIA:` directive, so delivery-
//! path validation rejects the nonexistent path and the attachment is dropped.
//!
//! The repair is deliberately narrow: it only rewrites paths inside a response
//! that already carries a `MEDIA:` directive, and only when the directive's
//! generated `computer_use_<uuid>` basename exactly matches a canonical
//! screenshot path returned by `computer_use` in the current turn. It never
//! auto-attaches captures, and normal media path validation still runs after.
//! Fail-open: the repair is cosmetic, so anything unexpected returns the
//! response unchanged.

use std::collections::HashMap;
use std::sync::OnceLock;

use fancy_regex::Regex;
use serde_json::Value;

/// Absolute-path prefix accepted for canonical capture paths (Windows drive,
/// POSIX root, or UNC share).
fn abs_prefix_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^(?:[A-Za-z]:[/\\]|/|\\\\)").unwrap())
}

fn capture_basename_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?i)^computer_use_[0-9a-f]{32}\.(?:png|jpe?g)$").unwrap())
}

fn capture_summary_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"(?i)\(shareable screenshot saved to (?P<path>(?:[A-Za-z]:[/\\]|/|\\\\)[^\r\n]*?computer_use_[0-9a-f]{32}\.(?:png|jpe?g))\)",
        )
        .unwrap()
    })
}

/// Map assistant tool-call ids to tool names for the given messages.
pub fn tool_name_by_call_id(messages: &[Value]) -> HashMap<String, String> {
    let mut mapping = HashMap::new();
    for msg in messages {
        if msg.get("role").and_then(Value::as_str) != Some("assistant") {
            continue;
        }
        let Some(calls) = msg.get("tool_calls").and_then(Value::as_array) else {
            continue;
        };
        for call in calls {
            let call_id = call
                .get("id")
                .and_then(Value::as_str)
                .or_else(|| call.get("call_id").and_then(Value::as_str));
            let name = call
                .get("function")
                .and_then(|f| f.get("name"))
                .and_then(Value::as_str)
                .or_else(|| call.get("name").and_then(Value::as_str))
                .unwrap_or("");
            if let Some(call_id) = call_id {
                if !call_id.is_empty() && !name.is_empty() {
                    mapping.insert(call_id.to_string(), name.to_string());
                }
            }
        }
    }
    mapping
}

/// A canonical capture basename for either path separator style, or empty.
fn computer_use_capture_basename(path: &str) -> String {
    let value = path.trim().trim_matches(['`', '"', '\'']);
    let basename = value.rsplit(['/', '\\']).next().unwrap_or(value);
    if capture_basename_re().is_match(basename).unwrap_or(false) {
        basename.to_lowercase()
    } else {
        String::new()
    }
}

/// Yield persisted screenshot paths from computer_use result content (JSON, a
/// multimodal content list, or a text summary).
fn iter_capture_paths(content: &Value, out: &mut Vec<String>) {
    match content {
        Value::String(s) => {
            let stripped = s.trim();
            if stripped.starts_with('{') || stripped.starts_with('[') {
                // JSON-looking: parse first, never regex-scan the raw text
                // (escaping would yield a nonexistent path). Fail closed on
                // unparseable JSON.
                if let Ok(payload) = serde_json::from_str::<Value>(stripped) {
                    if payload.is_object() || payload.is_array() {
                        iter_capture_paths(&payload, out);
                    }
                }
                return;
            }
            let mut pos = 0usize;
            while let Ok(Some(cap)) = capture_summary_re().captures_from_pos(s, pos) {
                let whole = cap.get(0).unwrap();
                if let Some(p) = cap.name("path") {
                    out.push(s[p.start()..p.end()].trim().to_string());
                }
                pos = whole.end().max(whole.start() + 1);
            }
        }
        Value::Array(items) => {
            for part in items {
                iter_capture_paths(part, out);
            }
        }
        Value::Object(map) => {
            if let Some(Value::String(sp)) = map.get("screenshot_path") {
                out.push(sp.clone());
            }
            if let Some(Value::Object(meta)) = map.get("meta") {
                if let Some(Value::String(sp)) = meta.get("screenshot_path") {
                    out.push(sp.clone());
                }
            }
            for field in ["content", "text", "text_summary", "summary"] {
                if let Some(nested) = map.get(field) {
                    if nested.is_string() || nested.is_object() || nested.is_array() {
                        iter_capture_paths(nested, out);
                    }
                }
            }
        }
        _ => {}
    }
}

/// Recover model-mangled paths for explicitly requested computer_use
/// screenshots. Only rewrites an already-explicit `MEDIA:` directive whose
/// unique generated basename matches a canonical screenshot path from this turn.
pub fn repair_explicit_computer_use_media_paths(
    response: &str,
    messages: &[Value],
    history_offset: usize,
) -> String {
    if !response.contains("MEDIA:") {
        return response.to_string();
    }

    // Select the current turn's messages.
    let turn_messages: &[Value] = if history_offset > 0 && messages.len() >= history_offset {
        &messages[history_offset..]
    } else if history_offset > 0 {
        match messages
            .iter()
            .rposition(|m| m.get("role").and_then(Value::as_str) == Some("user"))
        {
            Some(idx) => &messages[idx..],
            None => &[],
        }
    } else {
        messages
    };

    let call_id_names = tool_name_by_call_id(turn_messages);

    let mut canonical_by_basename: HashMap<String, String> = HashMap::new();
    for msg in turn_messages {
        let role = msg.get("role").and_then(Value::as_str).unwrap_or("");
        if role != "tool" && role != "function" {
            continue;
        }
        let call_id = msg
            .get("tool_call_id")
            .and_then(Value::as_str)
            .or_else(|| msg.get("call_id").and_then(Value::as_str))
            .unwrap_or("");
        let tool_name = msg
            .get("name")
            .and_then(Value::as_str)
            .or_else(|| msg.get("tool_name").and_then(Value::as_str))
            .map(|s| s.to_string())
            .or_else(|| call_id_names.get(call_id).cloned())
            .unwrap_or_default();
        if tool_name != "computer_use" {
            continue;
        }
        if let Some(content) = msg.get("content") {
            let mut paths = Vec::new();
            iter_capture_paths(content, &mut paths);
            for path in paths {
                let basename = computer_use_capture_basename(&path);
                if !basename.is_empty() && abs_prefix_re().is_match(&path).unwrap_or(false) {
                    canonical_by_basename.insert(basename, path);
                }
            }
        }
    }

    if canonical_by_basename.is_empty() {
        return response.to_string();
    }

    // Rewrite each emitted MEDIA path whose basename matches a canonical capture.
    let (media_files, _cleaned) = crate::media::extract_media(response);
    let mut repaired = response.to_string();
    for (emitted_path, _is_voice) in media_files {
        if let Some(canonical) =
            canonical_by_basename.get(&computer_use_capture_basename(&emitted_path))
        {
            if emitted_path != *canonical {
                repaired = repaired.replace(&emitted_path, canonical);
            }
        }
    }
    repaired
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_name_mapping_reads_function_and_fallbacks() {
        let messages = vec![serde_json::json!({
            "role": "assistant",
            "tool_calls": [
                {"id": "c1", "function": {"name": "computer_use"}},
                {"call_id": "c2", "name": "search"}
            ]
        })];
        let m = tool_name_by_call_id(&messages);
        assert_eq!(m.get("c1").map(String::as_str), Some("computer_use"));
        assert_eq!(m.get("c2").map(String::as_str), Some("search"));
    }

    #[test]
    fn basename_matches_generated_captures_only() {
        let uuid = "0123456789abcdef0123456789abcdef";
        assert_eq!(
            computer_use_capture_basename(&format!("/x/computer_use_{uuid}.png")),
            format!("computer_use_{uuid}.png")
        );
        assert_eq!(
            computer_use_capture_basename(&format!("C:\\y\\computer_use_{uuid}.JPEG")),
            format!("computer_use_{uuid}.jpeg")
        );
        assert_eq!(computer_use_capture_basename("/x/photo.png"), "");
    }

    #[test]
    fn iter_paths_from_dict_and_summary() {
        let uuid = "0123456789abcdef0123456789abcdef";
        let content = serde_json::json!({
            "screenshot_path": format!("/cache/computer_use_{uuid}.png"),
            "summary": format!("done (shareable screenshot saved to /cache/computer_use_{uuid}.png)")
        });
        let mut out = Vec::new();
        iter_capture_paths(&content, &mut out);
        assert!(out
            .iter()
            .any(|p| p.contains(&format!("computer_use_{uuid}.png"))));
    }

    #[test]
    fn no_media_tag_is_unchanged() {
        let messages: Vec<Value> = vec![];
        assert_eq!(
            repair_explicit_computer_use_media_paths("plain reply", &messages, 0),
            "plain reply"
        );
    }

    #[test]
    fn repairs_mangled_path_to_canonical() {
        // The tool persisted a Windows path; the model emitted a POSIX-looking
        // MEDIA path with the same generated basename. Repair rewrites it.
        let uuid = "0123456789abcdef0123456789abcdef";
        let canonical = format!("C:\\Users\\A\\.hermes\\cache\\images\\computer_use_{uuid}.png");
        let mangled = format!("/Users/A/.hermes/cache/images/computer_use_{uuid}.png");
        let messages = vec![
            serde_json::json!({
                "role": "assistant",
                "tool_calls": [{"id": "c1", "function": {"name": "computer_use"}}]
            }),
            serde_json::json!({
                "role": "tool",
                "tool_call_id": "c1",
                "content": {"screenshot_path": canonical}
            }),
        ];
        let response = format!("Here you go.\nMEDIA:{mangled}");
        let repaired = repair_explicit_computer_use_media_paths(&response, &messages, 0);
        assert!(repaired.contains(&canonical), "repaired: {repaired}");
        assert!(!repaired.contains(&mangled));
    }
}
