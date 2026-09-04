//! Port of the display-text helpers + config of gateway/stream_consumer.py.
//!
// Public API is ahead of its callers (the stream consumer wires it).
#![allow(dead_code)]
//!
//! The adapter-independent pieces of the streaming consumer: the code-fence
//! display helpers (escape a `` ``` `` so text can be wrapped in an outer fence;
//! close orphaned code/inline-code markers on truncated output) and
//! `StreamConsumerConfig`. `GatewayStreamConsumer` itself is the async sink that
//! progressively edits a platform message and hangs off the adapter render
//! hooks, so it lands with the adapter subsystem.

use std::sync::OnceLock;

use fancy_regex::Regex;

/// Config defaults (gateway/config.py).
pub const DEFAULT_STREAMING_EDIT_INTERVAL: f64 = 0.8;
pub const DEFAULT_STREAMING_BUFFER_THRESHOLD: i64 = 24;
pub const DEFAULT_STREAMING_CURSOR: &str = " \u{2589}"; // " ▉"

/// Escape triple-backtick markers so text can be safely wrapped inside an outer
/// ``` code block without the inner fence breaking it. Unchanged when there is
/// no `` ``` ``.
pub fn escape_code_fences_for_display(text: &str) -> String {
    if !text.contains("```") {
        return text.to_string();
    }
    text.replace("```", "\\`\\`\\`")
}

fn complete_fences_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?s)```.*?```").unwrap())
}

fn trailing_open_fence_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"```[^`]*$").unwrap())
}

/// Append a closing `` ``` `` fence and/or `` ` `` when the text has orphaned
/// code-block or inline-code markers (from output truncated mid-fence). An odd
/// count of `` ``` `` gets a closing fence on its own line; then, after stripping
/// complete ```…``` regions, an odd count of standalone `` ` `` gets a closing
/// backtick.
pub fn ensure_closed_code_fences(text: &str) -> String {
    if text.is_empty() {
        return text.to_string();
    }
    let mut out = text.to_string();

    // Step 1: balance triple-backtick fences.
    if out.matches("```").count() % 2 == 1 {
        out = format!("{}\n```", out.trim_end_matches('\n'));
    }

    // Step 2: balance single-backtick inline-code spans, ignoring backticks
    // inside complete ```…``` regions (and any trailing unclosed fence).
    let without = complete_fences_re().replace_all(&out, "");
    let without = trailing_open_fence_re().replace_all(&without, "");
    if without.matches('`').count() % 2 == 1 {
        out.push('`');
    }
    out
}

/// Streaming transport selection for a consumer.
#[derive(Debug, Clone, PartialEq)]
pub struct StreamConsumerConfig {
    pub edit_interval: f64,
    pub buffer_threshold: i64,
    pub cursor: String,
    pub buffer_only: bool,
    /// When > 0, deliver the final edit as a fresh message if the preview has
    /// been visible at least this long (so the platform timestamp reflects
    /// completion, not first-token, time). 0 = always edit in place.
    pub fresh_final_after_seconds: f64,
    /// "auto" | "draft" | "edit" | "off".
    pub transport: String,
    /// Originating chat type ("dm", "group", "forum", ...); gates native drafts.
    pub chat_type: String,
}

impl Default for StreamConsumerConfig {
    fn default() -> Self {
        Self {
            edit_interval: DEFAULT_STREAMING_EDIT_INTERVAL,
            buffer_threshold: DEFAULT_STREAMING_BUFFER_THRESHOLD,
            cursor: DEFAULT_STREAMING_CURSOR.to_string(),
            buffer_only: false,
            fresh_final_after_seconds: 0.0,
            transport: "edit".to_string(),
            chat_type: String::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escape_fences() {
        assert_eq!(escape_code_fences_for_display("no fences"), "no fences");
        assert_eq!(
            escape_code_fences_for_display("a ```rust\nx\n``` b"),
            "a \\`\\`\\`rust\nx\n\\`\\`\\` b"
        );
    }

    #[test]
    fn closes_orphaned_triple_fence() {
        // One opening fence, truncated -> a closing fence is appended on its
        // own line.
        let out = ensure_closed_code_fences("here:\n```rust\nlet x = 1;");
        assert!(out.ends_with("\n```"), "got {out:?}");
        assert_eq!(out.matches("```").count(), 2);
    }

    #[test]
    fn balanced_fences_unchanged() {
        let t = "```\ncode\n```";
        assert_eq!(ensure_closed_code_fences(t), t);
        assert_eq!(ensure_closed_code_fences(""), "");
    }

    #[test]
    fn closes_orphaned_inline_backtick() {
        // A stray inline-code backtick outside any fenced block gets closed.
        let out = ensure_closed_code_fences("use the `foo function");
        assert!(out.ends_with('`'));
        // Backticks inside a complete fenced block don't trigger a stray close.
        let fenced = "```\na ` b\n```";
        assert_eq!(ensure_closed_code_fences(fenced), fenced);
    }

    #[test]
    fn config_defaults() {
        let c = StreamConsumerConfig::default();
        assert_eq!(c.edit_interval, 0.8);
        assert_eq!(c.buffer_threshold, 24);
        assert_eq!(c.cursor, " \u{2589}");
        assert_eq!(c.transport, "edit");
    }
}
