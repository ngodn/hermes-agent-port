//! Port of gateway/kanban_watchers.py.
//!
// Public API is ahead of its callers (wired later).
#![allow(dead_code)]
//! Self-contained slices of the kanban watcher/notifier/dispatcher module.
//!
//! The Python file is dominated by two long async loops
//! (`_kanban_notifier_watcher`, `_kanban_dispatcher_watcher`) plus a
//! `GatewayKanbanWatchersMixin` whose methods read `self` state on the
//! `GatewayRunner`, drive adapters, wake sessions, and call into `kanban_db`,
//! `kanban_decompose`, `agent.estop`, `agent.redact`, `agent.i18n`,
//! `gateway.wake` and `gateway.session`. All of that is coupled to types that
//! are not ported yet, so it is left for later.
//!
//! What lives here is the pure, plain-data logic that can be lifted out and
//! tested on its own: live auto-decompose config resolution, the terminal-event
//! notification text builder, the review-reason scrubber (path redaction plus
//! whitespace/length normalisation), the delivery-metadata scope lookup, the
//! corrupt-board error-message predicate, and the shared constants those loops
//! reference. See the per-item docs for the exact Python source and any small
//! documented deviations (mainly: `agent.redact.redact_sensitive_text` is not
//! yet ported, so the review scrubber applies only the path/whitespace/length
//! pass that follows redaction upstream).

use std::sync::OnceLock;

use fancy_regex::Regex;
use serde_json::Value;

// ── shared constants (used by the coupled loops, wired later) ────────────────

/// Event kinds the notifier claims and advances the cursor past. Terminal set
/// from `_kanban_notifier_watcher`. `archived`/`unblocked` are claimed (so the
/// cursor moves past them) but produce no user message; see
/// `format_terminal_notification` returning `None` for them.
pub const TERMINAL_KINDS: [&str; 11] = [
    "completed",
    "blocked",
    "gave_up",
    "crashed",
    "timed_out",
    "status",
    "archived",
    "unblocked",
    "block_loop_detected",
    "review_requested",
    "changes_requested",
];

/// Kinds that hand a decision back to the origin session, so a `notify+wake` /
/// `wake` subscription injects a synthetic turn. From `_WAKE_KINDS`.
pub const WAKE_KINDS: [&str; 8] = [
    "completed",
    "gave_up",
    "crashed",
    "timed_out",
    "blocked",
    "review_requested",
    "changes_requested",
    "block_loop_detected",
];

/// Consecutive send/wake failures before a subscription is dropped
/// (`MAX_SEND_FAILURES`, ~60s at the 5s tick cadence).
pub const MAX_SEND_FAILURES: u32 = 12;

/// Stale done-sub GC cadence in seconds (`_GC_INTERVAL_SECONDS`).
pub const GC_INTERVAL_SECONDS: f64 = 3600.0;

/// Shipped default retention (days) for stale done/blocked subscriptions when
/// `kanban.done_sub_retention_days` is unreadable.
pub const DEFAULT_DONE_SUB_RETENTION_DAYS: i64 = 30;

/// Dispatcher "stuck" telemetry window: ready queue non-empty for this many
/// consecutive ticks with zero spawns triggers a warning (`HEALTH_WINDOW`).
pub const HEALTH_WINDOW: u32 = 6;

/// How long a board flagged as a corrupt SQLite file stays quarantined before
/// the dispatcher retries it (`CORRUPT_BOARD_RETRY_AFTER_SECONDS`).
pub const CORRUPT_BOARD_RETRY_AFTER_SECONDS: f64 = 300.0;

// ── Python-truthiness / coercion helpers ─────────────────────────────────────

/// Python `bool(value)` truthiness for a JSON value: null/false/0/""/[]/{} are
/// falsy, everything else (including the string "false") is truthy.
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

/// Python `str(value)` for the JSON shapes that reach these formatters.
/// Scalars match CPython exactly (`True`/`False`/`None`, `3`, `3.0`); arrays
/// and objects fall back to compact JSON rather than a Python `repr`, a
/// documented simplification for shapes that do not occur in these payloads.
fn py_str(v: &Value) -> String {
    match v {
        Value::Null => "None".to_string(),
        Value::Bool(true) => "True".to_string(),
        Value::Bool(false) => "False".to_string(),
        Value::Number(n) => n.to_string(),
        Value::String(s) => s.clone(),
        other => serde_json::to_string(other).unwrap_or_default(),
    }
}

/// Python `int(value)`: truncates numbers toward zero, parses integer-looking
/// strings, maps `True`/`False` to 1/0. Returns `None` for the cases CPython
/// would raise `TypeError`/`ValueError` on (non-integer strings, null,
/// collections).
fn py_int(v: &Value) -> Option<i64> {
    match v {
        Value::Bool(b) => Some(if *b { 1 } else { 0 }),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Some(i)
            } else {
                n.as_f64().map(|f| f.trunc() as i64)
            }
        }
        Value::String(s) => {
            // Python int() strips surrounding whitespace and allows a single
            // leading sign and underscores between digits. Be lenient on
            // underscores; reject anything else non-integer.
            let t = s.trim().replace('_', "");
            if t.is_empty() {
                return None;
            }
            t.parse::<i64>().ok()
        }
        _ => None,
    }
}

// ── auto-decompose settings (from _resolve_auto_decompose_settings) ──────────

/// Resolve the live `(enabled, per_tick)` auto-decompose settings from config.
///
/// Faithful port of `_resolve_auto_decompose_settings`. The Python function
/// takes a `load_config` callable that may raise; here `cfg` is the already
/// loaded config, and `None` represents that read having failed.
///
/// Fail modes match Python exactly:
///   * `cfg = None` (load raised) -> `(false, 3)`, never re-enabling a feature
///     the user turned off and never falling back to burst-prone default-on.
///   * `cfg` present but not an object, or with no usable `kanban` map ->
///     defaults `(true, 3)` (an empty `kanban` dict yields
///     `bool(True)`/`int(3)`).
///
/// `per_tick` is clamped to `>= 1`.
pub fn resolve_auto_decompose_settings(cfg: Option<&Value>) -> (bool, i64) {
    // Python: `except Exception: return False, 3` around the config read.
    let Some(cfg) = cfg else {
        return (false, 3);
    };
    // Python: `cfg.get("kanban", {}) if isinstance(cfg, dict) else {}`. A
    // non-object config, or a `kanban` that is not itself a map, collapses to
    // an empty map so the `.get(..., default)` calls below supply the
    // defaults. (CPython would raise on a non-dict `kanban`; failing safe to
    // defaults keeps us panic-free per the port rules.)
    let empty = serde_json::Map::new();
    let kcfg = cfg
        .as_object()
        .and_then(|m| m.get("kanban"))
        .and_then(|k| k.as_object())
        .unwrap_or(&empty);

    // enabled = bool(kcfg.get("auto_decompose", True))
    let enabled = match kcfg.get("auto_decompose") {
        Some(v) => is_truthy(v),
        None => true,
    };

    // per_tick = int(kcfg.get("auto_decompose_per_tick", 3) or 3)
    // The `or 3` swaps a falsy value for 3 before int(); a non-int raises,
    // caught as 3; then clamp >= 1.
    let raw = kcfg.get("auto_decompose_per_tick");
    let per_tick = match raw {
        // Missing key -> default 3 (int(3)).
        None => 3,
        // Present but falsy -> `or 3` -> int(3) = 3.
        Some(v) if !is_truthy(v) => 3,
        // Present and truthy -> int(v), TypeError/ValueError -> 3.
        Some(v) => py_int(v).unwrap_or(3),
    };
    let per_tick = if per_tick < 1 { 1 } else { per_tick };

    (enabled, per_tick)
}

// ── review-reason scrubbing (partial port of _safe_review_reason) ────────────

/// `_LOCAL_PATH_RE`: bare absolute local paths (POSIX roots or Windows drive
/// paths) not already glued to a word/scheme, used to strip filesystem detail
/// from externally delivered text.
fn local_path_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"(?<![\w:/])(?:/(?:Users|home|private|tmp|var|etc|workspace)/[^\s,;]+|[A-Za-z]:\\[^\s,;]+)",
        )
        .unwrap()
    })
}

/// The post-redaction tail of `_safe_review_reason`: replace bare local paths
/// with `[local path]`, collapse all whitespace runs to single spaces, and
/// truncate to `limit` characters with a trailing ellipsis.
///
/// DEVIATION: the Python function first runs
/// `agent.redact.redact_sensitive_text(..., force=True,
/// redact_url_credentials=True)`. `agent.redact` is not ported yet, so this
/// applies only the path/whitespace/length pass that follows it. Wire the
/// redaction step ahead of this call when that module lands.
pub fn scrub_review_reason(text: &str, limit: usize) -> String {
    // reason = _LOCAL_PATH_RE.sub("[local path]", reason)
    let replaced = local_path_re().replace_all(text, "[local path]");
    // reason = " ".join(reason.split())  -> collapse whitespace runs.
    let collapsed = replaced.split_whitespace().collect::<Vec<_>>().join(" ");
    // if len(reason) > limit: reason = reason[: limit - 1].rstrip() + "…"
    if collapsed.chars().count() > limit {
        let head: String = collapsed.chars().take(limit.saturating_sub(1)).collect();
        format!("{}\u{2026}", head.trim_end())
    } else {
        collapsed
    }
}

/// `_safe_review_reason(value, limit)` minus the redaction step: `None`
/// becomes `""`, any other value is stringified, then scrubbed. See
/// `scrub_review_reason` for the redaction deviation.
fn safe_review_reason_value(value: Option<&Value>, limit: usize) -> String {
    let text = match value {
        None | Some(Value::Null) => String::new(),
        Some(v) => py_str(v),
    };
    scrub_review_reason(&text, limit)
}

// ── wake scope lookup (metadata half of _wake_scope_id) ──────────────────────

/// Return the tenant scope a subscription's wake keys to, from its persisted
/// `delivery_metadata` only.
///
/// This is the metadata half of `_wake_scope_id`: it checks
/// `delivery_metadata` for `scope_id` / `slack_team_id` / `team_id` and returns
/// the first truthy value as a string. The adapter-side fallback
/// (`adapter.scope_id_for_chat`) is coupled to the live adapter and is left for
/// the caller once adapters are ported; `None` here means "no scope in
/// metadata", so the caller should try that fallback next.
pub fn wake_scope_id_from_metadata(sub: &Value) -> Option<String> {
    let meta = sub.get("delivery_metadata")?.as_object()?;
    for key in ["scope_id", "slack_team_id", "team_id"] {
        if let Some(v) = meta.get(key) {
            if is_truthy(v) {
                return Some(py_str(v));
            }
        }
    }
    None
}

// ── corrupt-board predicate (message half of _is_corrupt_board_db_error) ─────

/// Whether a SQLite error message marks the board file as a corrupt/invalid
/// database, from `_is_corrupt_board_db_error`.
///
/// This is the message-string half only. The Python function also treats a
/// `kanban_db.KanbanDbCorruptError` instance as corrupt and gates the rest on
/// `isinstance(exc, sqlite3.DatabaseError)`; those type checks are coupled to
/// the unported `kanban_db` and to rusqlite's error typing, so the caller
/// applies this predicate to the message of an already-classified DB error.
pub fn is_corrupt_board_db_message(msg: &str) -> bool {
    let lower = msg.to_lowercase();
    lower.contains("file is not a database") || lower.contains("database disk image is malformed")
}

// ── terminal-event notification text (from _kanban_notifier_watcher) ─────────

/// Python universal-newlines first-line: the substring up to the first line
/// boundary in `str.splitlines()`'s set. Covers the common boundaries; exotic
/// separators beyond these are treated as non-breaking (a documented
/// simplification).
fn first_line(s: &str) -> &str {
    match s.find([
        '\n', '\r', '\u{0b}', '\u{0c}', '\u{1c}', '\u{1d}', '\u{1e}', '\u{85}', '\u{2028}',
        '\u{2029}',
    ]) {
        Some(i) => &s[..i],
        None => s,
    }
}

fn truncate_chars(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

/// `if ev.payload and ev.payload.get(key):` - the value at `key` only when the
/// payload itself is truthy and the value is truthy.
fn truthy_payload_field<'a>(payload: Option<&'a Value>, key: &str) -> Option<&'a Value> {
    let p = payload?;
    if !is_truthy(p) {
        return None;
    }
    let v = p.get(key)?;
    if is_truthy(v) {
        Some(v)
    } else {
        None
    }
}

/// Build the user-facing terminal-event message for one claimed event.
///
/// Faithful port of the per-kind `msg = ...` branches inside
/// `_kanban_notifier_watcher`'s event loop. The coupled coroutine computes the
/// plain inputs (identity tag, board tag, truncated title, the run summary /
/// legacy `task.result`) and passes the event's `payload`; this returns the
/// exact message string, or `None` for the silent/unknown kinds
/// (`archived`, `unblocked`, anything else) that the loop `continue`s past.
///
/// Inputs mirror the Python locals:
///   * `board_tag`  - `"[slug] "` or `""`.
///   * `tag`        - `"@assignee "` or `""` (empty for `changes_requested`,
///     which the source omits the identity prefix on).
///   * `title`      - already truncated to 120 chars by the caller.
///   * `task_result`- legacy `task.result` fallback for the `completed` branch.
///
/// DEVIATIONS: the `changes_requested` branch uses `scrub_review_reason` via
/// `safe_review_reason_value`, which omits the not-yet-ported redaction pass
/// (see `scrub_review_reason`). The `timed_out` branch defaults a
/// non-numeric `limit_seconds` to 0 rather than raising, staying panic-free.
#[allow(clippy::too_many_arguments)]
pub fn format_terminal_notification(
    kind: &str,
    board_tag: &str,
    tag: &str,
    task_id: &str,
    title: &str,
    task_result: Option<&str>,
    payload: Option<&Value>,
) -> Option<String> {
    let msg = match kind {
        "completed" => {
            let mut handoff = String::new();
            if let Some(summary) = truthy_payload_field(payload, "summary") {
                let s = py_str(summary);
                let stripped = s.trim();
                let h = if stripped.is_empty() {
                    truncate_chars(&s, 200)
                } else {
                    truncate_chars(first_line(stripped), 200)
                };
                handoff = format!("\n{h}");
            } else if let Some(result) = task_result.filter(|r| !r.is_empty()) {
                let stripped = result.trim();
                let r = if stripped.is_empty() {
                    truncate_chars(result, 160)
                } else {
                    truncate_chars(first_line(stripped), 160)
                };
                handoff = format!("\n{r}");
            }
            format!("\u{2714} {board_tag}{tag}Kanban {task_id} done \u{2014} {title}{handoff}")
        }
        "blocked" => {
            let reason = truthy_payload_field(payload, "reason")
                .map(|v| format!(": {}", truncate_chars(&py_str(v), 160)))
                .unwrap_or_default();
            format!("\u{23f8} {board_tag}{tag}Kanban {task_id} blocked{reason}")
        }
        "gave_up" => {
            let err = truthy_payload_field(payload, "error")
                .map(|v| format!("\n{}", truncate_chars(&py_str(v), 200)))
                .unwrap_or_default();
            format!(
                "\u{2716} {board_tag}{tag}Kanban {task_id} gave up after repeated spawn failures{err}"
            )
        }
        "crashed" => {
            format!(
                "\u{2716} {board_tag}{tag}Kanban {task_id} worker crashed (pid gone); dispatcher will retry"
            )
        }
        "timed_out" => {
            let limit = truthy_payload_field(payload, "limit_seconds")
                .and_then(py_int)
                .unwrap_or(0);
            format!(
                "\u{23f1} {board_tag}{tag}Kanban {task_id} timed out (max_runtime={limit}s); will retry"
            )
        }
        "status" => {
            let new_status = truthy_payload_field(payload, "status")
                .map(py_str)
                .unwrap_or_default();
            format!("\u{1f504} {board_tag}{tag}Kanban {task_id} \u{2192} {new_status}")
        }
        "review_requested" => {
            // NB: this branch truncates the whole summary (not just the first
            // line) for the visible ping.
            let handoff = truthy_payload_field(payload, "summary")
                .map(|v| format!("\n{}", truncate_chars(&py_str(v), 200)))
                .unwrap_or_default();
            format!(
                "\u{1f440} {board_tag}{tag}Kanban {task_id} ready for review \u{2014} {title}{handoff}"
            )
        }
        "changes_requested" => {
            // payload = ev.payload or {}
            let p = payload.filter(|p| is_truthy(p));
            let reason = safe_review_reason_value(p.and_then(|p| p.get("reason")), 160);
            let reviewer = safe_review_reason_value(p.and_then(|p| p.get("reviewer")), 48);
            let implementer = safe_review_reason_value(p.and_then(|p| p.get("implementer")), 48);
            let reason_text = if reason.is_empty() {
                "reviewer feedback requires changes".to_string()
            } else {
                reason
            };
            let mut provenance = String::new();
            if !reviewer.is_empty() {
                provenance.push_str(&format!(" \u{2014} reviewer @{reviewer}"));
            }
            if !implementer.is_empty() {
                provenance.push_str(&format!(" \u{2192} implementer @{implementer}"));
            }
            // No identity `tag` on this line, matching the source.
            format!(
                "\u{1f6d1} {board_tag}Kanban {task_id} review requested changes/BLOCK: {reason_text}{provenance}"
            )
        }
        "block_loop_detected" => {
            let mut reason = String::new();
            let mut recurrences: Option<&Value> = None;
            if let Some(p) = payload.filter(|p| is_truthy(p)) {
                if let Some(r) = p.get("reason").filter(|v| is_truthy(v)) {
                    reason = format!(": {}", truncate_chars(&py_str(r), 160));
                }
                recurrences = p.get("recurrences");
            }
            let rc = match recurrences {
                Some(v) if is_truthy(v) => {
                    format!(" (blocked {}x for the same cause)", py_str(v))
                }
                _ => String::new(),
            };
            format!(
                "\u{1f6d1} {board_tag}{tag}Kanban {task_id} routed to TRIAGE \u{2014} needs a human decision{rc}{reason}"
            )
        }
        // archived / unblocked are claimed for cursor advancement but stay
        // silent, and any unknown kind is skipped.
        _ => return None,
    };
    Some(msg)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn auto_decompose_load_failure_fails_closed() {
        assert_eq!(resolve_auto_decompose_settings(None), (false, 3));
    }

    #[test]
    fn auto_decompose_non_object_uses_defaults() {
        // Not a dict -> kcfg {} -> (True, 3), distinct from the load-failure
        // (False, 3).
        assert_eq!(
            resolve_auto_decompose_settings(Some(&json!("nope"))),
            (true, 3)
        );
        assert_eq!(
            resolve_auto_decompose_settings(Some(&json!({})),),
            (true, 3)
        );
    }

    #[test]
    fn auto_decompose_reads_values() {
        let cfg = json!({"kanban": {"auto_decompose": false, "auto_decompose_per_tick": 5}});
        assert_eq!(resolve_auto_decompose_settings(Some(&cfg)), (false, 5));
    }

    #[test]
    fn auto_decompose_string_false_is_truthy() {
        // Python bool("false") == True.
        let cfg = json!({"kanban": {"auto_decompose": "false"}});
        assert!(resolve_auto_decompose_settings(Some(&cfg)).0);
    }

    #[test]
    fn auto_decompose_per_tick_falsy_and_clamp() {
        // 0 is falsy -> `or 3` -> 3.
        let cfg = json!({"kanban": {"auto_decompose_per_tick": 0}});
        assert_eq!(resolve_auto_decompose_settings(Some(&cfg)).1, 3);
        // Truthy but < 1 after int() cannot happen from 0; negative stays and clamps.
        let cfg = json!({"kanban": {"auto_decompose_per_tick": -4}});
        assert_eq!(resolve_auto_decompose_settings(Some(&cfg)).1, 1);
        // Float truncates toward zero.
        let cfg = json!({"kanban": {"auto_decompose_per_tick": 2.9}});
        assert_eq!(resolve_auto_decompose_settings(Some(&cfg)).1, 2);
        // Non-int string -> ValueError -> 3.
        let cfg = json!({"kanban": {"auto_decompose_per_tick": "abc"}});
        assert_eq!(resolve_auto_decompose_settings(Some(&cfg)).1, 3);
        // Integer string parses.
        let cfg = json!({"kanban": {"auto_decompose_per_tick": "7"}});
        assert_eq!(resolve_auto_decompose_settings(Some(&cfg)).1, 7);
    }

    #[test]
    fn scrub_redacts_paths_and_collapses_whitespace() {
        let out = scrub_review_reason("see   /home/alice/secret.txt\nfor  details", 200);
        assert_eq!(out, "see [local path] for details");
    }

    #[test]
    fn scrub_windows_path() {
        let out = scrub_review_reason(r"open C:\Users\bob\file.txt now", 200);
        assert_eq!(out, "open [local path] now");
    }

    #[test]
    fn scrub_truncates_with_ellipsis() {
        let out = scrub_review_reason("abcdefghij", 5);
        // 10 chars > 5 -> first 4 chars + ellipsis.
        assert_eq!(out, "abcd\u{2026}");
    }

    #[test]
    fn scrub_no_truncation_at_limit() {
        let out = scrub_review_reason("abcde", 5);
        assert_eq!(out, "abcde");
    }

    #[test]
    fn wake_scope_prefers_metadata_keys_in_order() {
        let sub = json!({"delivery_metadata": {"team_id": "T2", "scope_id": "S1"}});
        assert_eq!(wake_scope_id_from_metadata(&sub), Some("S1".to_string()));
        let sub = json!({"delivery_metadata": {"slack_team_id": "T9"}});
        assert_eq!(wake_scope_id_from_metadata(&sub), Some("T9".to_string()));
    }

    #[test]
    fn wake_scope_skips_falsy_and_missing() {
        let sub = json!({"delivery_metadata": {"scope_id": "", "team_id": 0}});
        assert_eq!(wake_scope_id_from_metadata(&sub), None);
        let sub = json!({"chat_id": "c1"});
        assert_eq!(wake_scope_id_from_metadata(&sub), None);
        // Numeric scope stringifies like Python str().
        let sub = json!({"delivery_metadata": {"scope_id": 123}});
        assert_eq!(wake_scope_id_from_metadata(&sub), Some("123".to_string()));
    }

    #[test]
    fn corrupt_board_message_predicate() {
        assert!(is_corrupt_board_db_message("file is not a database"));
        assert!(is_corrupt_board_db_message(
            "Error: database disk image is MALFORMED"
        ));
        assert!(!is_corrupt_board_db_message("database is locked"));
    }

    #[test]
    fn notif_completed_prefers_summary_first_line() {
        let payload = json!({"summary": "Fixed the bug\nmore detail"});
        let msg = format_terminal_notification(
            "completed",
            "[main] ",
            "@alice ",
            "T-1",
            "Do the thing",
            Some("legacy result"),
            Some(&payload),
        )
        .unwrap();
        assert_eq!(
            msg,
            "\u{2714} [main] @alice Kanban T-1 done \u{2014} Do the thing\nFixed the bug"
        );
    }

    #[test]
    fn notif_completed_falls_back_to_task_result() {
        let msg = format_terminal_notification(
            "completed",
            "",
            "",
            "T-2",
            "Title",
            Some("legacy line\nhidden"),
            None,
        )
        .unwrap();
        assert_eq!(msg, "\u{2714} Kanban T-2 done \u{2014} Title\nlegacy line");
    }

    #[test]
    fn notif_blocked_with_reason() {
        let payload = json!({"reason": "needs creds"});
        let msg =
            format_terminal_notification("blocked", "", "@bot ", "T-3", "T", None, Some(&payload))
                .unwrap();
        assert_eq!(msg, "\u{23f8} @bot Kanban T-3 blocked: needs creds");
    }

    #[test]
    fn notif_timed_out_reads_limit() {
        let payload = json!({"limit_seconds": 90});
        let msg =
            format_terminal_notification("timed_out", "", "", "T-4", "T", None, Some(&payload))
                .unwrap();
        assert_eq!(
            msg,
            "\u{23f1} Kanban T-4 timed out (max_runtime=90s); will retry"
        );
        // Missing limit defaults to 0.
        let msg =
            format_terminal_notification("timed_out", "", "", "T-4", "T", None, None).unwrap();
        assert!(msg.contains("max_runtime=0s"));
    }

    #[test]
    fn notif_status_transition() {
        let payload = json!({"status": "review"});
        let msg =
            format_terminal_notification("status", "[b] ", "", "T-5", "T", None, Some(&payload))
                .unwrap();
        assert_eq!(msg, "\u{1f504} [b] Kanban T-5 \u{2192} review");
    }

    #[test]
    fn notif_review_requested_uses_whole_summary() {
        let payload = json!({"summary": "line one\nline two"});
        let msg = format_terminal_notification(
            "review_requested",
            "",
            "",
            "T-6",
            "Feature",
            None,
            Some(&payload),
        )
        .unwrap();
        // review_requested keeps the newline (whole summary truncated), unlike
        // completed which takes only the first line.
        assert_eq!(
            msg,
            "\u{1f440} Kanban T-6 ready for review \u{2014} Feature\nline one\nline two"
        );
    }

    #[test]
    fn notif_changes_requested_provenance_and_default() {
        let payload = json!({"reviewer": "rev", "implementer": "imp"});
        let msg = format_terminal_notification(
            "changes_requested",
            "[m] ",
            "@ignored ",
            "T-7",
            "T",
            None,
            Some(&payload),
        )
        .unwrap();
        // No reason -> default text; no identity tag on this line.
        assert_eq!(
            msg,
            "\u{1f6d1} [m] Kanban T-7 review requested changes/BLOCK: reviewer feedback requires changes \u{2014} reviewer @rev \u{2192} implementer @imp"
        );
    }

    #[test]
    fn notif_block_loop_with_recurrences() {
        let payload = json!({"reason": "same error", "recurrences": 4});
        let msg = format_terminal_notification(
            "block_loop_detected",
            "",
            "@w ",
            "T-8",
            "T",
            None,
            Some(&payload),
        )
        .unwrap();
        assert_eq!(
            msg,
            "\u{1f6d1} @w Kanban T-8 routed to TRIAGE \u{2014} needs a human decision (blocked 4x for the same cause): same error"
        );
    }

    #[test]
    fn notif_silent_and_unknown_kinds_return_none() {
        assert!(format_terminal_notification("archived", "", "", "T", "T", None, None).is_none());
        assert!(format_terminal_notification("unblocked", "", "", "T", "T", None, None).is_none());
        assert!(format_terminal_notification("bogus", "", "", "T", "T", None, None).is_none());
    }
}
