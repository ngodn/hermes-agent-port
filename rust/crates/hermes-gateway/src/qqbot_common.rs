//! Port of gateway/platforms/qqbot/constants.py and utils.py.
//!
// Public API is ahead of its callers (the qqbot adapter/onboard slices land
// later); keep the whole surface exported even while unused.
#![allow(dead_code)]
//!
//! Two Python modules merged into one Rust module because utils.py imports
//! QQBOT_VERSION from constants.py, so they belong together.
//!
//! constants.py is a flat bag of package-level constants: the adapter version,
//! QQ API endpoints, timeout/retry knobs, message-length limits, and the
//! numeric message/media type codes the QQ Bot API uses on the wire.
//!
//! utils.py is three small helpers shared across the adapter:
//!  * `build_user_agent` / `get_api_headers` assemble the HTTP headers every
//!    QQ API call carries. q.qq.com serves a JavaScript anti-bot page unless
//!    `Accept: application/json` is present, so the header set is load-bearing.
//!  * `coerce_list` normalizes a loosely-typed config value (comma string,
//!    list, single scalar, or null) into a trimmed list of strings.
//!
//! Faithful-port notes on the User-Agent:
//!  * The Python original stitches in the live CPython version from
//!    `sys.version_info`. The Rust port has no Python interpreter, so the
//!    "Python/<x>" runtime segment is a caller-supplied string
//!    (`build_user_agent` takes it as an argument) rather than something this
//!    module can read. The wire format is kept identical.
//!  * `platform.system().lower()` maps to `os_name()`. The only value that
//!    needs remapping is macOS: Rust reports `"macos"` but Python reports the
//!    Darwin kernel name `"darwin"`, which is what QQ has seen historically.
//!  * The hermes version came from `importlib.metadata.version("hermes-agent")`
//!    with a `"dev"` fallback. The crate version (`CARGO_PKG_VERSION`) is the
//!    Rust analog; it is always present, so the fallback only guards an empty
//!    string.

use serde_json::Value;

// ---------------------------------------------------------------------------
// QQBot adapter version - bump on functional changes to the adapter package.
// ---------------------------------------------------------------------------

pub const QQBOT_VERSION: &str = "1.1.0";

// ---------------------------------------------------------------------------
// API endpoints
// ---------------------------------------------------------------------------

/// Default portal domain. Python reads `QQ_PORTAL_HOST` at import time and
/// falls back to `q.qq.com`; `portal_host()` reads the same env var.
pub const DEFAULT_PORTAL_HOST: &str = "q.qq.com";

/// The portal domain, configurable via `QQ_PORTAL_HOST` for corporate proxies
/// or test environments. Default: `q.qq.com` (production).
pub fn portal_host() -> String {
    match std::env::var("QQ_PORTAL_HOST") {
        Ok(v) => v,
        Err(_) => DEFAULT_PORTAL_HOST.to_string(),
    }
}

pub const API_BASE: &str = "https://api.sgroup.qq.com";
pub const TOKEN_URL: &str = "https://bots.qq.com/app/getAppAccessToken";
pub const GATEWAY_URL_PATH: &str = "/gateway";

// QR-code onboard endpoints (on the portal host)
pub const ONBOARD_CREATE_PATH: &str = "/lite/create_bind_task";
pub const ONBOARD_POLL_PATH: &str = "/lite/poll_bind_result";

/// The QR-code connect URL template. Carries a single `{task_id}` placeholder;
/// use `qr_url()` to fill it in.
pub const QR_URL_TEMPLATE: &str =
    "https://q.qq.com/qqbot/openclaw/connect.html?task_id={task_id}&_wv=2&source=hermes";

/// Fill the `{task_id}` placeholder in `QR_URL_TEMPLATE`.
pub fn qr_url(task_id: &str) -> String {
    format!("https://q.qq.com/qqbot/openclaw/connect.html?task_id={task_id}&_wv=2&source=hermes")
}

// ---------------------------------------------------------------------------
// Timeouts & retry
// ---------------------------------------------------------------------------

pub const DEFAULT_API_TIMEOUT: f64 = 30.0;
pub const FILE_UPLOAD_TIMEOUT: f64 = 120.0;
pub const CONNECT_TIMEOUT_SECONDS: f64 = 20.0;

/// Reconnect backoff schedule in seconds.
pub const RECONNECT_BACKOFF: [i64; 5] = [2, 5, 10, 30, 60];
pub const MAX_RECONNECT_ATTEMPTS: i64 = 100;
pub const RATE_LIMIT_DELAY: i64 = 60; // seconds
pub const QUICK_DISCONNECT_THRESHOLD: f64 = 5.0; // seconds
pub const MAX_QUICK_DISCONNECT_COUNT: i64 = 3;

pub const ONBOARD_POLL_INTERVAL: f64 = 2.0; // seconds between poll_bind_result calls
pub const ONBOARD_API_TIMEOUT: f64 = 10.0;

// ---------------------------------------------------------------------------
// Message limits
// ---------------------------------------------------------------------------

pub const MAX_MESSAGE_LENGTH: i64 = 4000;
pub const DEDUP_WINDOW_SECONDS: i64 = 300;
pub const DEDUP_MAX_SIZE: i64 = 1000;

// ---------------------------------------------------------------------------
// QQ Bot message types
// ---------------------------------------------------------------------------

pub const MSG_TYPE_TEXT: i64 = 0;
pub const MSG_TYPE_MARKDOWN: i64 = 2;
pub const MSG_TYPE_MEDIA: i64 = 7;
pub const MSG_TYPE_INPUT_NOTIFY: i64 = 6;

// ---------------------------------------------------------------------------
// QQ Bot file media types
// ---------------------------------------------------------------------------

pub const MEDIA_TYPE_IMAGE: i64 = 1;
pub const MEDIA_TYPE_VIDEO: i64 = 2;
pub const MEDIA_TYPE_VOICE: i64 = 3;
pub const MEDIA_TYPE_FILE: i64 = 4;

// ---------------------------------------------------------------------------
// User-Agent
// ---------------------------------------------------------------------------

/// The hermes-agent version, or `"dev"` if unavailable. The Python original
/// looked up the installed distribution; the Rust analog is the crate version,
/// which is always compiled in, so the fallback only guards an empty string.
fn hermes_version() -> String {
    let v = env!("CARGO_PKG_VERSION");
    if v.is_empty() {
        "dev".to_string()
    } else {
        v.to_string()
    }
}

/// The OS token, matching `platform.system().lower()`.
///
/// Python returns the kernel/OS name lowercased: `"linux"`, `"darwin"` (on
/// macOS), `"windows"`, `"freebsd"`, and so on. Rust's `env::consts::OS`
/// agrees on every value except macOS, where it says `"macos"` while Python
/// reports the Darwin kernel name, so that one is remapped.
fn os_name() -> String {
    match std::env::consts::OS {
        "macos" => "darwin".to_string(),
        other => other.to_string(),
    }
}

/// Build a descriptive User-Agent string.
///
/// Format: `QQBotAdapter/<qqbot_version> (Python/<py_version>; <os>; Hermes/<hermes_version>)`
///
/// Example: `QQBotAdapter/1.1.0 (Python/3.11.15; darwin; Hermes/0.9.0)`
///
/// `py_version` has no Rust equivalent (there is no Python interpreter), so the
/// caller supplies it. Pass a `major.minor.micro` string to match the shape the
/// Python code produced from `sys.version_info`.
pub fn build_user_agent(py_version: &str) -> String {
    let os = os_name();
    let hermes = hermes_version();
    format!("QQBotAdapter/{QQBOT_VERSION} (Python/{py_version}; {os}; Hermes/{hermes})")
}

/// Return the standard HTTP headers for QQBot API requests, in the same order
/// Python's dict yields them (`Content-Type`, `Accept`, `User-Agent`).
///
/// `q.qq.com` requires `Accept: application/json` - without it the server
/// returns a JavaScript anti-bot challenge page instead of JSON.
///
/// `py_version` is forwarded to `build_user_agent`; see its note.
pub fn get_api_headers(py_version: &str) -> [(&'static str, String); 3] {
    [
        ("Content-Type", "application/json".to_string()),
        ("Accept", "application/json".to_string()),
        ("User-Agent", build_user_agent(py_version)),
    ]
}

// ---------------------------------------------------------------------------
// Config helpers
// ---------------------------------------------------------------------------

/// Python `str(value)` for the value kinds `coerce_list` sees, before trimming.
///
/// The Python semantics that matter here:
///  * `None` -> `"None"` (only reached for a null *inside* a list; a top-level
///    null short-circuits to an empty list before this is called),
///  * `True`/`False` -> `"True"`/`"False"` (capitalized, unlike JSON),
///  * integers and floats render the same as serde_json's `Number` display
///    (`3`, `1.5`, `1.0`), matching CPython for ordinary config values,
///  * strings pass through unquoted.
///
/// Nested arrays/objects are an edge case config never really passes here;
/// Python would emit its `repr` (e.g. `['x', 'y']`). This falls back to the
/// serde_json rendering instead, which differs in quoting. Documented, not
/// matched, because it is not a real input.
fn python_str(value: &Value) -> String {
    match value {
        Value::Null => "None".to_string(),
        Value::Bool(true) => "True".to_string(),
        Value::Bool(false) => "False".to_string(),
        Value::Number(n) => n.to_string(),
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// Coerce a config value into a trimmed string list.
///
/// Accepts comma-separated strings, arrays (Python lists/tuples/sets all become
/// JSON arrays), single scalars, or null. Mirrors Python's `coerce_list`:
///  * null -> empty list,
///  * a string is split on `,`, each piece trimmed, empty pieces dropped,
///  * an array maps each element through `str()`, trims, drops empties,
///  * anything else becomes a one-element list of its trimmed `str()`, or the
///    empty list if that string is empty.
pub fn coerce_list(value: &Value) -> Vec<String> {
    match value {
        Value::Null => Vec::new(),
        Value::String(s) => s
            .split(',')
            .map(str::trim)
            .filter(|item| !item.is_empty())
            .map(str::to_string)
            .collect(),
        Value::Array(items) => items
            .iter()
            .map(|item| python_str(item).trim().to_string())
            .filter(|item| !item.is_empty())
            .collect(),
        other => {
            let s = python_str(other);
            let trimmed = s.trim();
            if trimmed.is_empty() {
                Vec::new()
            } else {
                vec![trimmed.to_string()]
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // Values locked against real Python:
    //   cd /home/eins0fx/development/hermes-agent-port
    //   python3 -c "<load constants.py + utils.py>; ..."
    // See the port session notes; the coerce_list cases below reproduce that
    // run exactly.

    #[test]
    fn constants_match_python() {
        assert_eq!(QQBOT_VERSION, "1.1.0");
        assert_eq!(DEFAULT_PORTAL_HOST, "q.qq.com");
        assert_eq!(API_BASE, "https://api.sgroup.qq.com");
        assert_eq!(TOKEN_URL, "https://bots.qq.com/app/getAppAccessToken");
        assert_eq!(GATEWAY_URL_PATH, "/gateway");
        assert_eq!(ONBOARD_CREATE_PATH, "/lite/create_bind_task");
        assert_eq!(ONBOARD_POLL_PATH, "/lite/poll_bind_result");
        assert_eq!(DEFAULT_API_TIMEOUT, 30.0);
        assert_eq!(FILE_UPLOAD_TIMEOUT, 120.0);
        assert_eq!(CONNECT_TIMEOUT_SECONDS, 20.0);
        assert_eq!(RECONNECT_BACKOFF, [2, 5, 10, 30, 60]);
        assert_eq!(MAX_RECONNECT_ATTEMPTS, 100);
        assert_eq!(RATE_LIMIT_DELAY, 60);
        assert_eq!(QUICK_DISCONNECT_THRESHOLD, 5.0);
        assert_eq!(MAX_QUICK_DISCONNECT_COUNT, 3);
        assert_eq!(ONBOARD_POLL_INTERVAL, 2.0);
        assert_eq!(ONBOARD_API_TIMEOUT, 10.0);
        assert_eq!(MAX_MESSAGE_LENGTH, 4000);
        assert_eq!(DEDUP_WINDOW_SECONDS, 300);
        assert_eq!(DEDUP_MAX_SIZE, 1000);
        assert_eq!(MSG_TYPE_TEXT, 0);
        assert_eq!(MSG_TYPE_MARKDOWN, 2);
        assert_eq!(MSG_TYPE_MEDIA, 7);
        assert_eq!(MSG_TYPE_INPUT_NOTIFY, 6);
        assert_eq!(MEDIA_TYPE_IMAGE, 1);
        assert_eq!(MEDIA_TYPE_VIDEO, 2);
        assert_eq!(MEDIA_TYPE_VOICE, 3);
        assert_eq!(MEDIA_TYPE_FILE, 4);
    }

    #[test]
    fn qr_url_fills_task_id() {
        assert_eq!(
            qr_url("abc123"),
            "https://q.qq.com/qqbot/openclaw/connect.html?task_id=abc123&_wv=2&source=hermes"
        );
        // The exported template still carries the placeholder.
        assert!(QR_URL_TEMPLATE.contains("{task_id}"));
    }

    #[test]
    fn portal_host_defaults_and_overrides() {
        // Note: env is process-global; scope the override tightly. Default path
        // only holds when the var is unset in this environment.
        std::env::remove_var("QQ_PORTAL_HOST");
        assert_eq!(portal_host(), "q.qq.com");
        std::env::set_var("QQ_PORTAL_HOST", "proxy.internal");
        assert_eq!(portal_host(), "proxy.internal");
        std::env::remove_var("QQ_PORTAL_HOST");
    }

    #[test]
    fn user_agent_shape() {
        // Locked against Python: build_user_agent() on Linux with py 3.14.7 and
        // an uninstalled distribution gave
        //   "QQBotAdapter/1.1.0 (Python/3.14.7; linux; Hermes/dev)"
        // The Rust hermes segment is the crate version rather than "dev", and
        // the OS is detected, so assert structure rather than an exact string.
        let ua = build_user_agent("3.14.7");
        assert!(ua.starts_with("QQBotAdapter/1.1.0 (Python/3.14.7; "));
        assert!(ua.ends_with(&format!("; Hermes/{})", hermes_version())));
        assert!(ua.contains(&format!("; {}; ", os_name())));

        #[cfg(target_os = "linux")]
        assert_eq!(os_name(), "linux"); // matches platform.system().lower()
    }

    #[test]
    fn api_headers_order_and_content() {
        let headers = get_api_headers("3.14.7");
        assert_eq!(headers[0], ("Content-Type", "application/json".to_string()));
        assert_eq!(headers[1], ("Accept", "application/json".to_string()));
        assert_eq!(headers[2].0, "User-Agent");
        assert_eq!(headers[2].1, build_user_agent("3.14.7"));
    }

    #[test]
    fn coerce_list_matches_python() {
        // Each case reproduces the locked Python run.
        assert_eq!(coerce_list(&Value::Null), Vec::<String>::new()); // None -> []
        assert_eq!(coerce_list(&json!("a, b ,,c,  ")), vec!["a", "b", "c"]); // csv
        assert_eq!(
            coerce_list(&json!(["x", " y ", "", 3])),
            vec!["x", "y", "3"]
        ); // list
        assert_eq!(coerce_list(&json!(["a", ""])), vec!["a"]); // tuple in Python
        assert_eq!(coerce_list(&json!(0)), vec!["0"]); // int 0 -> "0"
        assert_eq!(coerce_list(&json!("")), Vec::<String>::new()); // empty string
        assert_eq!(coerce_list(&json!("   ")), Vec::<String>::new()); // whitespace
        assert_eq!(coerce_list(&json!(false)), vec!["False"]); // bool -> "False"
        assert_eq!(coerce_list(&json!(true)), vec!["True"]); // bool -> "True"
        assert_eq!(coerce_list(&json!(123)), vec!["123"]);
        assert_eq!(coerce_list(&json!(1.5)), vec!["1.5"]);
    }

    #[test]
    fn coerce_list_scalar_edge_cases() {
        // null inside a list stringifies to "None" (unlike a top-level null).
        assert_eq!(coerce_list(&json!([null, "a"])), vec!["None", "a"]);
        // booleans inside a list keep Python's capitalized str().
        assert_eq!(coerce_list(&json!([true, false])), vec!["True", "False"]);
    }
}
