//! Port of gateway/platforms/webhook_filters.py.
//!
// Public API is ahead of its caller: the webhook adapter that drives these
// filters/transforms is not ported yet, so allow the surface until it lands.
#![allow(dead_code)]
//!
//! Route-local declarative filters and optional script transforms for the
//! webhook adapter.
//!
//! Faithfulness notes and modeling choices vs the Python original:
//!
//! * Python is duck-typed: payloads, specs, headers and resolved field values
//!   are all `Any`. Here everything is a `serde_json::Value`, and a resolved
//!   filter field is an `Option<Value>` where `None` stands in for the Python
//!   `_MISSING` sentinel (distinct from `Value::Null`, which is Python `None`).
//!
//! * `_stringify_filter_value` splits into two helpers because Python str() and
//!   json.dumps() disagree on scalars: [`stringify_filter_value`] renders a
//!   dict/list via [`python_json_dumps`] with `sort_keys=True` and a scalar via
//!   [`python_str`] (Python's `str()`: `True`/`False`/`None`, unquoted strings).
//!
//! * The profile-home resolution reuses `crate::config_file::hermes_home`
//!   (Python `hermes_constants.get_hermes_home`). The internal `*_in(home, ..)`
//!   variants take the home explicitly so the pure path logic is testable
//!   without mutating process env.
//!
//! * `run_route_script` shells out. `std::process::Command` has no built-in
//!   timeout, so it is spawned with piped stdin/stdout/stderr; a writer thread
//!   feeds the JSON payload to stdin, two reader threads drain stdout/stderr
//!   (this avoids the classic pipe-buffer-full deadlock that `communicate()`
//!   handles in Python), and the main thread polls `try_wait` against a
//!   deadline. On timeout the child is killed and reaped, matching Python's
//!   `subprocess.TimeoutExpired` -> `(False, None)`. Python does not pass
//!   `check=True`, so `CalledProcessError` is never actually raised; a non-zero
//!   exit falls through to the return-code branch, and everything else
//!   (missing interpreter etc, Python's bare `except Exception`) maps to the
//!   spawn/`Failed` path -> `(False, None)`.
//!
//! REPORTED couplings (not ported here, see the port task notes):
//!
//! * The child env: Python uses `tools.environments.local.build_subprocess_env`,
//!   which snapshots `os.environ` AND applies the Hermes secret-scrub +
//!   HERMES_HOME / subprocess-HOME propagation policy. That factory is not
//!   ported, so [`build_subprocess_env`] reproduces only its base (a copy of the
//!   current process environment).
//! * Output redaction: Python runs stdout/stderr through
//!   `agent.redact.redact_sensitive_text` before use (it feeds the transform,
//!   so it is functional, not just logging). `agent.redact` is not ported;
//!   [`redact_script_output`] is an identity pass-through for now, mirroring the
//!   deviation already recorded in `kanban_watchers.rs`.
//! * The interpreter for non-shell scripts: Python uses `sys.executable` (the
//!   Python running the gateway). The Rust gateway is not a Python process, so
//!   there is no direct equivalent; [`resolve_python_interpreter`] resolves one
//!   via `HERMES_PYTHON`, else `python3`/`python` on PATH.

use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use fancy_regex::Regex;
use serde_json::{Map, Value};
use tracing::{info, warn};

/// Default timeout for a route script, in seconds (Python
/// `DEFAULT_SCRIPT_TIMEOUT_SECONDS = 30`).
pub const DEFAULT_SCRIPT_TIMEOUT_SECONDS: i64 = 30;

// ---------------------------------------------------------------------------
// Value stringification (Python str() and json.dumps())
// ---------------------------------------------------------------------------

/// Python truthiness for a JSON value: `None`/`false`/`0`/`""`/`[]`/`{}` are
/// falsy, everything else truthy.
fn python_truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        // A number is falsy iff it equals zero (int 0 or float 0.0). NaN is
        // truthy in Python, and `as_f64` on NaN yields Some(NaN) != 0.0.
        Value::Number(n) => n.as_f64().map(|x| x != 0.0).unwrap_or(true),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

/// Python `str(value)` for a scalar, and a `str()`-equivalent (== `repr`) for a
/// container. Scalars: `None`, `True`/`False`, numbers as-is, strings unquoted.
fn python_str(v: &Value) -> String {
    match v {
        Value::Null => "None".to_string(),
        Value::Bool(true) => "True".to_string(),
        Value::Bool(false) => "False".to_string(),
        Value::Number(n) => n.to_string(),
        Value::String(s) => s.clone(),
        // str() of a list/dict equals its repr. Rare on this path (needles are
        // normally scalars); reproduced best-effort via python_repr.
        Value::Array(_) | Value::Object(_) => python_repr(v),
    }
}

/// Python `repr(value)`. Used only for the container arm of [`python_str`].
fn python_repr(v: &Value) -> String {
    match v {
        Value::Null => "None".to_string(),
        Value::Bool(true) => "True".to_string(),
        Value::Bool(false) => "False".to_string(),
        Value::Number(n) => n.to_string(),
        // repr of a str uses single quotes; good enough for the diagnostic path.
        Value::String(s) => format!("'{}'", s.replace('\\', "\\\\").replace('\'', "\\'")),
        Value::Array(a) => {
            let parts: Vec<String> = a.iter().map(python_repr).collect();
            format!("[{}]", parts.join(", "))
        }
        Value::Object(o) => {
            let parts: Vec<String> = o
                .iter()
                .map(|(k, val)| format!("'{}': {}", k, python_repr(val)))
                .collect();
            format!("{{{}}}", parts.join(", "))
        }
    }
}

/// Reproduce Python `json.dumps` with its defaults: `ensure_ascii=True`,
/// separators `(", ", ": ")`, and an optional `sort_keys`.
///
/// Number rendering defers to serde_json's `Display`, which matches Python for
/// ordinary ints and floats (`3` -> `3`, `3.0` -> `3.0`); exotic exponent forms
/// may differ, which does not matter for filter-field stringification.
fn python_json_dumps(v: &Value, sort_keys: bool) -> String {
    let mut out = String::new();
    dump_value(v, sort_keys, &mut out);
    out
}

fn dump_value(v: &Value, sort_keys: bool, out: &mut String) {
    match v {
        Value::Null => out.push_str("null"),
        Value::Bool(true) => out.push_str("true"),
        Value::Bool(false) => out.push_str("false"),
        Value::Number(n) => out.push_str(&n.to_string()),
        Value::String(s) => dump_string(s, out),
        Value::Array(a) => {
            out.push('[');
            for (i, item) in a.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                dump_value(item, sort_keys, out);
            }
            out.push(']');
        }
        Value::Object(o) => {
            out.push('{');
            let mut keys: Vec<&String> = o.keys().collect();
            if sort_keys {
                // Python sorts by code point; Rust str Ord (UTF-8 byte order)
                // agrees with code-point order for valid UTF-8.
                keys.sort();
            }
            for (i, k) in keys.into_iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                dump_string(k, out);
                out.push_str(": ");
                dump_value(&o[k.as_str()], sort_keys, out);
            }
            out.push('}');
        }
    }
}

/// JSON-escape a string the way `json.dumps(ensure_ascii=True)` does, including
/// the surrounding quotes. Non-ASCII (>= U+0080) is escaped to `\uXXXX` (with a
/// surrogate pair above U+FFFF); control chars below U+0020 use the short
/// escapes or `\u00XX`.
fn dump_string(s: &str, out: &mut String) {
    out.push('"');
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => {
                out.push_str(&format!("\\u{:04x}", c as u32));
            }
            c if (c as u32) < 0x7f => out.push(c),
            c => {
                // ensure_ascii: escape everything >= 0x7f. 0x7f (DEL) itself is
                // below 0x7f here, so it stays literal, matching json.dumps.
                let cp = c as u32;
                if cp <= 0xFFFF {
                    out.push_str(&format!("\\u{:04x}", cp));
                } else {
                    let v = cp - 0x10000;
                    let hi = 0xD800 + (v >> 10);
                    let lo = 0xDC00 + (v & 0x3FF);
                    out.push_str(&format!("\\u{:04x}\\u{:04x}", hi, lo));
                }
            }
        }
    }
    out.push('"');
}

/// Port of `_stringify_filter_value`. `None` (Python `_MISSING`) -> `""`;
/// dict/list -> `json.dumps(sort_keys=True)`; any other scalar -> Python
/// `str()`.
fn stringify_filter_value(value: Option<&Value>) -> String {
    match value {
        None => String::new(),
        Some(v @ (Value::Object(_) | Value::Array(_))) => python_json_dumps(v, true),
        Some(v) => python_str(v),
    }
}

/// Python `==` between two JSON values, covering the cross-type cases the
/// structural `serde_json` equality misses: `True == 1 == 1.0`, `3 == 3.0`.
fn python_eq(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Bool(x), Value::Bool(y)) => x == y,
        // bool is a subtype of int in Python: True == 1, False == 0.
        (Value::Bool(x), Value::Number(n)) | (Value::Number(n), Value::Bool(x)) => {
            n.as_f64() == Some(if *x { 1.0 } else { 0.0 })
        }
        (Value::Number(x), Value::Number(y)) => match (x.as_f64(), y.as_f64()) {
            (Some(xf), Some(yf)) => xf == yf,
            _ => x == y,
        },
        (Value::String(x), Value::String(y)) => x == y,
        (Value::Null, Value::Null) => true,
        (Value::Array(x), Value::Array(y)) => {
            x.len() == y.len() && x.iter().zip(y.iter()).all(|(p, q)| python_eq(p, q))
        }
        (Value::Object(x), Value::Object(y)) => {
            x.len() == y.len()
                && x.iter()
                    .all(|(k, xv)| y.get(k).map(|yv| python_eq(xv, yv)).unwrap_or(false))
        }
        _ => false,
    }
}

// ---------------------------------------------------------------------------
// Path resolution (profile home + script safety)
// ---------------------------------------------------------------------------

/// The user home for `~` expansion, `HOME` then `USERPROFILE` (matching
/// `config_file`'s home resolution).
fn user_home() -> Option<PathBuf> {
    for key in ["HOME", "USERPROFILE"] {
        if let Ok(val) = std::env::var(key) {
            let trimmed = val.trim();
            if !trimmed.is_empty() {
                return Some(PathBuf::from(trimmed));
            }
        }
    }
    None
}

/// Port of `os.path.expandvars` (POSIX form): replace `$name` and `${name}`
/// with the environment value; leave the literal text when the variable is
/// unset or the `${` is unterminated. `name` is ASCII `[A-Za-z0-9_]+`.
fn expandvars(s: &str) -> String {
    if !s.contains('$') {
        return s.to_string();
    }
    let bytes = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        if c != b'$' {
            // Push the whole UTF-8 char, not the byte.
            let ch_len = utf8_char_len(bytes[i]);
            out.push_str(&s[i..i + ch_len]);
            i += ch_len;
            continue;
        }
        // At a '$'.
        if i + 1 < bytes.len() && bytes[i + 1] == b'{' {
            // ${name}
            if let Some(close) = s[i + 2..].find('}') {
                let name = &s[i + 2..i + 2 + close];
                match std::env::var(name) {
                    Ok(val) => out.push_str(&val),
                    Err(_) => out.push_str(&s[i..i + 2 + close + 1]),
                }
                i = i + 2 + close + 1;
            } else {
                // Unterminated ${ : the regex alternative fails, '$' is literal.
                out.push('$');
                i += 1;
            }
            continue;
        }
        // $name
        let mut j = i + 1;
        while j < bytes.len() && is_var_char(bytes[j]) {
            j += 1;
        }
        if j > i + 1 {
            let name = &s[i + 1..j];
            match std::env::var(name) {
                Ok(val) => out.push_str(&val),
                Err(_) => out.push_str(&s[i..j]),
            }
            i = j;
        } else {
            // A lone '$' with no valid name.
            out.push('$');
            i += 1;
        }
    }
    out
}

fn is_var_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

fn utf8_char_len(first: u8) -> usize {
    if first < 0x80 {
        1
    } else if first >> 5 == 0b110 {
        2
    } else if first >> 4 == 0b1110 {
        3
    } else {
        4
    }
}

/// Port of `pathlib.Path(s).expanduser()` for the leading-`~` case: `~` and
/// `~/...` expand against the user home; anything else (including `~user`,
/// which we do not resolve) is returned unchanged, exactly as expanduser leaves
/// an unresolvable `~user`.
fn expanduser(s: &str) -> PathBuf {
    if s == "~" {
        return user_home().unwrap_or_else(|| PathBuf::from(s));
    }
    if let Some(rest) = s.strip_prefix("~/") {
        return match user_home() {
            Some(home) => home.join(rest),
            None => PathBuf::from(s),
        };
    }
    PathBuf::from(s)
}

/// Non-strict `Path.resolve()`: canonicalize when the path exists (resolving
/// symlinks, the security-relevant case), else absolutize against the cwd and
/// collapse `.`/`..` lexically. Mirrors `resolve(strict=False)`, which does not
/// require the path to exist.
fn resolve_pathish(p: &Path) -> PathBuf {
    if let Ok(c) = std::fs::canonicalize(p) {
        return c;
    }
    let abs = if p.is_absolute() {
        p.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|d| d.join(p))
            .unwrap_or_else(|_| p.to_path_buf())
    };
    normalize_lexical(&abs)
}

fn normalize_lexical(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for comp in p.components() {
        match comp {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Port of `_resolve_profile_path`, with the home passed explicitly.
fn resolve_profile_path_in(home: &Path, path_value: &Value) -> Option<PathBuf> {
    let s = path_value.as_str()?;
    let raw = expandvars(s.trim());
    if raw.is_empty() {
        return None;
    }
    if raw == "~/.hermes" {
        return Some(home.to_path_buf());
    }
    if let Some(rest) = raw.strip_prefix("~/.hermes/") {
        return Some(home.join(rest));
    }
    let path = expanduser(&raw);
    if path.is_absolute() {
        Some(path)
    } else {
        Some(home.join(path))
    }
}

/// Port of `_resolve_profile_path` using the active profile home.
pub fn resolve_profile_path(path_value: &Value) -> Option<PathBuf> {
    resolve_profile_path_in(&crate::config_file::hermes_home(), path_value)
}

/// Port of `_resolve_script_path`, with the home passed explicitly. Returns
/// `(Some(path), None)` on success or `(None, Some(reason))` on rejection.
fn resolve_script_path_in(home: &Path, script_value: &Value) -> (Option<PathBuf>, Option<String>) {
    let s = match script_value.as_str() {
        Some(s) if !s.trim().is_empty() => s,
        _ => return (None, Some("script path is empty".to_string())),
    };
    let scripts_root = resolve_pathish(&home.join("scripts"));
    let raw_text = expandvars(s.trim());

    let candidate = if raw_text == "~/.hermes" || raw_text.starts_with("~/.hermes/") {
        match resolve_profile_path_in(home, &Value::String(raw_text.clone())) {
            Some(mapped) => resolve_pathish(&mapped),
            None => scripts_root.clone(),
        }
    } else {
        let raw = expanduser(&raw_text);
        if raw.is_absolute() {
            resolve_pathish(&raw)
        } else {
            resolve_pathish(&scripts_root.join(&raw))
        }
    };

    // Python `candidate.relative_to(scripts_root)` raising ValueError == the
    // candidate not being under (or equal to) scripts_root.
    if !candidate.starts_with(&scripts_root) {
        return (
            None,
            Some(format!(
                "script path resolves outside {}",
                scripts_root.display()
            )),
        );
    }
    if !candidate.exists() {
        return (
            None,
            Some(format!("script not found: {}", candidate.display())),
        );
    }
    if !candidate.is_file() {
        return (
            None,
            Some(format!(
                "script path is not a file: {}",
                candidate.display()
            )),
        );
    }
    (Some(candidate), None)
}

/// Port of `_resolve_script_path` using the active profile home.
pub fn resolve_script_path(script_value: &Value) -> (Option<PathBuf>, Option<String>) {
    resolve_script_path_in(&crate::config_file::hermes_home(), script_value)
}

/// Port of `_load_filter_file_values`, with the home passed explicitly.
fn load_filter_file_values_in(home: &Path, path_value: &Value) -> Vec<Value> {
    let path = match resolve_profile_path_in(home, path_value) {
        Some(p) => p,
        None => return Vec::new(),
    };
    let raw = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(exc) => {
            warn!(
                "[webhook] filter in_file read failed for {}: {}",
                path.display(),
                exc
            );
            return Vec::new();
        }
    };
    match serde_json::from_str::<Value>(&raw) {
        Ok(Value::Array(items)) => items,
        // list(dict.keys()) -> the keys as strings.
        Ok(Value::Object(map)) => map.keys().map(|k| Value::String(k.clone())).collect(),
        Ok(other) => vec![other],
        // JSONDecodeError -> non-empty stripped lines.
        Err(_) => raw
            .lines()
            .map(|l| l.trim())
            .filter(|l| !l.is_empty())
            .map(|l| Value::String(l.to_string()))
            .collect(),
    }
}

/// Port of `_load_filter_file_values` using the active profile home.
pub fn load_filter_file_values(path_value: &Value) -> Vec<Value> {
    load_filter_file_values_in(&crate::config_file::hermes_home(), path_value)
}

// ---------------------------------------------------------------------------
// Filter evaluation + script transform
// ---------------------------------------------------------------------------

/// Port of `WebhookRouteProcessor`: evaluate declarative filters and optional
/// script transforms for a webhook route.
pub struct WebhookRouteProcessor {
    /// Effective script timeout in seconds (`max(1, int(...))`, as Python).
    pub script_timeout_seconds: i64,
}

impl Default for WebhookRouteProcessor {
    fn default() -> Self {
        Self::new(DEFAULT_SCRIPT_TIMEOUT_SECONDS)
    }
}

impl WebhookRouteProcessor {
    /// Python `__init__`: `script_timeout_seconds = max(1, int(...))`.
    pub fn new(script_timeout_seconds: i64) -> Self {
        Self {
            script_timeout_seconds: script_timeout_seconds.max(1),
        }
    }

    /// Port of `resolve_filter_field`: resolve a dotted field against the
    /// payload/event/headers context. Returns `None` for the Python `_MISSING`
    /// sentinel.
    pub fn resolve_filter_field(
        &self,
        field: &Value,
        payload: &Value,
        event_type: &str,
        headers: &Value,
    ) -> Option<Value> {
        let field_str = field.as_str()?;
        if field_str.trim().is_empty() {
            return None;
        }
        let parts: Vec<&str> = field_str
            .trim()
            .split('.')
            .filter(|p| !p.is_empty())
            .collect();
        if parts.is_empty() {
            return None;
        }

        // context["payload"] = payload.get("payload", payload)
        let ctx_payload = payload
            .as_object()
            .and_then(|o| o.get("payload"))
            .cloned()
            .unwrap_or_else(|| payload.clone());
        // headers -> dict(headers or {})
        let ctx_headers = match headers {
            Value::Object(_) => headers.clone(),
            _ => Value::Object(Map::new()),
        };

        let (mut value, rest): (Value, &[&str]) = match parts[0] {
            "payload" => (ctx_payload, &parts[1..]),
            "event" | "event_type" => (Value::String(event_type.to_string()), &parts[1..]),
            "headers" => (ctx_headers, &parts[1..]),
            // Not a context key: start from the whole payload and consume all
            // parts (Python's `else: value = payload`).
            _ => (payload.clone(), &parts[..]),
        };

        for part in rest {
            // Compute the next value into an owned temp so the borrow of `value`
            // is released before we reassign it.
            let next = match &value {
                Value::Object(map) => map.get(*part).cloned(),
                Value::Array(arr)
                    if !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit()) =>
                {
                    match part.parse::<usize>() {
                        Ok(idx) if idx < arr.len() => Some(arr[idx].clone()),
                        _ => None,
                    }
                }
                _ => return None,
            };
            match next {
                Some(v) => value = v,
                None => return None,
            }
        }
        Some(value)
    }

    /// Port of `filter_matches`: evaluate one declarative filter spec.
    pub fn filter_matches(
        &self,
        spec: &Value,
        payload: &Value,
        event_type: &str,
        headers: &Value,
    ) -> bool {
        let obj = match spec.as_object() {
            Some(o) => o,
            None => {
                warn!("[webhook] Ignoring invalid filter spec: {:?}", spec);
                return false;
            }
        };

        // all / any / not, in Python order.
        if obj.contains_key("all") {
            return match obj.get("all") {
                Some(Value::Array(items)) => items
                    .iter()
                    .all(|item| self.filter_matches(item, payload, event_type, headers)),
                _ => false,
            };
        }
        if obj.contains_key("any") {
            return match obj.get("any") {
                Some(Value::Array(items)) => items
                    .iter()
                    .any(|item| self.filter_matches(item, payload, event_type, headers)),
                _ => false,
            };
        }
        if obj.contains_key("not") {
            let inner = obj.get("not").unwrap_or(&Value::Null);
            return !self.filter_matches(inner, payload, event_type, headers);
        }

        let field = obj.get("field").cloned().unwrap_or(Value::Null);
        let value = self.resolve_filter_field(&field, payload, event_type, headers);

        if obj.contains_key("exists") {
            let exists = value.is_some();
            let want = python_truthy(obj.get("exists").unwrap());
            return exists == want;
        }
        // `spec.get("missing") is True` -> only the literal boolean true.
        if obj.get("missing") == Some(&Value::Bool(true)) {
            return value.is_none();
        }
        if obj.contains_key("equals") {
            let target = obj.get("equals").unwrap();
            return match &value {
                Some(v) => python_eq(v, target),
                None => false,
            };
        }
        if obj.contains_key("not_equals") {
            let target = obj.get("not_equals").unwrap();
            return match &value {
                Some(v) => !python_eq(v, target),
                None => true,
            };
        }
        if obj.contains_key("contains") {
            let needle = obj.get("contains").unwrap();
            let v = match &value {
                Some(v) => v,
                None => return false,
            };
            return match v {
                Value::Array(arr) => arr.iter().any(|el| python_eq(el, needle)),
                // `needle in dict` checks keys; JSON keys are strings.
                Value::Object(map) => needle
                    .as_str()
                    .map(|k| map.contains_key(k))
                    .unwrap_or(false),
                _ => stringify_filter_value(Some(v)).contains(python_str(needle).as_str()),
            };
        }
        if obj.contains_key("in") {
            let haystack = obj.get("in").unwrap();
            return match (haystack, &value) {
                (Value::Array(items), Some(v)) => items.iter().any(|el| python_eq(el, v)),
                _ => false,
            };
        }
        if obj.contains_key("in_file") {
            // Python always loads (the membership test evaluates the list), even
            // when the resolved value is missing.
            let values = load_filter_file_values_in(
                &crate::config_file::hermes_home(),
                obj.get("in_file").unwrap(),
            );
            return match &value {
                Some(v) => values.iter().any(|el| python_eq(el, v)),
                None => false,
            };
        }
        if obj.contains_key("regex") {
            let v = match &value {
                Some(v) => v,
                None => return false,
            };
            let pattern = python_str(obj.get("regex").unwrap());
            let haystack = stringify_filter_value(Some(v));
            return match Regex::new(&pattern) {
                Ok(re) => matches!(re.find(&haystack), Ok(Some(_))),
                Err(exc) => {
                    warn!("[webhook] Invalid webhook filter regex: {}", exc);
                    false
                }
            };
        }

        warn!(
            "[webhook] Filter spec has no supported operator: {:?}",
            spec
        );
        false
    }

    /// Port of `route_filters_match`.
    pub fn route_filters_match(
        &self,
        route_config: &Value,
        payload: &Value,
        event_type: &str,
        headers: &Value,
    ) -> bool {
        // filters = route_config.get("filters") or []
        let filters = route_config.as_object().and_then(|o| o.get("filters"));
        let filters = match filters {
            Some(f) if python_truthy(f) => f,
            // Falsy or absent -> [] -> `if not filters: return True`.
            _ => return true,
        };
        match filters {
            Value::Object(_) => self.filter_matches(filters, payload, event_type, headers),
            Value::Array(items) => items
                .iter()
                .all(|spec| self.filter_matches(spec, payload, event_type, headers)),
            _ => {
                warn!("[webhook] filters must be a list or object");
                false
            }
        }
    }

    /// Port of `run_route_script`: run a route script and return
    /// `(should_continue, transformed_payload)`.
    pub fn run_route_script(&self, script_value: &Value, payload: &Value) -> (bool, Option<Value>) {
        self.run_route_script_in(&crate::config_file::hermes_home(), script_value, payload)
    }

    fn run_route_script_in(
        &self,
        home: &Path,
        script_value: &Value,
        payload: &Value,
    ) -> (bool, Option<Value>) {
        let (path, error) = resolve_script_path_in(home, script_value);
        let path = match (path, error) {
            (Some(p), None) => p,
            (_, err) => {
                warn!(
                    "[webhook] script ignored webhook: {}",
                    err.as_deref().unwrap_or("script path is empty")
                );
                return (false, None);
            }
        };

        // Interpreter selection mirrors the `.sh`/`.bash` vs sys.executable split.
        let suffix = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_lowercase());
        let argv0: PathBuf = if matches!(suffix.as_deref(), Some("sh") | Some("bash")) {
            match resolve_bash() {
                Some(b) => b,
                None => {
                    warn!("[webhook] script ignored webhook: bash not found");
                    return (false, None);
                }
            }
        } else {
            match resolve_python_interpreter() {
                Some(p) => p,
                None => {
                    // Python would hit FileNotFoundError from the spawn; folded
                    // into the "script execution failed" path.
                    warn!("[webhook] script execution failed: no python interpreter");
                    return (false, None);
                }
            }
        };

        let mut cmd = Command::new(&argv0);
        cmd.arg(&path);
        if let Some(parent) = path.parent() {
            cmd.current_dir(parent);
        }
        // Explicit env (Python passes env=build_subprocess_env()); see the module
        // doc for the reported scrub/propagation deviation.
        cmd.env_clear();
        cmd.envs(build_subprocess_env());
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(exc) => {
                // Python's bare `except Exception` (FileNotFoundError etc).
                warn!("[webhook] script execution failed: {}", exc);
                return (false, None);
            }
        };

        // Feed the payload to stdin from a thread; drain stdout/stderr from
        // threads. This avoids a pipe-buffer-full deadlock, matching what
        // subprocess.communicate() does under the hood.
        let input = python_json_dumps(payload, false);
        let stdin = child.stdin.take();
        let writer = std::thread::spawn(move || {
            if let Some(mut s) = stdin {
                // Ignore write errors (e.g. broken pipe if the script never
                // reads stdin), exactly as communicate() tolerates EPIPE.
                let _ = s.write_all(input.as_bytes());
            }
        });
        let mut out = child.stdout.take().expect("stdout piped");
        let out_t = std::thread::spawn(move || {
            let mut b = Vec::new();
            let _ = out.read_to_end(&mut b);
            b
        });
        let mut err = child.stderr.take().expect("stderr piped");
        let err_t = std::thread::spawn(move || {
            let mut b = Vec::new();
            let _ = err.read_to_end(&mut b);
            b
        });

        let deadline = Instant::now() + Duration::from_secs(self.script_timeout_seconds as u64);
        let outcome = loop {
            match child.try_wait() {
                Ok(Some(status)) => break Wait::Exited(status),
                Ok(None) => {
                    if Instant::now() >= deadline {
                        break Wait::TimedOut;
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(_) => break Wait::Failed,
            }
        };

        let status = match outcome {
            Wait::TimedOut => {
                // Match CPython subprocess.run's timeout path: kill and reap the
                // direct child, then return without reading the pipes. We must
                // NOT join the reader threads here: a grandchild (e.g. a `sleep`
                // spawned by the script) inherits the stdout/stderr pipe and
                // keeps its write end open, so read_to_end would block until the
                // grandchild exits. Python leaks that fd too and returns
                // immediately; the detached reader threads drain and exit on
                // their own once the grandchild goes away.
                let _ = child.kill();
                let _ = child.wait();
                drop((writer, out_t, err_t));
                warn!("[webhook] script timed out: {}", path.display());
                return (false, None);
            }
            Wait::Failed => {
                let _ = child.kill();
                let _ = child.wait();
                drop((writer, out_t, err_t));
                warn!("[webhook] script execution failed: wait error");
                return (false, None);
            }
            Wait::Exited(status) => status,
        };

        let _ = writer.join();
        let out_bytes = out_t.join().unwrap_or_default();
        let err_bytes = err_t.join().unwrap_or_default();
        // text=True, encoding="utf-8", errors="replace" == lossy decode.
        let stdout = redact_script_output(String::from_utf8_lossy(&out_bytes).trim());
        let stderr = redact_script_output(String::from_utf8_lossy(&err_bytes).trim());

        if !status.success() {
            let code = status
                .code()
                .map(|c| c.to_string())
                .unwrap_or_else(|| "None".to_string());
            info!(
                "[webhook] script ignored webhook path={} code={} stderr={}",
                path.file_name().and_then(|n| n.to_str()).unwrap_or(""),
                code,
                truncate_chars(&stderr, 200),
            );
            return (false, None);
        }
        if stdout.is_empty() || stdout == "[SILENT]" {
            return (false, None);
        }

        let transformed: Value = match serde_json::from_str::<Value>(&stdout) {
            Ok(v) => v,
            Err(_) => {
                // {**payload, "script_output": stdout}
                let mut m = payload.as_object().cloned().unwrap_or_default();
                m.insert("script_output".to_string(), Value::String(stdout.clone()));
                Value::Object(m)
            }
        };
        if !transformed.is_object() {
            warn!("[webhook] script stdout must be a JSON object or text");
            return (false, None);
        }
        let obj = transformed.as_object().unwrap();
        if obj.get("[SILENT]") == Some(&Value::Bool(true))
            || obj.get("__hermes_ignore__") == Some(&Value::Bool(true))
        {
            return (false, None);
        }
        (true, Some(transformed))
    }
}

enum Wait {
    Exited(ExitStatus),
    TimedOut,
    Failed,
}

/// Truncate to at most `n` chars on a char boundary (Python `text[:200]`).
fn truncate_chars(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

/// `shutil.which("bash") or ("/bin/bash" if os.path.isfile("/bin/bash"))`.
fn resolve_bash() -> Option<PathBuf> {
    if let Some(b) = which("bash") {
        return Some(b);
    }
    let fallback = Path::new("/bin/bash");
    if fallback.is_file() {
        return Some(fallback.to_path_buf());
    }
    None
}

/// Interpreter for non-shell scripts. Python uses `sys.executable`; the Rust
/// gateway is not a Python process, so resolve one (HERMES_PYTHON, else
/// python3/python on PATH). REPORTED modeling choice.
fn resolve_python_interpreter() -> Option<PathBuf> {
    if let Ok(v) = std::env::var("HERMES_PYTHON") {
        let t = v.trim();
        if !t.is_empty() {
            let p = PathBuf::from(t);
            if p.is_file() {
                return Some(p);
            }
        }
    }
    which("python3").or_else(|| which("python"))
}

/// Minimal `shutil.which`: an explicit path is checked directly, otherwise
/// PATH is scanned for an executable file.
fn which(name: &str) -> Option<PathBuf> {
    if name.contains('/') {
        let p = PathBuf::from(name);
        return if is_executable_file(&p) {
            Some(p)
        } else {
            None
        };
    }
    let paths = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&paths) {
        let cand = dir.join(name);
        if is_executable_file(&cand) {
            return Some(cand);
        }
    }
    None
}

#[cfg(unix)]
fn is_executable_file(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    match std::fs::metadata(p) {
        Ok(m) => m.is_file() && (m.permissions().mode() & 0o111) != 0,
        Err(_) => false,
    }
}

#[cfg(not(unix))]
fn is_executable_file(p: &Path) -> bool {
    p.is_file()
}

/// REPORTED coupling: Python builds the child env via
/// `tools.environments.local.build_subprocess_env`, which snapshots os.environ
/// AND applies the Hermes secret-scrub + HERMES_HOME/subprocess-HOME
/// propagation policy. That factory is not ported yet, so we reproduce only its
/// base: a copy of the current process environment.
fn build_subprocess_env() -> Vec<(String, String)> {
    std::env::vars().collect()
}

/// REPORTED coupling: Python redacts script stdout/stderr via
/// `agent.redact.redact_sensitive_text` before using them (the stdout redaction
/// is functional, it feeds the transform). `agent.redact` is not ported yet, so
/// this is an identity pass-through, mirroring the deviation already recorded in
/// `kanban_watchers.rs`. Because identity cannot fail, Python's
/// "[REDACTED - redaction failed]" fallback branch is unreachable here.
fn redact_script_output(text: &str) -> String {
    text.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::path::PathBuf;

    // ---- pure stringification (golden from real Python) --------------------

    #[test]
    #[allow(clippy::approx_constant)] // 3.14 is a deliberate float-format sample, not PI
    fn stringify_matches_python() {
        // Goldens captured from _stringify_filter_value:
        assert_eq!(stringify_filter_value(None), "");
        assert_eq!(stringify_filter_value(Some(&json!("hello"))), "hello");
        assert_eq!(stringify_filter_value(Some(&json!(42))), "42");
        assert_eq!(stringify_filter_value(Some(&json!(3.0))), "3.0");
        assert_eq!(stringify_filter_value(Some(&json!(3.14))), "3.14");
        assert_eq!(stringify_filter_value(Some(&json!(true))), "True");
        assert_eq!(stringify_filter_value(Some(&json!(false))), "False");
        assert_eq!(stringify_filter_value(Some(&json!(null))), "None");
        // dict/list -> json.dumps(sort_keys=True)
        assert_eq!(
            stringify_filter_value(Some(&json!({"b":1,"a":2}))),
            r#"{"a": 2, "b": 1}"#
        );
        assert_eq!(stringify_filter_value(Some(&json!([1, 2, 3]))), "[1, 2, 3]");
        assert_eq!(
            stringify_filter_value(Some(&json!({"z":[3,2],"a":{"y":1,"x":2}}))),
            r#"{"a": {"x": 2, "y": 1}, "z": [3, 2]}"#
        );
        // ensure_ascii: Python json.dumps defaults to ensure_ascii=True, so
        // non-ASCII is escaped to \uXXXX.
        assert_eq!(
            stringify_filter_value(Some(&json!({"k":"ünïcode"}))),
            "{\"k\": \"\\u00fcn\\u00efcode\"}"
        );
        assert_eq!(
            stringify_filter_value(Some(&json!(["a", null, true]))),
            r#"["a", null, true]"#
        );
    }

    #[test]
    fn json_dumps_payload_no_sort_matches_python() {
        // Golden: json.dumps({"a":1,"b":[1,2],"c":None,"d":True})
        assert_eq!(
            python_json_dumps(&json!({"a":1,"b":[1,2],"c":null,"d":true}), false),
            r#"{"a": 1, "b": [1, 2], "c": null, "d": true}"#
        );
    }

    // ---- expandvars / expanduser (golden from posixpath) -------------------

    #[test]
    fn expandvars_matches_posix() {
        std::env::set_var("WF_TEST_FOO", "barval");
        assert_eq!(expandvars("$WF_TEST_FOO/x"), "barval/x");
        assert_eq!(expandvars("${WF_TEST_FOO}/y"), "barval/y");
        assert_eq!(expandvars("$WF_TEST_MISSING/z"), "$WF_TEST_MISSING/z");
        assert_eq!(expandvars("no vars"), "no vars");
        assert_eq!(expandvars("~/.hermes"), "~/.hermes");
        // Unterminated ${ stays literal.
        assert_eq!(expandvars("${unterminated"), "${unterminated");
        std::env::remove_var("WF_TEST_FOO");
    }

    // ---- profile path resolution (golden values) ---------------------------

    #[test]
    fn resolve_profile_path_golden() {
        let home = PathBuf::from("/tmp/hh");
        let rp = |s: &Value| resolve_profile_path_in(&home, s);
        assert_eq!(rp(&json!("~/.hermes")), Some(PathBuf::from("/tmp/hh")));
        assert_eq!(
            rp(&json!("~/.hermes/sub/x")),
            Some(PathBuf::from("/tmp/hh/sub/x"))
        );
        assert_eq!(rp(&json!("/abs/path")), Some(PathBuf::from("/abs/path")));
        assert_eq!(
            rp(&json!("rel/path")),
            Some(PathBuf::from("/tmp/hh/rel/path"))
        );
        // non-string -> None
        assert_eq!(rp(&json!(123)), None);
        // blank -> None
        assert_eq!(rp(&json!("   ")), None);
        // expandvars applied first
        std::env::set_var("WF_MYVAR", "scripts");
        assert_eq!(
            rp(&json!("$WF_MYVAR/a")),
            Some(PathBuf::from("/tmp/hh/scripts/a"))
        );
        std::env::remove_var("WF_MYVAR");
    }

    // ---- filter matching (golden from real Python) -------------------------

    fn sample() -> (Value, Value) {
        let payload = json!({
            "payload": {"user": {"name": "alice", "roles": ["admin", "dev"]},
                        "count": 3, "active": true, "tag": null},
            "extra": "top"
        });
        let headers = json!({"X-Event": "push", "Content-Type": "application/json"});
        (payload, headers)
    }

    #[test]
    fn resolve_filter_field_golden() {
        let p = WebhookRouteProcessor::default();
        let (payload, headers) = sample();
        let rf = |f: &str| p.resolve_filter_field(&json!(f), &payload, "push", &headers);
        assert_eq!(rf("payload.user.name"), Some(json!("alice")));
        assert_eq!(rf("payload.user.roles.0"), Some(json!("admin")));
        assert_eq!(rf("payload.user.roles.5"), None);
        assert_eq!(rf("event"), Some(json!("push")));
        assert_eq!(rf("event_type"), Some(json!("push")));
        assert_eq!(rf("headers.X-Event"), Some(json!("push")));
        // Not a context key -> falls back to the whole payload root.
        assert_eq!(rf("extra"), Some(json!("top")));
        assert_eq!(rf("nope.here"), None);
        assert_eq!(rf(""), None);
    }

    #[test]
    fn filter_matches_golden() {
        let p = WebhookRouteProcessor::default();
        let (payload, headers) = sample();
        let m = |spec: Value| p.filter_matches(&spec, &payload, "push", &headers);

        assert!(m(json!({"field":"payload.user.name","equals":"alice"})));
        assert!(!m(json!({"field":"payload.user.name","equals":"bob"})));
        assert!(m(json!({"field":"payload.count","equals":3})));
        // 3 == 3.0 in Python
        assert!(m(json!({"field":"payload.count","equals":3.0})));
        assert!(m(json!({"field":"payload.active","equals":true})));
        assert!(m(json!({"field":"payload.user.name","exists":true})));
        assert!(m(json!({"field":"payload.ghost","exists":false})));
        assert!(m(json!({"field":"payload.ghost","missing":true})));
        assert!(m(json!({"field":"payload.ghost","not_equals":"bob"})));
        assert!(m(json!({"field":"payload.user.roles","contains":"admin"})));
        assert!(m(json!({"field":"payload.user.name","contains":"lic"})));
        // dict contains checks keys
        assert!(m(json!({"field":"payload.user","contains":"name"})));
        assert!(m(json!({"field":"payload.count","in":[1,2,3]})));
        assert!(m(json!({"field":"payload.user.name","regex":"^al"})));
        // None stringifies to "None" for regex
        assert!(m(json!({"field":"payload.tag","regex":"None"})));
        assert!(m(json!({"all":[{"field":"event","equals":"push"},
                                 {"field":"payload.count","equals":3}]})));
        assert!(m(json!({"any":[{"field":"event","equals":"nope"},
                                 {"field":"payload.count","equals":3}]})));
        assert!(m(json!({"not":{"field":"event","equals":"nope"}})));
        // non-dict spec -> false
        assert!(!m(json!("notadict")));
    }

    #[test]
    fn route_filters_match_golden() {
        let p = WebhookRouteProcessor::default();
        let (payload, headers) = sample();
        let rfm = |cfg: Value| p.route_filters_match(&cfg, &payload, "push", &headers);
        assert!(rfm(json!({})));
        assert!(rfm(json!({"filters": []})));
        assert!(rfm(json!({"filters": {"field":"event","equals":"push"}})));
        assert!(rfm(json!({"filters": [{"field":"event","equals":"push"},
                                        {"field":"payload.count","equals":3}]})));
        assert!(!rfm(
            json!({"filters": [{"field":"event","equals":"nope"}]})
        ));
    }

    // ---- script path safety + subprocess transform -------------------------

    fn temp_home(tag: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "wf_test_{}_{}_{}",
            tag,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        std::fs::create_dir_all(p.join("scripts")).unwrap();
        p
    }

    fn write_script(home: &Path, name: &str, body: &str) -> PathBuf {
        let path = home.join("scripts").join(name);
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(body.as_bytes()).unwrap();
        path
    }

    #[test]
    fn resolve_script_path_safety() {
        let home = temp_home("scriptpath");
        write_script(&home, "t.sh", "#!/bin/bash\ncat\n");
        let scripts_root = std::fs::canonicalize(home.join("scripts")).unwrap();

        let (p, e) = resolve_script_path_in(&home, &json!("t.sh"));
        assert_eq!(p, Some(scripts_root.join("t.sh")));
        assert_eq!(e, None);

        let (p, e) = resolve_script_path_in(&home, &json!("../escape.sh"));
        assert_eq!(p, None);
        assert!(e.unwrap().starts_with("script path resolves outside"));

        let (p, e) = resolve_script_path_in(&home, &json!("nope.sh"));
        assert_eq!(p, None);
        assert!(e.unwrap().starts_with("script not found:"));

        let (p, e) = resolve_script_path_in(&home, &json!("  "));
        assert_eq!(p, None);
        assert_eq!(e.as_deref(), Some("script path is empty"));

        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn run_route_script_cat_roundtrips_payload() {
        let home = temp_home("cat");
        // cat echoes the JSON payload we feed to stdin back out.
        write_script(&home, "echo.sh", "#!/bin/bash\ncat\n");
        let p = WebhookRouteProcessor::default();
        let (cont, out) = p.run_route_script_in(&home, &json!("echo.sh"), &json!({"a":1,"b":2}));
        assert!(cont);
        assert_eq!(out, Some(json!({"a":1,"b":2})));
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn run_route_script_transform_and_variants() {
        let home = temp_home("variants");
        let p = WebhookRouteProcessor::default();

        write_script(
            &home,
            "tr.sh",
            "#!/bin/bash\necho '{\"transformed\": true, \"n\": 5}'\n",
        );
        assert_eq!(
            p.run_route_script_in(&home, &json!("tr.sh"), &json!({"a":1})),
            (true, Some(json!({"transformed": true, "n": 5})))
        );

        write_script(&home, "silent.sh", "#!/bin/bash\necho \"[SILENT]\"\n");
        assert_eq!(
            p.run_route_script_in(&home, &json!("silent.sh"), &json!({"a":1})),
            (false, None)
        );

        write_script(&home, "empty.sh", "#!/bin/bash\n\n");
        assert_eq!(
            p.run_route_script_in(&home, &json!("empty.sh"), &json!({"a":1})),
            (false, None)
        );

        write_script(&home, "fail.sh", "#!/bin/bash\necho oops >&2\nexit 3\n");
        assert_eq!(
            p.run_route_script_in(&home, &json!("fail.sh"), &json!({"a":1})),
            (false, None)
        );

        // Non-JSON stdout -> {**payload, script_output: text}.
        write_script(&home, "text.sh", "#!/bin/bash\necho hello-plain\n");
        assert_eq!(
            p.run_route_script_in(&home, &json!("text.sh"), &json!({"a":1})),
            (true, Some(json!({"a":1, "script_output": "hello-plain"})))
        );

        write_script(
            &home,
            "ig.sh",
            "#!/bin/bash\necho '{\"__hermes_ignore__\": true}'\n",
        );
        assert_eq!(
            p.run_route_script_in(&home, &json!("ig.sh"), &json!({"a":1})),
            (false, None)
        );

        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn run_route_script_times_out() {
        let home = temp_home("timeout");
        write_script(&home, "slow.sh", "#!/bin/bash\nsleep 5\necho done\n");
        let p = WebhookRouteProcessor::new(1);
        let start = Instant::now();
        let res = p.run_route_script_in(&home, &json!("slow.sh"), &json!({"a":1}));
        let elapsed = start.elapsed();
        assert_eq!(res, (false, None));
        // Killed at ~1s, well before the 5s sleep would finish.
        assert!(elapsed < Duration::from_secs(4), "elapsed {:?}", elapsed);
        let _ = std::fs::remove_dir_all(&home);
    }
}
