//! Port of gateway/platforms/helpers.py.
//!
// The public surface here lands ahead of the platform adapters that call it.
#![allow(dead_code)]
//!
//! Shared helper types and functions for gateway platform adapters: message
//! deduplication, text batch aggregation, markdown stripping, thread
//! participation tracking, GFM table -> bullet conversion, mention-pattern
//! compilation, and the fence-aware markdown chunking core.
//!
//! Faithfulness notes (these are the traps this module is built around):
//!
//!  * Python indexes, slices and measures strings by Unicode CODE POINT, and
//!    `len()` is a code-point count. Every splitter here therefore works over
//!    code-point offsets: [`cp_len`], [`cp_prefix`], [`cp_suffix_from`] and
//!    [`cp_rfind`] convert to/from byte offsets at the edges only. Using byte
//!    offsets would silently corrupt multibyte / emoji text.
//!  * Several functions take an optional `len_fn` (Python's `len_fn=None`
//!    default). It is modeled as `Option<&LenFn>` where `LenFn = dyn Fn(&str)
//!    -> usize`; `None` means Python's builtin `len`, i.e. `chars().count()`.
//!    `split_at_paragraph_boundary` has a Python fast path guarded by
//!    `if _len is len:` (identity against the builtin), which maps exactly to
//!    `len_fn.is_none()` here.
//!  * `str.rfind` returning `-1` and the `pos > 0` guards that follow it are
//!    preserved literally: an `Option<usize>` of `None` or `Some(0)` both fall
//!    through to the next split strategy.
//!  * Regexes use `fancy-regex` (this crate's regex dependency; there is no
//!    plain `regex` crate here). `re.DOTALL` becomes `(?s)`, `re.MULTILINE`
//!    becomes `(?m)`, `re.IGNORECASE` becomes `(?i)`.
//!
//! Two small pieces the Python module reaches out for are inlined here rather
//! than left as cross-module calls, because their sources are not ported yet
//! and both are tiny and self-contained:
//!
//!  * `split_markdown_table_row` delegates to `agent.markdown_tables.
//!    split_table_row`; the body (5 lines, no `wcwidth` involvement) is inlined.
//!  * `_chunk_newline_preferred` does a local import of
//!    `gateway.platforms.base._custom_unit_to_cp`; that helper is inlined as
//!    [`custom_unit_to_cp`]. When `gateway/platforms/base.py`'s remaining half
//!    is ported this should collapse to one shared copy.

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex, OnceLock};

use fancy_regex::Regex;
use serde_json::Value;
use tokio::task::JoinHandle;
use tracing::{info, warn};

use crate::platform_base_types::MessageEvent;

// ─── Code-point helpers ──────────────────────────────────────────────────────

/// Python's `len(s)`: a count of Unicode code points, not bytes.
#[inline]
pub fn cp_len(s: &str) -> usize {
    s.chars().count()
}

/// Byte offset of the `n`-th code point (or `s.len()` when `n` is past the end).
#[inline]
fn byte_of_cp(s: &str, n: usize) -> usize {
    match s.char_indices().nth(n) {
        Some((i, _)) => i,
        None => s.len(),
    }
}

/// Python's `s[:n]` with `n` in code points.
#[inline]
pub fn cp_prefix(s: &str, n: usize) -> &str {
    &s[..byte_of_cp(s, n)]
}

/// Python's `s[n:]` with `n` in code points.
#[inline]
pub fn cp_suffix_from(s: &str, n: usize) -> &str {
    &s[byte_of_cp(s, n)..]
}

/// Python's `s[-n:]` with `n` in code points.
#[inline]
fn cp_last(s: &str, n: usize) -> &str {
    let total = cp_len(s);
    cp_suffix_from(s, total.saturating_sub(n))
}

/// Python's `haystack.rfind(needle)`: the code-point index of the last
/// occurrence, or `None` for Python's `-1`.
#[inline]
fn cp_rfind(haystack: &str, needle: &str) -> Option<usize> {
    haystack
        .rfind(needle)
        .map(|b| haystack[..b].chars().count())
}

/// A custom length function. Python passes these as `len_fn=`; `None` means the
/// builtin `len`, i.e. a code-point count.
pub type LenFn<'a> = dyn Fn(&str) -> usize + 'a;

/// Python's `overflow=` hook on `greedy_pack_blocks`: splits an oversized
/// block that cannot fit the budget on its own.
pub type OverflowFn<'a> = dyn Fn(&str) -> Vec<String> + 'a;

/// Apply `len_fn` if present, else Python's builtin `len`.
#[inline]
fn measure(len_fn: Option<&LenFn>, s: &str) -> usize {
    match len_fn {
        Some(f) => f(s),
        None => cp_len(s),
    }
}

/// Largest code-point offset `n` with `len_fn(s[:n]) <= budget`.
///
/// Inlined copy of `gateway/platforms/base.py::_custom_unit_to_cp`, which
/// `_chunk_newline_preferred` imports locally. Binary search, O(log n) calls to
/// `len_fn`.
pub fn custom_unit_to_cp(s: &str, budget: usize, len_fn: Option<&LenFn>) -> usize {
    if measure(len_fn, s) <= budget {
        return cp_len(s);
    }
    let (mut lo, mut hi) = (0usize, cp_len(s));
    while lo < hi {
        let mid = (lo + hi).div_ceil(2);
        if measure(len_fn, cp_prefix(s, mid)) <= budget {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    lo
}

// ─── Message Deduplication ───────────────────────────────────────────────────

/// TTL-based message deduplication cache.
///
/// Replaces the identical `_seen_messages` / `_is_duplicate()` pattern that was
/// duplicated across the discord, slack, dingtalk, wecom, weixin, mattermost
/// and feishu adapters.
///
/// Python backs this with a `dict`, which is insertion-ordered, and the overflow
/// prune does a *stable* `sorted(..., key=timestamp)[-max_size:]`. Ties in the
/// timestamp therefore fall back to insertion order. This port keeps a
/// monotonically increasing `seq` per entry to reproduce that tiebreak exactly
/// while still getting O(1) lookups from a `HashMap`.
pub struct MessageDeduplicator {
    seen: HashMap<String, SeenEntry>,
    max_size: usize,
    ttl: f64,
    next_seq: u64,
}

#[derive(Clone, Copy)]
struct SeenEntry {
    at: f64,
    seq: u64,
}

/// Python's `time.time()`: seconds since the epoch as a float.
fn now_seconds() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

impl Default for MessageDeduplicator {
    /// Python's `MessageDeduplicator()` defaults: `max_size=2000`,
    /// `ttl_seconds=300`.
    fn default() -> Self {
        Self::new(2000, 300.0)
    }
}

impl MessageDeduplicator {
    pub fn new(max_size: usize, ttl_seconds: f64) -> Self {
        Self {
            seen: HashMap::new(),
            max_size,
            ttl: ttl_seconds,
            next_seq: 0,
        }
    }

    fn insert_fresh(&mut self, msg_id: &str, now: f64) {
        let seq = self.next_seq;
        self.next_seq = self.next_seq.wrapping_add(1);
        self.seen
            .insert(msg_id.to_string(), SeenEntry { at: now, seq });
    }

    /// True when `msg_id` was already seen within the TTL window. Otherwise the
    /// id is recorded (claimed) and `false` is returned.
    pub fn is_duplicate(&mut self, msg_id: &str) -> bool {
        // Python: `if not msg_id` — the empty string is falsy.
        if msg_id.is_empty() {
            return false;
        }
        let now = now_seconds();
        if let Some(entry) = self.seen.get(msg_id).copied() {
            if now - entry.at < self.ttl {
                return true;
            }
            // Expired: drop it and treat the id as new.
            self.seen.remove(msg_id);
        }
        self.insert_fresh(msg_id, now);
        if self.seen.len() > self.max_size {
            let cutoff = now - self.ttl;
            self.seen.retain(|_, v| v.at > cutoff);
            if self.seen.len() > self.max_size {
                // TTL pruning alone does not cap the cache when every entry is
                // still fresh. Keep the newest entries so the max_size bound is
                // enforced under sustained traffic.
                let mut items: Vec<(String, SeenEntry)> =
                    self.seen.iter().map(|(k, v)| (k.clone(), *v)).collect();
                // Python sorts by timestamp only, and `sorted` is stable, so
                // equal timestamps keep dict insertion order: (at, seq).
                items.sort_by(|a, b| {
                    a.1.at
                        .partial_cmp(&b.1.at)
                        .unwrap_or(std::cmp::Ordering::Equal)
                        .then(a.1.seq.cmp(&b.1.seq))
                });
                let drop_count = items.len() - self.max_size;
                self.seen = items.into_iter().skip(drop_count).collect();
            }
        }
        false
    }

    /// Whether `msg_id` is live in the cache, without inserting it. Expired
    /// entries are evicted as a side effect, exactly as in Python.
    pub fn contains(&mut self, msg_id: &str) -> bool {
        if msg_id.is_empty() {
            return false;
        }
        let seen_at = match self.seen.get(msg_id) {
            Some(e) => e.at,
            None => return false,
        };
        if now_seconds() - seen_at < self.ttl {
            return true;
        }
        self.seen.remove(msg_id);
        false
    }

    /// Release a claimed message id after a cancelled/failed handoff.
    pub fn discard(&mut self, msg_id: &str) {
        self.seen.remove(msg_id);
    }

    pub fn clear(&mut self) {
        self.seen.clear();
    }

    /// Live entry count. Not in the Python API; used by tests.
    pub fn len(&self) -> usize {
        self.seen.len()
    }

    pub fn is_empty(&self) -> bool {
        self.seen.is_empty()
    }
}

// ─── Text Batch Aggregation ──────────────────────────────────────────────────

/// The async handler a [`TextBatchAggregator`] dispatches batched events to.
/// Python passes a coroutine function taking one `MessageEvent`.
pub type BatchHandler =
    dyn Fn(MessageEvent) -> Pin<Box<dyn Future<Output = ()> + Send>> + Send + Sync;

struct PendingBatch {
    event: MessageEvent,
    /// Python stashes this as a dynamic `event._last_chunk_len` attribute.
    /// `MessageEvent` is a fixed struct here, so it rides alongside instead.
    last_chunk_len: usize,
}

#[derive(Default)]
struct BatchState {
    pending: HashMap<String, PendingBatch>,
    /// key -> (generation, task handle). The generation stands in for Python's
    /// `self._pending_tasks.get(key) is current_task` identity check: a tokio
    /// task can start before its `JoinHandle` is stored, so the flush compares
    /// generations rather than handle identity.
    tasks: HashMap<String, (u64, JoinHandle<()>)>,
    next_gen: u64,
}

/// Aggregates rapid-fire text events into single messages.
///
/// Replaces the `_enqueue_text_event` / `_flush_text_batch` pattern that was
/// duplicated in telegram, discord, matrix, wecom and feishu.
///
/// Methods that spawn take `self: &Arc<Self>` because the flush task needs to
/// outlive the `enqueue` call, which is where Python's `asyncio.create_task`
/// hands the bound method to the loop.
pub struct TextBatchAggregator {
    handler: Arc<BatchHandler>,
    batch_delay: f64,
    split_delay: f64,
    split_threshold: usize,
    state: Mutex<BatchState>,
}

impl TextBatchAggregator {
    /// Python defaults: `batch_delay=0.6`, `split_delay=2.0`,
    /// `split_threshold=4000`.
    pub fn new(handler: Arc<BatchHandler>) -> Arc<Self> {
        Self::with_options(handler, 0.6, 2.0, 4000)
    }

    pub fn with_options(
        handler: Arc<BatchHandler>,
        batch_delay: f64,
        split_delay: f64,
        split_threshold: usize,
    ) -> Arc<Self> {
        Arc::new(Self {
            handler,
            batch_delay,
            split_delay,
            split_threshold,
            state: Mutex::new(BatchState::default()),
        })
    }

    /// True when batching is active (delay > 0).
    pub fn is_enabled(&self) -> bool {
        self.batch_delay > 0.0
    }

    /// Add `event` to the pending batch for `key`, restarting the flush timer.
    ///
    /// Must be called from inside a tokio runtime (Python requires a running
    /// event loop for `asyncio.create_task` in the same way).
    pub fn enqueue(self: &Arc<Self>, event: MessageEvent, key: &str) {
        // Python: `chunk_len = len(event.text or "")`.
        let chunk_len = cp_len(&event.text);
        let generation = {
            let mut st = self.state.lock().unwrap();
            match st.pending.get_mut(key) {
                // A `MessageEvent` dataclass is always truthy, so Python's
                // `if not existing` only fires for a missing key.
                Some(existing) => {
                    existing.event.text = format!("{}\n{}", existing.event.text, event.text);
                    existing.last_chunk_len = chunk_len;
                }
                None => {
                    st.pending.insert(
                        key.to_string(),
                        PendingBatch {
                            event,
                            last_chunk_len: chunk_len,
                        },
                    );
                }
            }

            // Cancel the prior flush timer.
            if let Some((_, prior)) = st.tasks.get(key) {
                if !prior.is_finished() {
                    prior.abort();
                }
            }
            let generation = st.next_gen;
            st.next_gen = st.next_gen.wrapping_add(1);
            generation
        };

        let this = Arc::clone(self);
        let owned_key = key.to_string();
        let handle = tokio::spawn(async move {
            this.flush(owned_key, generation).await;
        });
        let mut st = self.state.lock().unwrap();
        st.tasks.insert(key.to_string(), (generation, handle));
    }

    async fn flush(self: Arc<Self>, key: String, generation: u64) {
        let last_len = {
            let st = self.state.lock().unwrap();
            st.pending.get(&key).map(|p| p.last_chunk_len).unwrap_or(0)
        };

        // Longer delay when the last chunk looks like a split message.
        let delay = if last_len >= self.split_threshold {
            self.split_delay
        } else {
            self.batch_delay
        };
        // `asyncio.sleep` on a negative delay yields immediately; `Duration`
        // cannot be negative, so clamp.
        tokio::time::sleep(std::time::Duration::from_secs_f64(delay.max(0.0))).await;

        let event = {
            let mut st = self.state.lock().unwrap();
            st.pending.remove(&key)
        };
        if let Some(p) = event {
            (self.handler)(p.event).await;
        }

        let mut st = self.state.lock().unwrap();
        if st.tasks.get(&key).map(|(g, _)| *g) == Some(generation) {
            st.tasks.remove(&key);
        }
    }

    /// Cancel all pending flush tasks and drop the pending batches.
    pub fn cancel_all(&self) {
        let mut st = self.state.lock().unwrap();
        for (_, handle) in st.tasks.values() {
            if !handle.is_finished() {
                handle.abort();
            }
        }
        st.tasks.clear();
        st.pending.clear();
    }

    /// Peek at the pending batch text for `key`. Not in the Python API; tests
    /// use it to observe the aggregation without racing the flush.
    pub fn pending_text(&self, key: &str) -> Option<String> {
        let st = self.state.lock().unwrap();
        st.pending.get(key).map(|p| p.event.text.clone())
    }
}

// ─── Markdown Stripping ──────────────────────────────────────────────────────

macro_rules! lazy_re {
    ($name:ident, $pat:expr) => {
        fn $name() -> &'static Regex {
            static RE: OnceLock<Regex> = OnceLock::new();
            RE.get_or_init(|| Regex::new($pat).expect("static regex"))
        }
    };
}

// `re.DOTALL` -> `(?s)`, `re.MULTILINE` -> `(?m)`.
lazy_re!(re_bold, r"(?s)\*\*(.+?)\*\*");
lazy_re!(re_italic_star, r"(?s)\*(.+?)\*");
lazy_re!(re_bold_under, r"(?s)\b__(?![\s_])(.+?)(?<![\s_])__\b");
lazy_re!(re_italic_under, r"(?s)\b_(?![\s_])(.+?)(?<![\s_])_\b");
lazy_re!(re_code_block, r"```[a-zA-Z0-9_+-]*\n?");
lazy_re!(re_inline_code, r"`(.+?)`");
lazy_re!(re_heading, r"(?m)^#{1,6}\s+");
lazy_re!(re_link, r"\[([^\]]+)\]\([^\)]+\)");
lazy_re!(re_multi_newline, r"\n{3,}");

/// Strip markdown formatting for plain-text platforms (SMS, iMessage, ...).
///
/// Replaces the identical `_strip_markdown()` functions that were duplicated in
/// sms.py, bluebubbles.py and feishu.py.
pub fn strip_markdown(text: &str) -> String {
    let t = re_bold().replace_all(text, "${1}").into_owned();
    let t = re_italic_star().replace_all(&t, "${1}").into_owned();
    let t = re_bold_under().replace_all(&t, "${1}").into_owned();
    let t = re_italic_under().replace_all(&t, "${1}").into_owned();
    let t = re_code_block().replace_all(&t, "").into_owned();
    let t = re_inline_code().replace_all(&t, "${1}").into_owned();
    let t = re_heading().replace_all(&t, "").into_owned();
    let t = re_link().replace_all(&t, "${1}").into_owned();
    let t = re_multi_newline().replace_all(&t, "\n\n").into_owned();
    t.trim().to_string()
}

// ─── Thread Participation Tracking ───────────────────────────────────────────

/// Persistent tracking of threads the bot has participated in.
///
/// Replaces the identical `_load/_save_participated_threads` +
/// `_mark_thread_participated` pattern from discord.py and matrix.py.
///
/// Python holds a `dict[str, None]`, so ordering is insertion order and the
/// `_save` truncation keeps the *newest* `max_tracked` ids. This port carries an
/// ordered `Vec` plus a `HashSet` for membership.
pub struct ThreadParticipationTracker {
    platform: String,
    max_tracked: usize,
    order: Vec<String>,
    index: HashSet<String>,
}

impl ThreadParticipationTracker {
    /// Python class attribute `_MAX_TRACKED = 500` (unused by the code, which
    /// takes the default from the `max_tracked=500` argument, but kept for
    /// completeness).
    pub const MAX_TRACKED: usize = 500;

    /// Python's `ThreadParticipationTracker(platform_name)`: `max_tracked=500`.
    pub fn new(platform_name: &str) -> Self {
        Self::with_max(platform_name, 500)
    }

    pub fn with_max(platform_name: &str, max_tracked: usize) -> Self {
        let mut tracker = Self {
            platform: platform_name.to_string(),
            max_tracked,
            order: Vec::new(),
            index: HashSet::new(),
        };
        // Python: `{str(thread_id): None for thread_id in self._load()}` — a
        // dict comprehension, so duplicates collapse to the first position.
        let loaded = tracker.load();
        for id in loaded {
            if tracker.index.insert(id.clone()) {
                tracker.order.push(id);
            }
        }
        tracker
    }

    fn state_path(&self) -> PathBuf {
        crate::config_file::hermes_home().join(format!("{}_threads.json", self.platform))
    }

    fn load(&self) -> Vec<String> {
        let path = self.state_path();
        if path.exists() {
            if let Ok(raw) = std::fs::read_to_string(&path) {
                if let Ok(Value::Array(items)) = serde_json::from_str::<Value>(&raw) {
                    // Python: `[str(thread_id) for thread_id in data]`.
                    return items.iter().map(py_str).collect();
                }
            }
        }
        Vec::new()
    }

    fn save(&mut self) {
        let path = self.state_path();
        if self.order.len() > self.max_tracked {
            let drop_count = self.order.len() - self.max_tracked;
            self.order.drain(..drop_count);
            self.index = self.order.iter().cloned().collect();
        }
        // Python calls `atomic_json_write(path, thread_list, indent=None)`,
        // which is `json.dump` with the default separators (", " / ": ") and
        // ensure_ascii=True, written to a temp file then renamed.
        let body = py_json_dumps_str_list(&self.order);
        let _ = atomic_text_write(&path, &body);
    }

    /// Mark `thread_id` as participated and persist.
    pub fn mark(&mut self, thread_id: &str) {
        if !self.index.contains(thread_id) {
            self.index.insert(thread_id.to_string());
            self.order.push(thread_id.to_string());
            self.save();
        }
    }

    /// Python's `__contains__`.
    pub fn contains(&self, thread_id: &str) -> bool {
        self.index.contains(thread_id)
    }

    pub fn clear(&mut self) {
        self.order.clear();
        self.index.clear();
    }

    /// Tracked ids in insertion order. Not in the Python API; used by tests.
    pub fn ids(&self) -> &[String] {
        &self.order
    }
}

/// `json.dumps(list_of_str)` with Python's defaults: `", "` item separator and
/// `ensure_ascii=True` (non-ASCII escaped as `\uXXXX`, surrogate pairs for
/// astral code points).
fn py_json_dumps_str_list(items: &[String]) -> String {
    let mut out = String::from("[");
    for (i, item) in items.iter().enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        out.push_str(&py_json_quote(item));
    }
    out.push(']');
    out
}

fn py_json_quote(s: &str) -> String {
    let mut out = String::from("\"");
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\x08' => out.push_str("\\b"),
            '\x0c' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c if (c as u32) < 0x7f => out.push(c),
            c => {
                let mut buf = [0u16; 2];
                for unit in c.encode_utf16(&mut buf) {
                    out.push_str(&format!("\\u{:04x}", unit));
                }
            }
        }
    }
    out.push('"');
    out
}

/// Temp file + rename, mirroring `utils.atomic_json_write`'s durability
/// contract (parents created, target never half-written).
fn atomic_text_write(path: &Path, body: &str) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    let tmp = path.with_file_name(format!(".{}_{}.tmp", stem, std::process::id()));
    std::fs::write(&tmp, body)?;
    std::fs::rename(&tmp, path)
}

// ─── Phone Number Redaction ──────────────────────────────────────────────────

/// Redact a phone number for logging, preserving country code and last 4.
///
/// Replaces the identical `_redact_phone()` functions in signal.py, sms.py and
/// bluebubbles.py. All slicing is by code point.
pub fn redact_phone(phone: &str) -> String {
    if phone.is_empty() {
        return "<none>".to_string();
    }
    let n = cp_len(phone);
    if n <= 8 {
        // Python: `return phone[:2] + "****" + phone[-2:] if len(phone) > 4 else "****"`
        // — the conditional wraps the whole expression.
        return if n > 4 {
            format!("{}****{}", cp_prefix(phone, 2), cp_last(phone, 2))
        } else {
            "****".to_string()
        };
    }
    format!("{}****{}", cp_prefix(phone, 4), cp_last(phone, 4))
}

// ─── GFM Markdown Table -> Bullet Conversion ─────────────────────────────────
// Shared by the Discord and Telegram adapters. Discord calls
// convert_table_to_bullets() directly; Telegram imports the primitives but
// keeps its own MarkdownV2-aware renderer.

/// A GFM table delimiter row: optional outer pipes, cells of dashes (with
/// optional alignment colons) separated by `|`. At least one internal `|` is
/// required, so a lone `---` rule is NOT matched.
pub fn table_separator_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"^\s*\|?\s*:?-+:?\s*(?:\|\s*:?-+:?\s*){1,}\|?\s*$").expect("static regex")
    })
}

/// Python's `TABLE_SEPARATOR_RE.match(line)`. The pattern is anchored at both
/// ends, so a `match` (prefix) and a search are equivalent here.
pub fn is_table_separator(line: &str) -> bool {
    table_separator_re().is_match(line).unwrap_or(false)
}

/// True when `line` could plausibly be a table data row.
pub fn is_table_row(line: &str) -> bool {
    let stripped = line.trim();
    !stripped.is_empty() && stripped.contains('|')
}

/// Split a GFM table row into stripped cell values.
///
/// Python delegates to `agent.markdown_tables.split_table_row`; that body is
/// inlined here (it does not touch `wcwidth`).
pub fn split_markdown_table_row(line: &str) -> Vec<String> {
    let mut s = line.trim();
    if let Some(rest) = s.strip_prefix('|') {
        s = rest;
    }
    if let Some(rest) = s.strip_suffix('|') {
        s = rest;
    }
    s.split('|').map(|c| c.trim().to_string()).collect()
}

/// Render a detected GFM table as bold-heading + bullet groups.
///
/// Uses the same alignment logic as Telegram's renderer: for non-row-label
/// tables `data_cells = cells` (the full row) and the bullet whose value
/// duplicates the heading is skipped, which keeps header -> value alignment
/// correct.
pub fn render_table_block<S: AsRef<str>>(table_block: &[S]) -> String {
    let join_all = || {
        table_block
            .iter()
            .map(|l| l.as_ref())
            .collect::<Vec<_>>()
            .join("\n")
    };

    if table_block.len() < 3 {
        return join_all();
    }

    let headers = split_markdown_table_row(table_block[0].as_ref());
    if headers.len() < 2 {
        return join_all();
    }

    // Python guards this with `if len(table_block) > 2`, which is already
    // implied by the `< 3` early return above.
    let first_data_row = split_markdown_table_row(table_block[2].as_ref());
    let has_row_label_col = first_data_row.len() == headers.len() + 1;

    let mut rendered_groups: Vec<String> = Vec::new();
    for (offset, row) in table_block[2..].iter().enumerate() {
        let index = offset + 1; // Python's `enumerate(..., start=1)`.
        let cells = split_markdown_table_row(row.as_ref());
        let (heading, mut data_cells) = if has_row_label_col {
            let heading = match cells.first() {
                Some(c) if !c.is_empty() => c.clone(),
                _ => format!("Row {}", index),
            };
            let rest = if cells.is_empty() {
                Vec::new()
            } else {
                cells[1..].to_vec()
            };
            (heading, rest)
        } else {
            let heading = cells
                .iter()
                .find(|c| !c.is_empty())
                .cloned()
                .unwrap_or_else(|| format!("Row {}", index));
            (heading, cells)
        };

        if data_cells.len() < headers.len() {
            data_cells.resize(headers.len(), String::new());
        } else if data_cells.len() > headers.len() {
            data_cells.truncate(headers.len());
        }

        let mut bullets: Vec<String> = Vec::new();
        for (header, value) in headers.iter().zip(data_cells.iter()) {
            if !has_row_label_col && *value == heading {
                continue;
            }
            bullets.push(format!("\u{2022} {}: {}", header, value));
        }

        let mut group_lines = vec![format!("**{}**", heading)];
        group_lines.extend(bullets);
        rendered_groups.push(group_lines.join("\n"));
    }

    rendered_groups.join("\n\n")
}

/// Rewrite GFM pipe tables into bold-heading + bullet groups. Tables inside
/// fenced code blocks are left alone.
pub fn convert_table_to_bullets(text: &str) -> String {
    if !text.contains('|') || !text.contains('-') {
        return text.to_string();
    }

    let lines: Vec<&str> = text.split('\n').collect();
    let mut out: Vec<String> = Vec::new();
    let mut in_fence = false;
    let mut i = 0usize;
    while i < lines.len() {
        let line = lines[i];
        let stripped = line.trim_start();

        if stripped.starts_with("```") {
            in_fence = !in_fence;
            out.push(line.to_string());
            i += 1;
            continue;
        }
        if in_fence {
            out.push(line.to_string());
            i += 1;
            continue;
        }

        if line.contains('|') && i + 1 < lines.len() && is_table_separator(lines[i + 1]) {
            let mut table_block: Vec<&str> = vec![line, lines[i + 1]];
            let mut j = i + 2;
            while j < lines.len() && is_table_row(lines[j]) {
                table_block.push(lines[j]);
                j += 1;
            }
            out.push(render_table_block(&table_block));
            i = j;
            continue;
        }

        out.push(line.to_string());
        i += 1;
    }

    out.join("\n")
}

// ─── Mention-pattern compilation ─────────────────────────────────────────────

/// Python's `str(obj)` for the JSON-ish values that reach this module.
///
/// Only the scalar cases are exercised by the callers (`str(pattern)` on config
/// entries). Containers fall back to a JSON rendering, which diverges from
/// Python's `repr`-with-single-quotes formatting; no caller relies on that.
fn py_str(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => "None".to_string(),
        Value::Bool(true) => "True".to_string(),
        Value::Bool(false) => "False".to_string(),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                i.to_string()
            } else if let Some(u) = n.as_u64() {
                u.to_string()
            } else if let Some(f) = n.as_f64() {
                // Python renders an integral float as "3.0", not "3".
                if f.fract() == 0.0 && f.is_finite() {
                    format!("{:.1}", f)
                } else {
                    f.to_string()
                }
            } else {
                n.to_string()
            }
        }
        other => other.to_string(),
    }
}

/// Python's `type(x).__name__` for the warning message.
fn py_type_name(v: &Value) -> &'static str {
    match v {
        Value::Null => "NoneType",
        Value::Bool(_) => "bool",
        Value::Number(n) => {
            if n.is_f64() && n.as_i64().is_none() && n.as_u64().is_none() {
                "float"
            } else {
                "int"
            }
        }
        Value::String(_) => "str",
        Value::Array(_) => "list",
        Value::Object(_) => "dict",
    }
}

/// Compile regex wake-word/mention patterns from config or env values.
///
/// Two adapter families share this logic:
///
/// * **Config-style** (dingtalk, telegram): pass `platform_label`. `raw` is the
///   value from `config.extra` after env fallback parsing; it must be a list or
///   a string, anything else logs a warning and yields `[]`. Non-string entries
///   are skipped. A summary info log is emitted when patterns load.
/// * **Wakeword-style** (photon, bluebubbles): pass `defaults`. `raw` may be
///   None (use defaults), a string (JSON list or comma/newline separated), a
///   list, or a scalar (wrapped in a list). Entries are coerced via `str()`.
///
/// `log_prefix` is interpolated into every log message so per-adapter log output
/// stays identical to the historical inline implementations.
///
/// Python's `logger_` override is dropped: this crate logs through the global
/// `tracing` subscriber, which has no per-call logger object.
pub fn compile_mention_patterns(
    raw: Option<&Value>,
    log_prefix: &str,
    platform_label: Option<&str>,
    display_label: Option<&str>,
    defaults: Option<&[String]>,
) -> Vec<Regex> {
    // `None` and JSON `null` both stand for Python's `None`.
    let raw = match raw {
        Some(Value::Null) | None => None,
        Some(v) => Some(v),
    };

    if let Some(platform_label) = platform_label {
        // Config-style (dingtalk/telegram) semantics.
        let display_name = display_label.unwrap_or(platform_label);
        let patterns: Vec<Value> = match raw {
            None => return Vec::new(),
            Some(Value::String(s)) => vec![Value::String(s.clone())],
            Some(Value::Array(a)) => a.clone(),
            Some(other) => {
                warn!(
                    "[{}] {} mention_patterns must be a list or string; got {}",
                    log_prefix,
                    platform_label,
                    py_type_name(other)
                );
                return Vec::new();
            }
        };

        let mut compiled: Vec<Regex> = Vec::new();
        for pattern in &patterns {
            let s = match pattern {
                Value::String(s) => s,
                // Python: `if not isinstance(pattern, str) ...: continue`.
                _ => continue,
            };
            if s.trim().is_empty() {
                continue;
            }
            match Regex::new(&format!("(?i){}", s)) {
                Ok(re) => compiled.push(re),
                Err(exc) => warn!(
                    "[{}] Invalid {} mention pattern {:?}: {}",
                    log_prefix, display_name, s, exc
                ),
            }
        }
        if !compiled.is_empty() {
            info!(
                "[{}] Loaded {} {} mention pattern(s)",
                log_prefix,
                compiled.len(),
                display_name
            );
        }
        return compiled;
    }

    // Wakeword-style (photon/bluebubbles) semantics.
    let patterns: Vec<Value> = match raw {
        None => defaults
            .unwrap_or(&[])
            .iter()
            .map(|s| Value::String(s.clone()))
            .collect(),
        Some(Value::String(s)) => {
            let text = s.trim();
            // Python: `loaded = json.loads(text) if text else []`, with any
            // exception mapping to `loaded = None`.
            let loaded: Option<Value> = if text.is_empty() {
                Some(Value::Array(Vec::new()))
            } else {
                serde_json::from_str::<Value>(text).ok()
            };
            match loaded {
                Some(Value::Array(a)) => a,
                _ => text
                    .lines()
                    .flat_map(|line| line.split(','))
                    .map(|part| Value::String(part.trim().to_string()))
                    .collect(),
            }
        }
        Some(Value::Array(a)) => a.clone(),
        Some(other) => vec![other.clone()],
    };

    let mut compiled: Vec<Regex> = Vec::new();
    for pattern in &patterns {
        let text = py_str(pattern);
        let text = text.trim();
        if text.is_empty() {
            continue;
        }
        match Regex::new(&format!("(?i){}", text)) {
            Ok(re) => compiled.push(re),
            Err(exc) => warn!(
                "[{}] Invalid mention pattern {:?}: {}",
                log_prefix, text, exc
            ),
        }
    }
    compiled
}

// ─── Fence-Aware Markdown Chunking ───────────────────────────────────────────
// Shared core for the fence-aware markdown chunkers that previously lived as
// near-duplicates in gateway/stream_consumer.py, gateway/platforms/yuanbao.py
// (MarkdownProcessor, the richest version, which this core is derived from) and
// gateway/platforms/weixin.py. Each caller keeps its own knobs:
//
//   * stream_consumer: newline-preferred splitting + close/reopen fence
//     balancing (prefer_paragraphs=false, balance_fences=true)
//   * yuanbao: atomic-block extraction + paragraph-boundary splitting, fences
//     kept intact as atoms (prefer_paragraphs=true, balance_fences=false)
//   * weixin: keeps its own block splitter but reuses greedy_pack_blocks.

/// True when `text` ends inside an unclosed ``` code fence.
///
/// Scans line by line, toggling in/out state on lines starting with ```. An odd
/// number of toggles means the trailing fence is unclosed.
pub fn text_has_unclosed_fence(text: &str) -> bool {
    let mut in_fence = false;
    for line in text.split('\n') {
        if line.starts_with("```") {
            in_fence = !in_fence;
        }
    }
    in_fence
}

/// True when the last non-empty line starts and ends with `|`.
pub fn text_ends_with_table_row(text: &str) -> bool {
    let trimmed = text.trim_end();
    if trimmed.is_empty() {
        return false;
    }
    let last_line = trimmed.split('\n').next_back().unwrap_or("").trim();
    last_line.starts_with('|') && last_line.ends_with('|')
}

/// True when an atomic block is a code block (starts with ```).
pub fn is_fence_atom(text: &str) -> bool {
    text.trim_start().starts_with("```")
}

/// True when an atomic block is a table (first line is `|...|`).
pub fn is_table_atom(text: &str) -> bool {
    let first_line = text.split('\n').next().unwrap_or("").trim();
    first_line.starts_with('|') && first_line.ends_with('|')
}

lazy_re!(re_sentence_end_newline, r"[\u{3002}\u{ff01}\u{ff1f}.!?]\n");

/// Find the nearest paragraph boundary within `max_chars`; return `(head, tail)`.
///
/// Split priority:
///   1. Blank line (paragraph boundary)
///   2. Newline after sentence-ending punctuation (CJK and ASCII)
///   3. Last newline
///   4. Force split at the `max_chars` window boundary
///
/// `head + tail == text` always holds. `len_fn` allows measuring in custom units
/// (e.g. UTF-16 code units); a binary search finds the largest prefix that fits
/// when it is provided.
///
/// Python's `if _len is len:` fast path is `len_fn.is_none()` here. The binary
/// search branch produces the same window for the builtin `len`, so this is a
/// performance distinction only.
pub fn split_at_paragraph_boundary(
    text: &str,
    max_chars: usize,
    len_fn: Option<&LenFn>,
) -> (String, String) {
    if measure(len_fn, text) <= max_chars {
        return (text.to_string(), String::new());
    }

    let window: &str = if len_fn.is_none() {
        cp_prefix(text, max_chars)
    } else {
        let (mut lo, mut hi) = (0usize, cp_len(text));
        while lo < hi {
            let mid = (lo + hi).div_ceil(2);
            if measure(len_fn, cp_prefix(text, mid)) <= max_chars {
                lo = mid;
            } else {
                hi = mid - 1;
            }
        }
        cp_prefix(text, lo)
    };

    // 1. Prefer the last blank line (\n\n) as a paragraph boundary. Python's
    // `pos > 0` treats both "not found" (-1) and index 0 as no split.
    if let Some(pos) = cp_rfind(window, "\n\n") {
        if pos > 0 {
            return (
                cp_prefix(text, pos + 2).to_string(),
                cp_suffix_from(text, pos + 2).to_string(),
            );
        }
    }

    // 2. Then the last newline following sentence-ending punctuation. Python
    // iterates every match and keeps the last `m.end()`.
    let mut best_pos: i64 = -1;
    if let Ok(iter) = re_sentence_end_newline()
        .find_iter(window)
        .collect::<Result<Vec<_>, _>>()
    {
        if let Some(last) = iter.last() {
            best_pos = window[..last.end()].chars().count() as i64;
        }
    }
    if best_pos > 0 {
        let b = best_pos as usize;
        return (
            cp_prefix(text, b).to_string(),
            cp_suffix_from(text, b).to_string(),
        );
    }

    // 3. Fallback: the last newline.
    if let Some(pos) = cp_rfind(window, "\n") {
        if pos > 0 {
            return (
                cp_prefix(text, pos + 1).to_string(),
                cp_suffix_from(text, pos + 1).to_string(),
            );
        }
    }

    // 4. No valid split point: force split at the window boundary.
    let cut = cp_len(window);
    (
        cp_prefix(text, cut).to_string(),
        cp_suffix_from(text, cut).to_string(),
    )
}

/// Split markdown into indivisible "atomic blocks".
///
/// Atoms are fenced code blocks (``` ... ``` inclusive), tables (consecutive
/// `|...|` lines) and plain paragraphs separated by blank lines. Blank lines are
/// separators and belong to no atom.
pub fn split_markdown_atoms(text: &str) -> Vec<String> {
    fn is_table_line(line: &str) -> bool {
        let stripped = line.trim();
        stripped.starts_with('|') && stripped.ends_with('|')
    }

    let lines: Vec<&str> = text.split('\n').collect();
    let mut atoms: Vec<String> = Vec::new();
    let mut current_lines: Vec<&str> = Vec::new();
    let mut in_fence = false;

    // Python's inner `_flush_current` closure.
    macro_rules! flush_current {
        () => {
            if !current_lines.is_empty() {
                let atom = current_lines.join("\n");
                if !atom.trim().is_empty() {
                    atoms.push(atom);
                }
                current_lines.clear();
            }
        };
    }

    for line in lines {
        if in_fence {
            current_lines.push(line);
            if line.starts_with("```") && current_lines.len() > 1 {
                in_fence = false;
                flush_current!();
            }
        } else if line.starts_with("```") {
            flush_current!();
            in_fence = true;
            current_lines.push(line);
        } else if is_table_line(line) {
            if !current_lines.is_empty() && !is_table_line(current_lines[current_lines.len() - 1]) {
                flush_current!();
            }
            current_lines.push(line);
        } else if line.trim().is_empty() {
            flush_current!();
        } else {
            if !current_lines.is_empty() && is_table_line(current_lines[current_lines.len() - 1]) {
                flush_current!();
            }
            current_lines.push(line);
        }
    }

    flush_current!();

    atoms
}

/// Infer the separator (`"\n"` or `"\n\n"`) between two chunks.
///
/// Single newline when the boundary sits at a code fence or a continued table;
/// paragraph separator otherwise.
pub fn infer_block_separator(prev_chunk: &str, next_chunk: &str) -> &'static str {
    let prev_trimmed = prev_chunk.trim_end();
    let next_trimmed = next_chunk.trim_start();

    if prev_trimmed.ends_with("```") || next_trimmed.starts_with("```") {
        return "\n";
    }

    if text_ends_with_table_row(prev_chunk) {
        let first_line = if next_trimmed.is_empty() {
            ""
        } else {
            next_trimmed.split('\n').next().unwrap_or("").trim()
        };
        if first_line.starts_with('|') && first_line.ends_with('|') {
            return "\n";
        }
    }

    "\n\n"
}

/// Stream-aware fence merge: rejoin chunks truncated mid-fence.
///
/// While chunk `i` has an unclosed fence and a successor exists, merge the
/// successor into it using [`infer_block_separator`].
pub fn merge_streaming_fences<S: AsRef<str>>(chunks: &[S]) -> Vec<String> {
    if chunks.is_empty() {
        return Vec::new();
    }

    let mut result: Vec<String> = Vec::new();
    let mut i = 0usize;
    while i < chunks.len() {
        let mut current = chunks[i].as_ref().to_string();
        while text_has_unclosed_fence(&current) && i + 1 < chunks.len() {
            let next = chunks[i + 1].as_ref();
            let sep = infer_block_separator(&current, next);
            current = format!("{}{}{}", current, sep, next);
            i += 1;
        }
        result.push(current);
        i += 1;
    }

    result
}

/// Close orphaned ``` fences at each chunk boundary and reopen on the next.
///
/// When a split lands inside a triple-backtick code block, close the fence at
/// the end of the head chunk and reopen it (with the original language tag) at
/// the start of the next, so every delivered chunk is fence-balanced on its own.
///
/// `carry_lang` is an `Option<String>` on purpose: Python distinguishes `None`
/// (not in code) from `""` (in code, no language tag), and `""` still emits a
/// bare ```` ``` ```` reopen prefix.
pub fn balance_fences_across_chunks<S: AsRef<str>>(chunks: &[S]) -> Vec<String> {
    if chunks.len() <= 1 {
        return chunks.iter().map(|c| c.as_ref().to_string()).collect();
    }
    let mut out: Vec<String> = Vec::new();
    let mut carry_lang: Option<String> = None;
    for chunk in chunks {
        let chunk = chunk.as_ref();
        let prefix = match &carry_lang {
            Some(l) => format!("```{}\n", l),
            None => String::new(),
        };
        let mut in_code = carry_lang.is_some();
        let mut lang = carry_lang.clone().unwrap_or_default();
        for line in chunk.split('\n') {
            let stripped = line.trim();
            if let Some(after_fence) = stripped.strip_prefix("```") {
                if in_code {
                    in_code = false;
                    lang = String::new();
                } else {
                    in_code = true;
                    let tag = after_fence.trim();
                    lang = if tag.is_empty() {
                        String::new()
                    } else {
                        tag.split_whitespace().next().unwrap_or("").to_string()
                    };
                }
            }
        }
        let mut body = format!("{}{}", prefix, chunk);
        if in_code {
            body.push_str("\n```");
            carry_lang = Some(lang);
        } else {
            carry_lang = None;
        }
        out.push(body);
    }
    out
}

/// Greedily pack pre-split `blocks` into chunks of at most `max_length`.
///
/// Blocks are joined with `sep` while they fit. A block that alone exceeds the
/// limit is passed to `overflow(block)` (which must return a list of chunks)
/// when provided, else emitted as-is.
///
/// Python defaults: `len_fn=None`, `sep="\n\n"`, `overflow=None`.
pub fn greedy_pack_blocks<S: AsRef<str>>(
    blocks: &[S],
    max_length: usize,
    len_fn: Option<&LenFn>,
    sep: &str,
    overflow: Option<&OverflowFn>,
) -> Vec<String> {
    let mut packed: Vec<String> = Vec::new();
    let mut current = String::new();
    for block in blocks {
        let block = block.as_ref();
        // Python: `block if not current else f"{current}{sep}{block}"` — the
        // empty string is falsy.
        let candidate = if current.is_empty() {
            block.to_string()
        } else {
            format!("{}{}{}", current, sep, block)
        };
        if measure(len_fn, &candidate) <= max_length {
            current = candidate;
            continue;
        }
        if !current.is_empty() {
            packed.push(std::mem::take(&mut current));
        }
        if measure(len_fn, block) <= max_length {
            current = block.to_string();
            continue;
        }
        match overflow {
            Some(f) => packed.extend(f(block)),
            None => packed.push(block.to_string()),
        }
    }
    if !current.is_empty() {
        packed.push(current);
    }
    packed
}

/// Split markdown text into chunks of at most `limit`, respecting fences.
///
/// Two strategies, selected by `prefer_paragraphs`:
///
/// `prefer_paragraphs = true` (yuanbao-derived, the richest): extract atomic
/// blocks (code fences, tables, paragraphs), greedily merge them up to `limit`,
/// split still-oversized non-atomic chunks at paragraph boundaries, then
/// re-merge small neighbours. Code blocks and tables are never split in the
/// middle; a single atom larger than `limit` is emitted oversize rather than
/// broken.
///
/// `prefer_paragraphs = false` (stream_consumer-derived): newline-preferred hard
/// splitting with headroom reserved for fence markers when the text contains
/// ```` ``` ````.
///
/// `balance_fences = true` post-processes the chunks so a split inside a code
/// block closes the fence on the head chunk and reopens it (with the language
/// tag) on the tail.
///
/// Python defaults: `len_fn=None`, `prefer_paragraphs=True`,
/// `balance_fences=False`.
pub fn split_text_fence_aware(
    text: &str,
    limit: usize,
    len_fn: Option<&LenFn>,
    prefer_paragraphs: bool,
    balance_fences: bool,
) -> Vec<String> {
    if text.is_empty() {
        return Vec::new();
    }

    let chunks = if prefer_paragraphs {
        chunk_markdown_paragraphs(text, limit, len_fn)
    } else {
        chunk_newline_preferred(text, limit, len_fn)
    };

    if balance_fences {
        return balance_fences_across_chunks(&chunks);
    }
    chunks
}

/// Yuanbao-derived paragraph/atom chunking pipeline.
pub fn chunk_markdown_paragraphs(
    text: &str,
    max_chars: usize,
    len_fn: Option<&LenFn>,
) -> Vec<String> {
    if measure(len_fn, text) <= max_chars {
        return vec![text.to_string()];
    }

    // Phase 1: extract atomic blocks.
    let atoms = split_markdown_atoms(text);

    // Phase 2: greedy merge.
    let mut chunks: Vec<String> = Vec::new();
    let mut indivisible_set: HashSet<usize> = HashSet::new();
    let mut current_parts: Vec<String> = Vec::new();
    let mut current_len: usize = 0;

    for atom in &atoms {
        let atom_len = measure(len_fn, atom);
        let mut sep_len = if current_parts.is_empty() { 0 } else { 2 };
        let projected_len = current_len + sep_len + atom_len;

        if projected_len > max_chars && !current_parts.is_empty() {
            chunks.push(current_parts.join("\n\n"));
            current_parts.clear();
            current_len = 0;
            sep_len = 0;
        }

        if current_parts.is_empty()
            && atom_len > max_chars
            && (is_fence_atom(atom) || is_table_atom(atom))
        {
            indivisible_set.insert(chunks.len());
            chunks.push(atom.clone());
            continue;
        }

        current_parts.push(atom.clone());
        current_len += sep_len + atom_len;
    }
    if !current_parts.is_empty() {
        chunks.push(current_parts.join("\n\n"));
    }

    // Phase 3: split still-oversized chunks at paragraph boundaries.
    let mut result: Vec<String> = Vec::new();
    for (idx, chunk) in chunks.iter().enumerate() {
        if measure(len_fn, chunk) <= max_chars {
            result.push(chunk.clone());
            continue;
        }
        if indivisible_set.contains(&idx) {
            result.push(chunk.clone());
            continue;
        }
        if text_has_unclosed_fence(chunk) {
            result.push(chunk.clone());
            continue;
        }

        let mut remaining = chunk.clone();
        while measure(len_fn, &remaining) > max_chars {
            let (mut head, tail) = split_at_paragraph_boundary(&remaining, max_chars, len_fn);
            // Python rebinds `remaining` to the tail in the tuple unpack BEFORE
            // the `if not head` fallback, so the fallback slices the tail, not
            // the pre-split string. Keep that order.
            remaining = tail;
            if head.is_empty() {
                let forced_head = cp_prefix(&remaining, max_chars).to_string();
                let forced_tail = cp_suffix_from(&remaining, max_chars).to_string();
                head = forced_head;
                remaining = forced_tail;
            }
            if !head.is_empty() {
                result.push(head);
            }
        }
        if !remaining.is_empty() {
            result.push(remaining);
        }
    }

    // Phase 4: merge small trailing/leading chunks with neighbours.
    if result.len() > 1 {
        let mut merged: Vec<String> = vec![result[0].clone()];
        for chunk in &result[1..] {
            let prev = merged.last().unwrap();
            let combined = format!("{}\n\n{}", prev, chunk);
            if measure(len_fn, &combined) <= max_chars {
                *merged.last_mut().unwrap() = combined;
            } else {
                merged.push(chunk.clone());
            }
        }
        result = merged;
    }

    result.into_iter().filter(|c| !c.is_empty()).collect()
}

/// Stream-consumer-derived newline-preferred splitting (no balancing).
pub fn chunk_newline_preferred(text: &str, limit: usize, len_fn: Option<&LenFn>) -> Vec<String> {
    if measure(len_fn, text) <= limit {
        return vec![text.to_string()];
    }
    // Reserve headroom for the close/reopen fence markers a balancing pass may
    // add, so balanced chunks stay within the platform limit.
    let mut split_limit = limit;
    if text.contains("```") {
        // Python: `max(limit - 16, limit // 2, 1)`. `limit - 16` can go negative
        // in Python, where the max() then picks another arm; saturating_sub
        // clamps to 0, which loses to `limit // 2` or `1` the same way.
        split_limit = limit.saturating_sub(16).max(limit / 2).max(1);
    }

    let mut chunks: Vec<String> = Vec::new();
    let mut remaining = text.to_string();
    while measure(len_fn, &remaining) > split_limit {
        let cp_budget = custom_unit_to_cp(&remaining, split_limit, len_fn);
        // Python: `remaining.rfind("\n", 0, _cp_budget)` — search the first
        // cp_budget code points; -1 when absent.
        let region = cp_prefix(&remaining, cp_budget);
        let found = cp_rfind(region, "\n").map(|p| p as i64).unwrap_or(-1);
        let split_at = if found < (cp_budget / 2) as i64 {
            cp_budget
        } else {
            found as usize
        };
        chunks.push(cp_prefix(&remaining, split_at).to_string());
        let next = cp_suffix_from(&remaining, split_at)
            .trim_start_matches('\n')
            .to_string();
        remaining = next;
    }
    if !remaining.is_empty() {
        chunks.push(remaining);
    }
    chunks
}

// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // Golden vectors below were produced by running the Python module directly:
    //   cd <repo> && python3 -c "import sys; sys.path.insert(0,'.'); \
    //     from gateway.platforms.helpers import <fn>; print(repr(<fn>(...)))"
    // Every `assert_eq!` marked GOLDEN carries a literal from that run.

    fn v(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    // ── strip_markdown ───────────────────────────────────────────────────────

    #[test]
    fn golden_strip_markdown() {
        // GOLDEN 1
        assert_eq!(
            strip_markdown("**bold** and *it* and `code` and [x](http://y)\n### Head\n\n\n\nend"),
            "bold and it and code and x\nHead\n\nend"
        );
        // GOLDEN 2
        assert_eq!(
            strip_markdown("__strong__ and _em_ and ```py\nx=1\n```\n"),
            "strong and em and x=1"
        );
        // GOLDEN 3
        assert_eq!(strip_markdown("mixed **a *b* c** tail"), "mixed a b c tail");
        // GOLDEN 4 — multibyte + emoji survive untouched, heading stripped.
        assert_eq!(
            strip_markdown("**\u{4f60}\u{597d}** \u{1f389} *\u{4e16}\u{754c}*\n# \u{6807}\u{9898}"),
            "\u{4f60}\u{597d} \u{1f389} \u{4e16}\u{754c}\n\u{6807}\u{9898}"
        );
    }

    // ── redact_phone ─────────────────────────────────────────────────────────

    #[test]
    fn golden_redact_phone() {
        // GOLDEN 5
        assert_eq!(redact_phone(""), "<none>");
        assert_eq!(redact_phone("1234"), "****");
        assert_eq!(redact_phone("12345"), "12****45");
        assert_eq!(redact_phone("12345678"), "12****78");
        assert_eq!(redact_phone("+12345678901"), "+123****8901");
        assert_eq!(redact_phone("12345678901234"), "1234****1234");
    }

    #[test]
    fn redact_phone_is_code_point_indexed() {
        // 6 code points, so the <= 8 branch: first 2 + last 2 code points.
        let s = "\u{1f600}\u{1f601}\u{1f602}\u{1f603}\u{1f604}\u{1f605}";
        assert_eq!(cp_len(s), 6);
        assert_eq!(redact_phone(s), "\u{1f600}\u{1f601}****\u{1f604}\u{1f605}");
    }

    // ── tables ───────────────────────────────────────────────────────────────

    #[test]
    fn golden_table_to_bullets() {
        // GOLDEN 6
        let tbl = "intro\n\n| A | B |\n| --- | --- |\n| 1 | 2 |\n| 3 | 4 |\n\nafter";
        assert_eq!(
            convert_table_to_bullets(tbl),
            "intro\n\n**1**\n\u{2022} B: 2\n\n**3**\n\u{2022} B: 4\n\nafter"
        );
        // GOLDEN 7 — row-label column (data rows have one more cell than headers).
        let tbl2 = "| | X | Y |\n|---|---|---|\n| r1 | 1 | 2 |\n| r2 | 3 | 4 |";
        assert_eq!(
            convert_table_to_bullets(tbl2),
            "**r1**\n\u{2022} X: 1\n\u{2022} Y: 2\n\n**r2**\n\u{2022} X: 3\n\u{2022} Y: 4"
        );
        // GOLDEN 8 — a table inside a fence is left alone.
        assert_eq!(
            convert_table_to_bullets("```\n| A | B |\n| --- | --- |\n| 1 | 2 |\n```"),
            "```\n| A | B |\n| --- | --- |\n| 1 | 2 |\n```"
        );
    }

    #[test]
    fn golden_table_primitives() {
        // GOLDEN 9
        assert_eq!(
            split_markdown_table_row("|  a | b  |c |"),
            v(&["a", "b", "c"])
        );
        assert_eq!(split_markdown_table_row("a | b"), v(&["a", "b"]));
        assert!(is_table_row("  | a |  "));
        assert!(!is_table_row("   "));
        assert_eq!(
            render_table_block(&["| A | B |", "|---|---|", "| 1 | 2 |"]),
            "**1**\n\u{2022} B: 2"
        );
        // Fewer than 3 lines, or fewer than 2 headers: joined verbatim.
        assert_eq!(render_table_block(&["| A |", "|---|"]), "| A |\n|---|");
        assert_eq!(
            render_table_block(&["| A |", "|---|", "| 1 |"]),
            "| A |\n|---|\n| 1 |"
        );
    }

    #[test]
    fn golden_table_separator_regex() {
        // GOLDEN 10 — a lone `---` rule must NOT match.
        assert!(is_table_separator("| --- | --- |"));
        assert!(!is_table_separator("---"));
        assert!(is_table_separator("| :--- | ---: |"));
        assert!(is_table_separator("|---|---|---|"));
        assert!(is_table_separator("  -- | -- "));
        assert!(!is_table_separator("| a | b |"));
        assert!(is_table_separator(":-:|:-:"));
    }

    // ── fence / atom predicates ──────────────────────────────────────────────

    #[test]
    fn golden_fence_predicates() {
        // GOLDEN 11
        assert!(text_has_unclosed_fence("a\n```py\ncode"));
        assert!(!text_has_unclosed_fence("a\n```py\ncode\n```\n"));
        assert!(text_ends_with_table_row("x\n| a | b |\n\n"));
        assert!(is_fence_atom("  ```py"));
        assert!(is_table_atom("| a |\nmore"));
        assert!(!is_table_atom("no pipes"));
    }

    // ── split_markdown_atoms ─────────────────────────────────────────────────

    #[test]
    fn golden_split_markdown_atoms() {
        // GOLDEN 12 — a blank line INSIDE a fence does not break the atom.
        let text =
            "para one\nstill one\n\n| a | b |\n| c | d |\n\n```py\nx = 1\n\ny = 2\n```\n\nlast para";
        assert_eq!(
            split_markdown_atoms(text),
            v(&[
                "para one\nstill one",
                "| a | b |\n| c | d |",
                "```py\nx = 1\n\ny = 2\n```",
                "last para",
            ])
        );
    }

    // ── separators / merging / balancing ─────────────────────────────────────

    #[test]
    fn golden_infer_block_separator() {
        // GOLDEN 13
        assert_eq!(infer_block_separator("code\n```", "next"), "\n");
        assert_eq!(infer_block_separator("| a |", "| b |"), "\n");
        assert_eq!(infer_block_separator("hello", "world"), "\n\n");
    }

    #[test]
    fn golden_merge_streaming_fences() {
        // GOLDEN 14
        assert_eq!(
            merge_streaming_fences(&["a\n```py", "x=1", "```\nb", "c"]),
            v(&["a\n```py\n\nx=1\n```\nb", "c"])
        );
        assert_eq!(merge_streaming_fences::<String>(&[]), Vec::<String>::new());
    }

    #[test]
    fn golden_balance_fences_across_chunks() {
        // GOLDEN 15 — head chunk gets a closing fence, tail reopens with the tag.
        assert_eq!(
            balance_fences_across_chunks(&["intro\n```python\nx=1", "y=2\n```\ndone"]),
            v(&["intro\n```python\nx=1\n```", "```python\ny=2\n```\ndone"])
        );
        // A single chunk is returned untouched, even when unbalanced.
        assert_eq!(
            balance_fences_across_chunks(&["```py\nx"]),
            v(&["```py\nx"])
        );
    }

    // ── greedy_pack_blocks ───────────────────────────────────────────────────

    #[test]
    fn golden_greedy_pack_blocks() {
        // GOLDEN 16 — "aaa\n\nbbb" is exactly 8, the boundary case.
        assert_eq!(
            greedy_pack_blocks(&["aaa", "bbb", "ccc"], 8, None, "\n\n", None),
            v(&["aaa\n\nbbb", "ccc"])
        );
        // GOLDEN 17 — an oversize block with no overflow hook is emitted as-is.
        assert_eq!(
            greedy_pack_blocks(&["aaa", "bbbbbbbbbbbb", "cc"], 8, None, "\n\n", None),
            v(&["aaa", "bbbbbbbbbbbb", "cc"])
        );
        // GOLDEN 18 — with an overflow hook.
        let split6: &dyn Fn(&str) -> Vec<String> =
            &|b: &str| vec![b[..6].to_string(), b[6..].to_string()];
        assert_eq!(
            greedy_pack_blocks(
                &["aaa", "bbbbbbbbbbbb", "cc"],
                8,
                None,
                "\n\n",
                Some(split6)
            ),
            v(&["aaa", "bbbbbb", "bbbbbb", "cc"])
        );
        // GOLDEN 19 — custom separator.
        assert_eq!(
            greedy_pack_blocks(&["ab", "cd"], 4, None, "\n", None),
            v(&["ab", "cd"])
        );
    }

    #[test]
    fn golden_greedy_pack_blocks_with_len_fn() {
        // GOLDEN 20 — len_fn counts non-ASCII as width 2, so "你好" is 4 and
        // "你好\n\n世界" is 10 > 5: nothing merges.
        let width: &dyn Fn(&str) -> usize = &|s: &str| {
            s.chars()
                .map(|c| if (c as u32) > 127 { 2 } else { 1 })
                .sum()
        };
        assert_eq!(
            greedy_pack_blocks(
                &["\u{4f60}\u{597d}", "\u{4e16}\u{754c}", "ab"],
                5,
                Some(width),
                "\n\n",
                None
            ),
            v(&["\u{4f60}\u{597d}", "\u{4e16}\u{754c}", "ab"])
        );
    }

    // ── split_at_paragraph_boundary ──────────────────────────────────────────

    #[test]
    fn golden_split_at_paragraph_boundary() {
        // GOLDEN 21 — blank-line boundary wins, the "\n\n" stays on the head.
        assert_eq!(
            split_at_paragraph_boundary("aaa\n\nbbb\nccc ddd", 10, None),
            ("aaa\n\n".to_string(), "bbb\nccc ddd".to_string())
        );
        // Fits: tail is empty.
        assert_eq!(
            split_at_paragraph_boundary("short", 10, None),
            ("short".to_string(), String::new())
        );
        // GOLDEN 22 — sentence-ending punctuation + newline.
        assert_eq!(
            split_at_paragraph_boundary("hello. \nworld foo bar", 12, None),
            ("hello. \n".to_string(), "world foo bar".to_string())
        );
        // GOLDEN 23 — no newline at all: forced split at the window boundary.
        assert_eq!(
            split_at_paragraph_boundary("abcdefghijklmno", 6, None),
            ("abcdef".to_string(), "ghijklmno".to_string())
        );
        // GOLDEN 24 — CJK full stop counts as a sentence end.
        assert_eq!(
            split_at_paragraph_boundary(
                "\u{53e5}\u{5b50}\u{4e00}\u{3002}\n\u{53e5}\u{5b50}\u{4e8c}\u{3002}\n\u{53e5}\u{5b50}\u{4e09}\u{3002}",
                7,
                None
            ),
            (
                "\u{53e5}\u{5b50}\u{4e00}\u{3002}\n".to_string(),
                "\u{53e5}\u{5b50}\u{4e8c}\u{3002}\n\u{53e5}\u{5b50}\u{4e09}\u{3002}".to_string()
            )
        );
    }

    #[test]
    fn golden_split_at_paragraph_boundary_is_code_point_indexed() {
        // GOLDEN 25 — 7 code points but 17 bytes. Python cuts at code point 5,
        // keeping the emoji whole. A byte-offset port would split the emoji.
        let text = "\u{4f60}\u{597d}\u{4e16}\u{754c}\u{1f389}ab";
        assert_eq!(cp_len(text), 7);
        assert_eq!(text.len(), 18);
        assert_eq!(
            split_at_paragraph_boundary(text, 5, None),
            (
                "\u{4f60}\u{597d}\u{4e16}\u{754c}\u{1f389}".to_string(),
                "ab".to_string()
            )
        );
    }

    #[test]
    fn golden_split_at_paragraph_boundary_with_len_fn() {
        // GOLDEN 26 — the binary-search window branch (len_fn is Some).
        let width: &dyn Fn(&str) -> usize = &|s: &str| {
            s.chars()
                .map(|c| if (c as u32) > 127 { 2 } else { 1 })
                .sum()
        };
        assert_eq!(
            split_at_paragraph_boundary("aaaa\nbbbb\ncccc", 9, Some(width)),
            ("aaaa\n".to_string(), "bbbb\ncccc".to_string())
        );
    }

    // ── split_text_fence_aware ───────────────────────────────────────────────

    const FENCED: &str = "Here is intro text that is fairly long.\n\n```python\ndef f():\n    return 1\n```\n\nAnd a trailing paragraph with more words.";

    #[test]
    fn golden_split_text_fence_aware_paragraph_mode() {
        // GOLDEN 27 — the load-bearing case: the fenced block stays whole.
        assert_eq!(
            split_text_fence_aware(FENCED, 60, None, true, false),
            v(&[
                "Here is intro text that is fairly long.",
                "```python\ndef f():\n    return 1\n```",
                "And a trailing paragraph with more words.",
            ])
        );
        // GOLDEN 28 — tighter limit forces the trailing paragraph to split, and
        // the fence still survives intact.
        assert_eq!(
            split_text_fence_aware(FENCED, 40, None, true, false),
            v(&[
                "Here is intro text that is fairly long.",
                "```python\ndef f():\n    return 1\n```",
                "And a trailing paragraph with more words",
                ".",
            ])
        );
        assert_eq!(
            split_text_fence_aware("", 10, None, true, false),
            Vec::<String>::new()
        );
        assert_eq!(
            split_text_fence_aware("tiny", 10, None, true, false),
            v(&["tiny"])
        );
    }

    #[test]
    fn golden_split_text_fence_aware_newline_mode() {
        // GOLDEN 29 — newline-preferred keeps the trailing "\n" on the head.
        assert_eq!(
            split_text_fence_aware(FENCED, 60, None, false, false),
            v(&[
                "Here is intro text that is fairly long.\n",
                "```python\ndef f():\n    return 1\n```\n",
                "And a trailing paragraph with more words.",
            ])
        );
        // GOLDEN 30 — balancing is a no-op here since every chunk is balanced.
        assert_eq!(
            split_text_fence_aware(FENCED, 60, None, false, true),
            v(&[
                "Here is intro text that is fairly long.\n",
                "```python\ndef f():\n    return 1\n```\n",
                "And a trailing paragraph with more words.",
            ])
        );
        // GOLDEN 31 — no newline in the first window: forced cut at cp_budget,
        // then a newline at index 5 that loses the `< cp_budget // 2` test.
        let long = format!("{}\n{}", "a".repeat(30), "b".repeat(30));
        assert_eq!(
            split_text_fence_aware(&long, 25, None, false, false),
            vec![
                "a".repeat(25),
                format!("{}\n{}", "a".repeat(5), "b".repeat(19)),
                "b".repeat(11),
            ]
        );
    }

    #[test]
    fn golden_split_text_fence_aware_multibyte() {
        // GOLDEN 32 — emoji + CJK paragraphs. Both modes must agree with Python.
        let emoji = "\u{1f389}\u{1f389}\u{1f389} emoji para one\n\n\u{4f60}\u{597d}\u{4e16}\u{754c}\u{4f60}\u{597d}\u{4e16}\u{754c}\u{4f60}\u{597d}\u{4e16}\u{754c}\n\n\u{1f388} tail para here";
        assert_eq!(
            split_text_fence_aware(emoji, 20, None, true, false),
            v(&[
                "\u{1f389}\u{1f389}\u{1f389} emoji para one",
                "\u{4f60}\u{597d}\u{4e16}\u{754c}\u{4f60}\u{597d}\u{4e16}\u{754c}\u{4f60}\u{597d}\u{4e16}\u{754c}",
                "\u{1f388} tail para here",
            ])
        );
        // GOLDEN 33
        assert_eq!(
            split_text_fence_aware(emoji, 20, None, false, false),
            v(&[
                "\u{1f389}\u{1f389}\u{1f389} emoji para one\n",
                "\u{4f60}\u{597d}\u{4e16}\u{754c}\u{4f60}\u{597d}\u{4e16}\u{754c}\u{4f60}\u{597d}\u{4e16}\u{754c}\n",
                "\u{1f388} tail para here",
            ])
        );
    }

    #[test]
    fn golden_chunk_helpers_direct() {
        // GOLDEN 34
        assert_eq!(
            chunk_markdown_paragraphs("p1 aaa\n\np2 bbb\n\np3 ccc", 10, None),
            v(&["p1 aaa", "p2 bbb", "p3 ccc"])
        );
        // GOLDEN 35
        assert_eq!(
            chunk_newline_preferred("line1\nline2\nline3\nline4", 12, None),
            v(&["line1\nline2", "line3\nline4"])
        );
    }

    #[test]
    fn golden_chunk_newline_preferred_utf16_len_fn() {
        // GOLDEN 36 — len_fn measures UTF-16 code units (a surrogate pair is 2),
        // so custom_unit_to_cp's binary search decides the code-point budget.
        // Python: _chunk_newline_preferred("🎉🎉🎉🎉🎉\nabcdefgh\nxyz", 8, u16len)
        //   => ['🎉🎉🎉🎉', '🎉\nabcde', 'fgh\nxyz']
        let u16len: &dyn Fn(&str) -> usize = &|s: &str| s.chars().map(|c| c.len_utf16()).sum();
        let text = "\u{1f389}\u{1f389}\u{1f389}\u{1f389}\u{1f389}\nabcdefgh\nxyz";
        assert_eq!(
            chunk_newline_preferred(text, 8, Some(u16len)),
            v(&[
                "\u{1f389}\u{1f389}\u{1f389}\u{1f389}",
                "\u{1f389}\nabcde",
                "fgh\nxyz",
            ])
        );
    }

    // ── compile_mention_patterns ─────────────────────────────────────────────

    fn pats(regexes: &[Regex]) -> Vec<String> {
        regexes
            .iter()
            .map(|r| r.as_str().trim_start_matches("(?i)").to_string())
            .collect()
    }

    #[test]
    fn golden_compile_mention_patterns() {
        // GOLDEN 37 — wakeword style, None falls back to defaults.
        let defaults = v(&["hey", "yo"]);
        assert_eq!(
            pats(&compile_mention_patterns(
                None,
                "X",
                None,
                None,
                Some(&defaults)
            )),
            v(&["hey", "yo"])
        );
        // GOLDEN 38 — a non-JSON string splits on newlines then commas.
        assert_eq!(
            pats(&compile_mention_patterns(
                Some(&Value::String("hey, yo\nsup".into())),
                "X",
                None,
                None,
                None
            )),
            v(&["hey", "yo", "sup"])
        );
        // GOLDEN 39 — a JSON list string parses as a list.
        assert_eq!(
            pats(&compile_mention_patterns(
                Some(&Value::String("[\"a\",\"b\"]".into())),
                "X",
                None,
                None,
                None
            )),
            v(&["a", "b"])
        );
        // GOLDEN 40 — an empty string yields no patterns.
        assert_eq!(
            pats(&compile_mention_patterns(
                Some(&Value::String(String::new())),
                "X",
                None,
                None,
                None
            )),
            Vec::<String>::new()
        );
        // GOLDEN 41 — a scalar is wrapped and str()-coerced.
        assert_eq!(
            pats(&compile_mention_patterns(
                Some(&Value::from(7)),
                "X",
                None,
                None,
                None
            )),
            v(&["7"])
        );
    }

    #[test]
    fn golden_compile_mention_patterns_config_style() {
        // GOLDEN 42 — config style wraps a bare string.
        assert_eq!(
            pats(&compile_mention_patterns(
                Some(&Value::String("bot".into())),
                "X",
                Some("dingtalk"),
                None,
                None
            )),
            v(&["bot"])
        );
        // GOLDEN 43 — a non-list, non-string value warns and yields nothing.
        assert_eq!(
            pats(&compile_mention_patterns(
                Some(&Value::from(42)),
                "X",
                Some("dingtalk"),
                None,
                None
            )),
            Vec::<String>::new()
        );
        // GOLDEN 44 — blanks and non-strings are skipped, invalid regexes warn.
        let raw = Value::Array(vec![
            Value::String("a".into()),
            Value::String(" ".into()),
            Value::from(5),
            Value::String("b[".into()),
        ]);
        assert_eq!(
            pats(&compile_mention_patterns(
                Some(&raw),
                "X",
                Some("d"),
                None,
                None
            )),
            v(&["a"])
        );
        // Null / absent raw returns empty in config mode.
        assert!(compile_mention_patterns(None, "X", Some("d"), None, None).is_empty());
        assert!(
            compile_mention_patterns(Some(&Value::Null), "X", Some("d"), None, None).is_empty()
        );
    }

    #[test]
    fn compiled_mention_patterns_are_case_insensitive() {
        let raw = Value::String("hermes".into());
        let compiled = compile_mention_patterns(Some(&raw), "X", None, None, None);
        assert_eq!(compiled.len(), 1);
        assert!(compiled[0].is_match("Hey HERMES!").unwrap());
    }

    // ── MessageDeduplicator ──────────────────────────────────────────────────

    #[test]
    fn dedup_basic_and_empty_id() {
        let mut d = MessageDeduplicator::default();
        assert!(!d.is_duplicate("a"));
        assert!(d.is_duplicate("a"));
        // The empty string is falsy in Python: never a duplicate, never stored.
        assert!(!d.is_duplicate(""));
        assert!(!d.is_duplicate(""));
        assert!(!d.contains(""));
        assert_eq!(d.len(), 1);
    }

    #[test]
    fn dedup_ttl_expiry() {
        // A zero TTL means `now - seen_at < 0` is always false: nothing is ever
        // a duplicate, and the expired entry is replaced.
        let mut d = MessageDeduplicator::new(10, 0.0);
        assert!(!d.is_duplicate("a"));
        assert!(!d.is_duplicate("a"));
        assert!(!d.contains("a"));
    }

    #[test]
    fn dedup_contains_does_not_claim() {
        let mut d = MessageDeduplicator::default();
        assert!(!d.contains("x"));
        assert_eq!(d.len(), 0);
        assert!(!d.is_duplicate("x"));
        assert!(d.contains("x"));
    }

    #[test]
    fn dedup_discard_and_clear() {
        let mut d = MessageDeduplicator::default();
        assert!(!d.is_duplicate("a"));
        d.discard("a");
        assert!(!d.is_duplicate("a"));
        d.clear();
        assert_eq!(d.len(), 0);
    }

    #[test]
    fn dedup_enforces_max_size_when_all_entries_are_fresh() {
        // Every entry stays inside the TTL, so the TTL prune keeps them all and
        // the newest-N fallback has to kick in.
        let mut d = MessageDeduplicator::new(4, 3600.0);
        for i in 0..20 {
            assert!(!d.is_duplicate(&format!("m{}", i)));
        }
        assert!(d.len() <= 4);
        // The newest ids survive.
        assert!(d.contains("m19"));
        assert!(!d.contains("m0"));
    }

    // ── ThreadParticipationTracker ───────────────────────────────────────────

    #[test]
    fn thread_tracker_persists_and_truncates() {
        // hermes_home() reads HERMES_HOME, which is process-global. Take the
        // crate-wide test lock ONCE (never nested).
        let _guard = crate::secret_scope::GLOBAL_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());

        let mut dir = std::env::temp_dir();
        dir.push(format!(
            "hermes_helpers_threads_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let prev = std::env::var("HERMES_HOME").ok();
        std::env::set_var("HERMES_HOME", &dir);

        {
            let mut t = ThreadParticipationTracker::with_max("discord", 3);
            assert!(!t.contains("t1"));
            t.mark("t1");
            t.mark("t2");
            t.mark("t1"); // idempotent
            assert!(t.contains("t1"));
            assert_eq!(t.ids().to_vec(), v(&["t1", "t2"]));
            t.mark("t3");
            t.mark("t4"); // pushes past max_tracked=3, oldest drops on save
            assert_eq!(t.ids().to_vec(), v(&["t2", "t3", "t4"]));
        }

        // Reload from disk.
        let path = dir.join("discord_threads.json");
        let raw = std::fs::read_to_string(&path).unwrap();
        // Python's json.dump(indent=None) separators are ", " / ": ".
        assert_eq!(raw, "[\"t2\", \"t3\", \"t4\"]");
        let t2 = ThreadParticipationTracker::with_max("discord", 3);
        assert!(t2.contains("t3"));
        assert!(!t2.contains("t1"));

        let mut t3 = ThreadParticipationTracker::with_max("discord", 3);
        t3.clear();
        assert!(!t3.contains("t3"));

        match prev {
            Some(v) => std::env::set_var("HERMES_HOME", v),
            None => std::env::remove_var("HERMES_HOME"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn json_dump_matches_python_ensure_ascii() {
        assert_eq!(py_json_dumps_str_list(&v(&[])), "[]");
        assert_eq!(py_json_dumps_str_list(&v(&["a"])), "[\"a\"]");
        // ensure_ascii=True escapes non-ASCII, astral chars as surrogate pairs.
        assert_eq!(
            py_json_dumps_str_list(&v(&["\u{4f60}", "\u{1f389}"])),
            "[\"\\u4f60\", \"\\ud83c\\udf89\"]"
        );
        assert_eq!(
            py_json_dumps_str_list(&v(&["a\"b\\c\nd"])),
            "[\"a\\\"b\\\\c\\nd\"]"
        );
    }

    // ── TextBatchAggregator ──────────────────────────────────────────────────

    fn text_event(text: &str) -> MessageEvent {
        MessageEvent {
            text: text.to_string(),
            ..MessageEvent::default()
        }
    }

    #[test]
    fn batch_aggregator_merges_and_dispatches() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();
        rt.block_on(async {
            let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
            let sink = Arc::clone(&seen);
            let handler: Arc<BatchHandler> = Arc::new(move |ev: MessageEvent| {
                let sink = Arc::clone(&sink);
                Box::pin(async move {
                    sink.lock().unwrap().push(ev.text);
                })
            });
            let agg = TextBatchAggregator::with_options(handler, 0.05, 0.2, 4000);
            assert!(agg.is_enabled());

            agg.enqueue(text_event("one"), "k");
            agg.enqueue(text_event("two"), "k");
            // Python joins pending chunks with a single newline.
            assert_eq!(agg.pending_text("k").as_deref(), Some("one\ntwo"));

            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
            assert_eq!(*seen.lock().unwrap(), v(&["one\ntwo"]));
            assert!(agg.pending_text("k").is_none());
        });
    }

    #[test]
    fn batch_aggregator_cancel_all_drops_pending() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();
        rt.block_on(async {
            let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
            let sink = Arc::clone(&seen);
            let handler: Arc<BatchHandler> = Arc::new(move |ev: MessageEvent| {
                let sink = Arc::clone(&sink);
                Box::pin(async move {
                    sink.lock().unwrap().push(ev.text);
                })
            });
            let agg = TextBatchAggregator::with_options(handler, 0.2, 0.4, 4000);
            agg.enqueue(text_event("hello"), "k");
            agg.cancel_all();
            tokio::time::sleep(std::time::Duration::from_millis(300)).await;
            assert!(seen.lock().unwrap().is_empty());
        });
    }

    #[test]
    fn batch_aggregator_disabled_when_delay_is_zero() {
        let handler: Arc<BatchHandler> = Arc::new(|_ev: MessageEvent| Box::pin(async {}));
        let agg = TextBatchAggregator::with_options(handler, 0.0, 2.0, 4000);
        assert!(!agg.is_enabled());
    }

    // ── code-point helper sanity ─────────────────────────────────────────────

    #[test]
    fn code_point_helpers_are_not_byte_indexed() {
        let s = "a\u{4f60}\u{1f389}b";
        assert_eq!(cp_len(s), 4);
        assert_eq!(s.len(), 9);
        assert_eq!(cp_prefix(s, 2), "a\u{4f60}");
        assert_eq!(cp_suffix_from(s, 2), "\u{1f389}b");
        assert_eq!(cp_last(s, 2), "\u{1f389}b");
        assert_eq!(cp_rfind("ab\ncd\nef", "\n"), Some(5));
        assert_eq!(cp_rfind("\u{4f60}\u{597d}\n\u{4e16}", "\n"), Some(2));
        assert_eq!(cp_rfind("abc", "\n"), None);
    }

    #[test]
    fn custom_unit_to_cp_binary_search() {
        let u16len: &dyn Fn(&str) -> usize = &|s: &str| s.chars().map(|c| c.len_utf16()).sum();
        let text = "\u{1f389}\u{1f389}\u{1f389}ab";
        // Budget 5 UTF-16 units. The prefixes are 🎉(2), 🎉🎉(4), 🎉🎉🎉(6):
        // 3 code points already costs 6 > 5, so the largest fitting prefix is 2
        // code points (4 units). Verified against Python _custom_unit_to_cp.
        assert_eq!(custom_unit_to_cp(text, 5, Some(u16len)), 2);
        // Everything fits.
        assert_eq!(custom_unit_to_cp(text, 100, Some(u16len)), 5);
        // Default len: the budget IS the code-point count.
        assert_eq!(custom_unit_to_cp(text, 2, None), 2);
    }
}
