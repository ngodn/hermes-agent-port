//! Port of gateway/platforms/qqbot/onboard.py.
//!
// The onboarding entry point is ahead of its caller (the qqbot adapter and the
// setup CLI land later), so keep the whole surface exported while unused.
#![allow(dead_code)]
//!
//! QQBot scan-to-configure (QR code onboard).
//!
//! Mirrors the Feishu onboarding pattern: a single public entry point
//! [`qr_register`] that runs the whole flow (create bind task -> display QR code
//! -> poll -> decrypt credentials).
//!
//! Calls the `q.qq.com` `create_bind_task` / `poll_bind_result` APIs to generate
//! a QR-code URL and poll for scan completion. On success the caller receives the
//! bot's *app_id*, *client_secret* (decrypted locally), and the scanner's
//! *user_openid*, enough to fully configure the QQBot gateway.
//!
//! Reference: <https://bot.q.qq.com/wiki/develop/api-v2/>
//!
//! Faithful-port notes:
//!
//! * Python uses a synchronous `httpx.Client`. The gateway crate's HTTP stack is
//!   `reqwest`, which is async only (no `blocking` feature is enabled), so the
//!   flow is `async` here. The request shapes (URL, method, headers, JSON body,
//!   timeout, redirect following) are unchanged.
//! * Everything the flow touches outside pure logic (the two POSTs, sleeping,
//!   the monotonic clock, stdout, and QR rendering) goes through the
//!   [`OnboardIo`] trait, so the flow is testable without a network or a wall
//!   clock. [`ReqwestOnboardIo`] is the real implementation.
//! * `_render_qr` tried to import the optional `qrcode` Python package and
//!   rendered ASCII art when present. There is no equivalent crate in this
//!   workspace and the port adds no dependencies, so the default
//!   [`OnboardIo::render_qr`] returns `false`, which is exactly the Python
//!   behavior when `qrcode` is not installed: the URL-only branch, including the
//!   `pip install qrcode` tip. An implementation is free to override it.
//! * `get_api_headers` takes a Python-version string in the Rust port (there is
//!   no interpreter to read it from). It is threaded through as the
//!   `py_version` argument of the flow functions so the User-Agent stays
//!   byte-identical to whatever the caller wants on the wire.
//! * Python raises `RuntimeError` on a bad `retcode` and lets unexpected errors
//!   escape `qr_register`. Here those are [`OnboardError`] values: recoverable
//!   outcomes still return `Ok(None)`, and only the errors Python let propagate
//!   (a decrypt failure) come back as `Err`.

use async_trait::async_trait;
use serde_json::{json, Value};
use std::time::{Duration, Instant};

use crate::qqbot_common::{
    get_api_headers, portal_host, qr_url, ONBOARD_API_TIMEOUT, ONBOARD_CREATE_PATH,
    ONBOARD_POLL_INTERVAL, ONBOARD_POLL_PATH,
};
use crate::qqbot_crypto::{decrypt_secret, CryptoError};

// ---------------------------------------------------------------------------
// Bind status
// ---------------------------------------------------------------------------

/// Status codes returned by [`poll_bind_result`]. Port of the `BindStatus`
/// `IntEnum`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BindStatus {
    None,
    Pending,
    Completed,
    Expired,
}

impl BindStatus {
    pub fn as_i64(self) -> i64 {
        match self {
            BindStatus::None => 0,
            BindStatus::Pending => 1,
            BindStatus::Completed => 2,
            BindStatus::Expired => 3,
        }
    }

    /// Python's `BindStatus(value)`. An unknown value raises `ValueError`
    /// there; here it is an error the caller turns into a failed poll.
    ///
    /// Enum lookup in Python is a dict lookup on the value, so `2.0` and `True`
    /// find `COMPLETED` and `PENDING` the same way `2` and `1` do. That is why
    /// any JSON number (and any bool) whose numeric value lands on 0..=3 is
    /// accepted here.
    fn from_json(value: &Value) -> Result<Self, OnboardError> {
        let n = match value {
            Value::Number(n) => n.as_f64(),
            Value::Bool(true) => Some(1.0),
            Value::Bool(false) => Some(0.0),
            _ => None,
        };
        // Explicit float comparisons rather than `Some(0.0)` patterns: matching
        // on floating-point literals is discouraged, and the guard states the
        // intent (Python's enum lookup is a dict lookup, so 2.0 and True hash
        // equal to 2 and 1) more clearly than a pattern would.
        #[allow(clippy::redundant_guards)]
        match n {
            Some(x) if x == 0.0 => Ok(BindStatus::None),
            Some(x) if x == 1.0 => Ok(BindStatus::Pending),
            Some(x) if x == 2.0 => Ok(BindStatus::Completed),
            Some(x) if x == 3.0 => Ok(BindStatus::Expired),
            _ => Err(OnboardError::BadResponse(format!(
                "{} is not a valid BindStatus",
                py_str(value)
            ))),
        }
    }
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Everything that can go wrong in the onboarding flow.
///
/// The Python code raised `RuntimeError` for API-level failures, let `httpx`
/// raise its own for transport/status failures, and let `AttributeError` /
/// `ValueError` escape when a response was shaped wrong. The variants below keep
/// those apart because `qr_register` treats them differently (all of them are
/// swallowed inside the poll loop; only a decrypt failure escapes the function).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OnboardError {
    /// Transport failure, a non-2xx status (`raise_for_status`), or a body that
    /// did not parse as JSON.
    Http(String),
    /// A non-zero `retcode`, or a missing `task_id`. The string is the exact
    /// message the Python `RuntimeError` carried.
    Api(String),
    /// A response whose `data` member was not an object, or a `status` outside
    /// the enum. Python raised `AttributeError` / `ValueError` here.
    BadResponse(String),
    /// Key generation or `client_secret` decryption failed.
    Crypto(CryptoError),
}

impl std::fmt::Display for OnboardError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OnboardError::Http(m) => write!(f, "{m}"),
            OnboardError::Api(m) => write!(f, "{m}"),
            OnboardError::BadResponse(m) => write!(f, "{m}"),
            OnboardError::Crypto(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for OnboardError {}

impl From<CryptoError> for OnboardError {
    fn from(e: CryptoError) -> Self {
        OnboardError::Crypto(e)
    }
}

// ---------------------------------------------------------------------------
// Python value helpers
// ---------------------------------------------------------------------------

/// Python `str(value)` for the JSON value kinds these responses carry.
///
/// `None` -> `"None"`, `True`/`False` capitalized, numbers and strings as-is.
/// Containers fall back to the serde rendering; they never appear in the fields
/// this module stringifies.
fn py_str(value: &Value) -> String {
    match value {
        Value::Null => "None".to_string(),
        Value::Bool(true) => "True".to_string(),
        Value::Bool(false) => "False".to_string(),
        Value::Number(n) => n.to_string(),
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// Python truthiness: `None`, `False`, `0`, `""`, `[]`, `{}` are falsy.
fn py_truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().map(|x| x != 0.0).unwrap_or(true),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

/// Python `value == 0`, used for the `retcode` check. `0`, `0.0` and `False`
/// all compare equal to `0` in Python; everything else (including a missing
/// key, which yields `None`) does not.
fn py_eq_zero(value: Option<&Value>) -> bool {
    match value {
        Some(Value::Number(n)) => n.as_f64() == Some(0.0),
        Some(Value::Bool(b)) => !*b,
        _ => false,
    }
}

/// `mapping.get(key, default)` where a missing key falls back to `default` but
/// an explicit null does not.
fn dict_get<'a>(data: &'a Value, key: &str) -> Option<&'a Value> {
    data.as_object().and_then(|m| m.get(key))
}

/// `data.get("msg", fallback)` rendered through `str(RuntimeError(...))`.
/// An explicit `null` becomes `"None"`, matching `str(RuntimeError(None))`.
fn error_message(data: &Value, fallback: &str) -> String {
    match dict_get(data, "msg") {
        Some(v) => py_str(v),
        None => fallback.to_string(),
    }
}

/// Python `urllib.parse.quote(s)`: percent-encode everything except the
/// unreserved set `A-Z a-z 0-9 _ . - ~` plus the default safe character `/`.
/// Encoding is applied to the UTF-8 bytes, uppercase hex, as CPython does.
fn quote(s: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut out = String::with_capacity(s.len());
    for b in s.as_bytes() {
        let c = *b;
        let safe = c.is_ascii_alphanumeric() || matches!(c, b'_' | b'.' | b'-' | b'~' | b'/');
        if safe {
            out.push(c as char);
        } else {
            out.push('%');
            out.push(HEX[(c >> 4) as usize] as char);
            out.push(HEX[(c & 0x0f) as usize] as char);
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Injectable IO
// ---------------------------------------------------------------------------

/// Everything the flow needs from the outside world.
///
/// The Python module reached straight for `httpx`, `time` and `print`. Bundling
/// those into one trait keeps the ported logic (URL and body construction,
/// response parsing, the retry/refresh state machine) pure and testable, and it
/// is the seam the unit tests below drive with a stub.
#[async_trait]
pub trait OnboardIo: Send + Sync {
    /// One `client.post(url, json=body, headers=headers)` with the given
    /// timeout, following redirects, then `raise_for_status()` and `.json()`.
    async fn post_json(
        &self,
        url: &str,
        body: &Value,
        headers: &[(&'static str, String)],
        timeout: f64,
    ) -> Result<Value, OnboardError>;

    /// `time.sleep(seconds)`.
    async fn sleep(&self, seconds: f64);

    /// `time.monotonic()`, in seconds.
    fn monotonic(&self) -> f64;

    /// `print(line)`. Called once per output line; an empty string is a bare
    /// `print()`.
    fn print_line(&self, line: &str) {
        println!("{line}");
    }

    /// `generate_bind_key()`. Part of the seam only so tests can pin the AES
    /// key and exercise the real decrypt on the completed branch; the default
    /// is the ported CSPRNG helper, which is what production uses.
    fn generate_bind_key(&self) -> Result<String, CryptoError> {
        crate::qqbot_crypto::generate_bind_key()
    }

    /// `_render_qr(url)`. The default is the "qrcode not installed" branch,
    /// since no QR crate is vendored here. Returns true when a QR code was
    /// actually drawn.
    fn render_qr(&self, url: &str) -> bool {
        let _ = url;
        false
    }
}

/// The real [`OnboardIo`]: `reqwest` for HTTP, `tokio` for sleeping, `Instant`
/// for the monotonic clock, stdout for printing.
pub struct ReqwestOnboardIo {
    start: Instant,
}

impl Default for ReqwestOnboardIo {
    fn default() -> Self {
        Self::new()
    }
}

impl ReqwestOnboardIo {
    pub fn new() -> Self {
        Self {
            start: Instant::now(),
        }
    }
}

#[async_trait]
impl OnboardIo for ReqwestOnboardIo {
    async fn post_json(
        &self,
        url: &str,
        body: &Value,
        headers: &[(&'static str, String)],
        timeout: f64,
    ) -> Result<Value, OnboardError> {
        // `with httpx.Client(timeout=..., follow_redirects=True)`: a client per
        // call, redirects followed (reqwest's default policy follows too).
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs_f64(timeout))
            .build()
            .map_err(|e| OnboardError::Http(format!("build http client: {e}")))?;

        let mut req = client.post(url).json(body);
        for (name, value) in headers {
            req = req.header(*name, value.as_str());
        }
        let resp = req
            .send()
            .await
            .map_err(|e| OnboardError::Http(e.to_string()))?;
        // resp.raise_for_status()
        let resp = resp
            .error_for_status()
            .map_err(|e| OnboardError::Http(e.to_string()))?;
        resp.json::<Value>()
            .await
            .map_err(|e| OnboardError::Http(e.to_string()))
    }

    async fn sleep(&self, seconds: f64) {
        tokio::time::sleep(Duration::from_secs_f64(seconds)).await;
    }

    fn monotonic(&self) -> f64 {
        self.start.elapsed().as_secs_f64()
    }
}

// ---------------------------------------------------------------------------
// URL builders
// ---------------------------------------------------------------------------

/// `f"https://{PORTAL_HOST}{ONBOARD_CREATE_PATH}"`.
pub fn create_bind_task_url() -> String {
    format!("https://{}{}", portal_host(), ONBOARD_CREATE_PATH)
}

/// `f"https://{PORTAL_HOST}{ONBOARD_POLL_PATH}"`.
pub fn poll_bind_result_url() -> String {
    format!("https://{}{}", portal_host(), ONBOARD_POLL_PATH)
}

/// Build the QR-code target URL for a given `task_id`.
///
/// Port of `build_connect_url`: `QR_URL_TEMPLATE.format(task_id=quote(task_id))`.
pub fn build_connect_url(task_id: &str) -> String {
    qr_url(&quote(task_id))
}

// ---------------------------------------------------------------------------
// HTTP helpers
// ---------------------------------------------------------------------------

/// Create a bind task and return `(task_id, aes_key_base64)`.
///
/// Port of `_create_bind_task`. Errors on a non-zero `retcode` or a missing
/// `task_id`, carrying the same messages the Python `RuntimeError` did.
pub async fn create_bind_task(
    io: &dyn OnboardIo,
    py_version: &str,
    timeout: f64,
) -> Result<(String, String), OnboardError> {
    let url = create_bind_task_url();
    let key = io.generate_bind_key()?;

    let headers = get_api_headers(py_version);
    let data = io
        .post_json(&url, &json!({ "key": key }), &headers, timeout)
        .await?;

    if !py_eq_zero(dict_get(&data, "retcode")) {
        return Err(OnboardError::Api(error_message(
            &data,
            "create_bind_task failed",
        )));
    }

    // `(data.get("data") or {}).get("task_id")`: a falsy `data` member becomes
    // an empty dict; a truthy non-dict would raise AttributeError in Python.
    let inner = dict_get(&data, "data").filter(|v| py_truthy(v));
    let task_id = match inner {
        None => None,
        Some(Value::Object(m)) => m.get("task_id"),
        Some(other) => {
            return Err(OnboardError::BadResponse(format!(
                "'{}' object has no attribute 'get'",
                json_type_name(other)
            )))
        }
    };

    let task_id = match task_id {
        Some(v) if py_truthy(v) => py_str(v),
        _ => {
            return Err(OnboardError::Api(
                "create_bind_task: missing task_id in response".to_string(),
            ))
        }
    };

    tracing::debug!(task_id = %task_id, "create_bind_task ok");
    Ok((task_id, key))
}

/// Result of one `poll_bind_result` call: `(status, bot_appid,
/// bot_encrypt_secret, user_openid)` in the Python tuple's order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PollResult {
    pub status: BindStatus,
    pub bot_appid: String,
    pub bot_encrypt_secret: String,
    pub user_openid: String,
}

/// Poll the bind result for `task_id`. Port of `_poll_bind_result`.
pub async fn poll_bind_result(
    io: &dyn OnboardIo,
    py_version: &str,
    task_id: &str,
    timeout: f64,
) -> Result<PollResult, OnboardError> {
    let url = poll_bind_result_url();

    let headers = get_api_headers(py_version);
    let data = io
        .post_json(&url, &json!({ "task_id": task_id }), &headers, timeout)
        .await?;

    if !py_eq_zero(dict_get(&data, "retcode")) {
        return Err(OnboardError::Api(error_message(
            &data,
            "poll_bind_result failed",
        )));
    }

    // `d = data.get("data", {})`: a missing key defaults to an empty dict, but
    // an explicit null (or any non-dict) leaves `d.get(...)` raising
    // AttributeError, which the caller's poll loop swallows.
    let empty = Value::Object(serde_json::Map::new());
    let d = match dict_get(&data, "data") {
        None => &empty,
        Some(v @ Value::Object(_)) => v,
        Some(other) => {
            return Err(OnboardError::BadResponse(format!(
                "'{}' object has no attribute 'get'",
                json_type_name(other)
            )))
        }
    };

    let status = match dict_get(d, "status") {
        Some(v) => BindStatus::from_json(v)?,
        None => BindStatus::None, // d.get("status", 0)
    };

    Ok(PollResult {
        status,
        // str(d.get("bot_appid", ""))
        bot_appid: dict_get(d, "bot_appid").map(py_str).unwrap_or_default(),
        // d.get("bot_encrypt_secret", "") is handed straight to decrypt_secret,
        // which needs a string; stringify anything else the way Python's
        // base64 decoder would have choked on.
        bot_encrypt_secret: dict_get(d, "bot_encrypt_secret")
            .map(py_str)
            .unwrap_or_default(),
        user_openid: dict_get(d, "user_openid").map(py_str).unwrap_or_default(),
    })
}

/// The Python type name a value would report in an AttributeError.
fn json_type_name(v: &Value) -> &'static str {
    match v {
        Value::Null => "NoneType",
        Value::Bool(_) => "bool",
        Value::Number(n) => {
            if n.is_f64() {
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

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

const MAX_REFRESHES: i64 = 3;

/// What a successful registration hands back. Port of the Python dict
/// `{"app_id": ..., "client_secret": ..., "user_openid": ...}`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Registration {
    pub app_id: String,
    pub client_secret: String,
    pub user_openid: String,
}

/// Run the QQBot scan-to-configure QR registration flow.
///
/// Port of `qr_register`. Handles create -> display -> poll -> decrypt in one
/// call.
///
/// Returns `Ok(Some(registration))` on success and `Ok(None)` on failure,
/// expiry, or timeout (the Python `None` returns). Unexpected errors propagate,
/// as in Python: a `decrypt_secret` failure after a completed scan comes back as
/// `Err`.
///
/// `py_version` is forwarded to `get_api_headers`; see the module docs.
pub async fn qr_register(
    io: &dyn OnboardIo,
    py_version: &str,
    timeout_seconds: i64,
) -> Result<Option<Registration>, OnboardError> {
    let deadline = io.monotonic() + timeout_seconds as f64;

    for refresh_count in 0..=MAX_REFRESHES {
        // -- Create bind task --
        let (task_id, aes_key) = match create_bind_task(io, py_version, ONBOARD_API_TIMEOUT).await {
            Ok(pair) => pair,
            Err(exc) => {
                tracing::warn!("[QQBot onboard] Failed to create bind task: {exc}");
                return Ok(None);
            }
        };

        let url = build_connect_url(&task_id);

        // -- Display QR code + URL --
        io.print_line("");
        if io.render_qr(&url) {
            io.print_line(&format!(
                "  Scan the QR code above, or open this URL directly:\n  {url}"
            ));
        } else {
            io.print_line(&format!("  Open this URL in QQ on your phone:\n  {url}"));
            io.print_line("  Tip: pip install qrcode  to display a scannable QR code here");
        }
        io.print_line("");

        // -- Poll loop --
        // `while ... else` in Python: the else arm runs only when the condition
        // goes false, not when the loop breaks. `expired` records that break.
        let mut expired = false;
        while io.monotonic() < deadline {
            let poll = match poll_bind_result(io, py_version, &task_id, ONBOARD_API_TIMEOUT).await {
                Ok(p) => p,
                Err(_) => {
                    io.sleep(ONBOARD_POLL_INTERVAL).await;
                    continue;
                }
            };

            if poll.status == BindStatus::Completed {
                // Not guarded in Python: a decrypt failure escapes qr_register.
                let client_secret = decrypt_secret(&poll.bot_encrypt_secret, &aes_key)?;
                io.print_line("");
                io.print_line(&format!("  QR scan complete! (App ID: {})", poll.bot_appid));
                if !poll.user_openid.is_empty() {
                    io.print_line(&format!("  Scanner's OpenID: {}", poll.user_openid));
                }
                return Ok(Some(Registration {
                    app_id: poll.bot_appid,
                    client_secret,
                    user_openid: poll.user_openid,
                }));
            }

            if poll.status == BindStatus::Expired {
                if refresh_count >= MAX_REFRESHES {
                    tracing::warn!(
                        "[QQBot onboard] QR code expired {MAX_REFRESHES} times - giving up"
                    );
                    return Ok(None);
                }
                io.print_line(&format!(
                    "\n  QR code expired, refreshing... ({}/{})",
                    refresh_count + 1,
                    MAX_REFRESHES
                ));
                expired = true;
                break; // next for-loop iteration creates a new task
            }

            io.sleep(ONBOARD_POLL_INTERVAL).await;
        }

        if !expired {
            // deadline reached without completing
            tracing::warn!("[QQBot onboard] Poll timed out after {timeout_seconds}s");
            return Ok(None);
        }
    }

    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    // ---- stub IO -------------------------------------------------------

    /// Scripted responses keyed by nothing but call order, per endpoint.
    struct StubIo {
        create: Mutex<Vec<Result<Value, OnboardError>>>,
        poll: Mutex<Vec<Result<Value, OnboardError>>>,
        /// Every (url, body, timeout) that was posted, in order.
        requests: Mutex<Vec<(String, Value, f64)>>,
        headers_seen: Mutex<Vec<Vec<(String, String)>>>,
        printed: Mutex<Vec<String>>,
        /// Virtual monotonic clock, advanced by `sleep`.
        clock: Mutex<f64>,
        slept: Mutex<Vec<f64>>,
        render: bool,
        /// When set, `generate_bind_key` returns this instead of minting one,
        /// so a golden ciphertext can actually be decrypted by the flow.
        bind_key: Option<String>,
    }

    impl StubIo {
        fn new() -> Self {
            Self {
                create: Mutex::new(Vec::new()),
                poll: Mutex::new(Vec::new()),
                requests: Mutex::new(Vec::new()),
                headers_seen: Mutex::new(Vec::new()),
                printed: Mutex::new(Vec::new()),
                clock: Mutex::new(0.0),
                slept: Mutex::new(Vec::new()),
                render: false,
                bind_key: None,
            }
        }

        fn with_create(self, responses: Vec<Result<Value, OnboardError>>) -> Self {
            *self.create.lock().unwrap() = responses;
            self
        }

        fn with_poll(self, responses: Vec<Result<Value, OnboardError>>) -> Self {
            *self.poll.lock().unwrap() = responses;
            self
        }

        fn printed(&self) -> Vec<String> {
            self.printed.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl OnboardIo for StubIo {
        async fn post_json(
            &self,
            url: &str,
            body: &Value,
            headers: &[(&'static str, String)],
            timeout: f64,
        ) -> Result<Value, OnboardError> {
            self.requests
                .lock()
                .unwrap()
                .push((url.to_string(), body.clone(), timeout));
            self.headers_seen.lock().unwrap().push(
                headers
                    .iter()
                    .map(|(k, v)| (k.to_string(), v.clone()))
                    .collect(),
            );
            let queue = if url.ends_with(ONBOARD_CREATE_PATH) {
                &self.create
            } else {
                &self.poll
            };
            let mut q = queue.lock().unwrap();
            if q.is_empty() {
                return Err(OnboardError::Http("stub: no scripted response".into()));
            }
            q.remove(0)
        }

        async fn sleep(&self, seconds: f64) {
            self.slept.lock().unwrap().push(seconds);
            *self.clock.lock().unwrap() += seconds;
        }

        fn monotonic(&self) -> f64 {
            *self.clock.lock().unwrap()
        }

        fn print_line(&self, line: &str) {
            self.printed.lock().unwrap().push(line.to_string());
        }

        fn render_qr(&self, _url: &str) -> bool {
            self.render
        }

        fn generate_bind_key(&self) -> Result<String, CryptoError> {
            match &self.bind_key {
                Some(k) => Ok(k.clone()),
                None => crate::qqbot_crypto::generate_bind_key(),
            }
        }
    }

    // AES-256-GCM golden vector from the ported crypto module (produced by the
    // real Python `cryptography` AESGCM): key, ciphertext, plaintext.
    const KEY_B64: &str = "AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8=";
    const ENC_B64: &str = "AAECAwQFBgcICQoLL2e6d6rIsX7uM/L/DeMCKy8+Lerg/b7GjaOWOA==";
    const PLAINTEXT: &str = "hello-secret";

    fn ok(v: Value) -> Result<Value, OnboardError> {
        Ok(v)
    }

    // ---- pure helpers, locked against real Python ----------------------

    #[test]
    fn quote_matches_python_urllib() {
        // python3 -c "from urllib.parse import quote; ..."
        assert_eq!(quote("task-123"), "task-123");
        assert_eq!(quote("a b/c#d"), "a%20b/c%23d");
        assert_eq!(quote("任务"), "%E4%BB%BB%E5%8A%A1");
        assert_eq!(quote("x?y=z&w"), "x%3Fy%3Dz%26w");
        assert_eq!(quote("a+b"), "a%2Bb");
        assert_eq!(quote("~_.-"), "~_.-");
        assert_eq!(quote("%"), "%25");
        assert_eq!(quote(""), "");
    }

    #[test]
    fn connect_url_matches_python() {
        // QR_URL_TEMPLATE.format(task_id=quote(task_id)) under real constants.py
        assert_eq!(
            build_connect_url("task-123"),
            "https://q.qq.com/qqbot/openclaw/connect.html?task_id=task-123&_wv=2&source=hermes"
        );
        assert_eq!(
            build_connect_url("a b/c#d"),
            "https://q.qq.com/qqbot/openclaw/connect.html?task_id=a%20b/c%23d&_wv=2&source=hermes"
        );
    }

    #[test]
    fn endpoint_urls_match_python() {
        // Env is process-global; the loader tests share this lock.
        let _guard = crate::secret_scope::GLOBAL_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        std::env::remove_var("QQ_PORTAL_HOST");
        assert_eq!(
            create_bind_task_url(),
            "https://q.qq.com/lite/create_bind_task"
        );
        assert_eq!(
            poll_bind_result_url(),
            "https://q.qq.com/lite/poll_bind_result"
        );
        // The portal host is configurable, and both URLs follow it.
        std::env::set_var("QQ_PORTAL_HOST", "proxy.internal");
        assert_eq!(
            create_bind_task_url(),
            "https://proxy.internal/lite/create_bind_task"
        );
        assert_eq!(
            poll_bind_result_url(),
            "https://proxy.internal/lite/poll_bind_result"
        );
        std::env::remove_var("QQ_PORTAL_HOST");
    }

    #[test]
    fn bind_status_values_match_python_intenum() {
        assert_eq!(BindStatus::None.as_i64(), 0);
        assert_eq!(BindStatus::Pending.as_i64(), 1);
        assert_eq!(BindStatus::Completed.as_i64(), 2);
        assert_eq!(BindStatus::Expired.as_i64(), 3);
        assert_eq!(
            BindStatus::from_json(&json!(2)).unwrap(),
            BindStatus::Completed
        );
        // Enum lookup is a dict lookup: 2.0 and True hash-equal 2 and 1.
        assert_eq!(
            BindStatus::from_json(&json!(2.0)).unwrap(),
            BindStatus::Completed
        );
        assert_eq!(
            BindStatus::from_json(&json!(true)).unwrap(),
            BindStatus::Pending
        );
        // Anything else is a ValueError in Python.
        assert!(BindStatus::from_json(&json!(7)).is_err());
        assert!(BindStatus::from_json(&json!(null)).is_err());
        assert!(BindStatus::from_json(&json!("2")).is_err());
    }

    #[test]
    fn python_value_semantics() {
        assert!(py_eq_zero(Some(&json!(0))));
        assert!(py_eq_zero(Some(&json!(0.0))));
        assert!(py_eq_zero(Some(&json!(false)))); // False == 0 in Python
        assert!(!py_eq_zero(Some(&json!(1))));
        assert!(!py_eq_zero(Some(&json!(null)))); // None != 0
        assert!(!py_eq_zero(None)); // missing key -> None

        assert!(!py_truthy(&json!("")));
        assert!(!py_truthy(&json!(0)));
        assert!(!py_truthy(&json!({})));
        assert!(py_truthy(&json!("x")));

        // str(RuntimeError(None)) == "None"
        assert_eq!(error_message(&json!({"msg": null}), "fallback"), "None");
        assert_eq!(error_message(&json!({}), "fallback"), "fallback");
        assert_eq!(error_message(&json!({"msg": "boom"}), "fallback"), "boom");
    }

    // ---- create_bind_task ---------------------------------------------

    #[tokio::test]
    async fn create_bind_task_posts_expected_request() {
        let io = StubIo::new().with_create(vec![ok(json!({
            "retcode": 0,
            "data": {"task_id": "T-1"}
        }))]);
        let (task_id, key) = create_bind_task(&io, "3.11.9", ONBOARD_API_TIMEOUT)
            .await
            .unwrap();
        assert_eq!(task_id, "T-1");
        assert_eq!(key.len(), 44); // base64 of 32 random bytes

        let reqs = io.requests.lock().unwrap();
        assert_eq!(reqs.len(), 1);
        assert!(reqs[0].0.ends_with("/lite/create_bind_task"));
        assert_eq!(reqs[0].1, json!({ "key": key }));
        assert_eq!(reqs[0].2, 10.0); // ONBOARD_API_TIMEOUT

        let headers = io.headers_seen.lock().unwrap();
        assert_eq!(headers[0][0].0, "Content-Type");
        assert_eq!(
            headers[0][1],
            ("Accept".to_string(), "application/json".to_string())
        );
        assert_eq!(headers[0][2].0, "User-Agent");
        assert!(headers[0][2].1.contains("Python/3.11.9"));
    }

    #[tokio::test]
    async fn create_bind_task_error_paths() {
        // Non-zero retcode carries the server message.
        let io = StubIo::new().with_create(vec![ok(json!({"retcode": 40001, "msg": "nope"}))]);
        assert_eq!(
            create_bind_task(&io, "3.11.9", 10.0).await,
            Err(OnboardError::Api("nope".into()))
        );

        // Non-zero retcode with no msg falls back to the fixed string.
        let io = StubIo::new().with_create(vec![ok(json!({"retcode": 1}))]);
        assert_eq!(
            create_bind_task(&io, "3.11.9", 10.0).await,
            Err(OnboardError::Api("create_bind_task failed".into()))
        );

        // Missing retcode is None != 0 -> also an error.
        let io = StubIo::new().with_create(vec![ok(json!({"data": {"task_id": "T"}}))]);
        assert!(matches!(
            create_bind_task(&io, "3.11.9", 10.0).await,
            Err(OnboardError::Api(_))
        ));

        // retcode 0 but no task_id.
        for body in [
            json!({"retcode": 0}),
            json!({"retcode": 0, "data": null}),
            json!({"retcode": 0, "data": {}}),
            json!({"retcode": 0, "data": {"task_id": ""}}),
        ] {
            let io = StubIo::new().with_create(vec![ok(body)]);
            assert_eq!(
                create_bind_task(&io, "3.11.9", 10.0).await,
                Err(OnboardError::Api(
                    "create_bind_task: missing task_id in response".into()
                ))
            );
        }
    }

    // ---- poll_bind_result ---------------------------------------------

    #[tokio::test]
    async fn poll_bind_result_parses_response() {
        let io = StubIo::new().with_poll(vec![ok(json!({
            "retcode": 0,
            "data": {
                "status": 2,
                "bot_appid": 102030405,
                "bot_encrypt_secret": ENC_B64,
                "user_openid": "OPENID-1"
            }
        }))]);
        let got = poll_bind_result(&io, "3.11.9", "T-1", ONBOARD_API_TIMEOUT)
            .await
            .unwrap();
        assert_eq!(
            got,
            PollResult {
                status: BindStatus::Completed,
                // str(102030405) - Python stringifies bot_appid unconditionally
                bot_appid: "102030405".into(),
                bot_encrypt_secret: ENC_B64.into(),
                user_openid: "OPENID-1".into(),
            }
        );
        let reqs = io.requests.lock().unwrap();
        assert!(reqs[0].0.ends_with("/lite/poll_bind_result"));
        assert_eq!(reqs[0].1, json!({"task_id": "T-1"}));
    }

    #[tokio::test]
    async fn poll_bind_result_defaults_and_errors() {
        // Missing "data" defaults to {} -> status 0, empty strings.
        let io = StubIo::new().with_poll(vec![ok(json!({"retcode": 0}))]);
        let got = poll_bind_result(&io, "3.11.9", "T", 10.0).await.unwrap();
        assert_eq!(got.status, BindStatus::None);
        assert_eq!(got.bot_appid, "");
        assert_eq!(got.bot_encrypt_secret, "");
        assert_eq!(got.user_openid, "");

        // Explicit null "data" is not a dict -> AttributeError analog.
        let io = StubIo::new().with_poll(vec![ok(json!({"retcode": 0, "data": null}))]);
        assert!(matches!(
            poll_bind_result(&io, "3.11.9", "T", 10.0).await,
            Err(OnboardError::BadResponse(_))
        ));

        // Bad retcode.
        let io = StubIo::new().with_poll(vec![ok(json!({"retcode": 9, "msg": "bad task"}))]);
        assert_eq!(
            poll_bind_result(&io, "3.11.9", "T", 10.0).await,
            Err(OnboardError::Api("bad task".into()))
        );
        let io = StubIo::new().with_poll(vec![ok(json!({"retcode": 9}))]);
        assert_eq!(
            poll_bind_result(&io, "3.11.9", "T", 10.0).await,
            Err(OnboardError::Api("poll_bind_result failed".into()))
        );
    }

    // ---- qr_register flow ---------------------------------------------

    fn completed_body() -> Value {
        json!({
            "retcode": 0,
            "data": {
                "status": 2,
                "bot_appid": "APPID-9",
                "bot_encrypt_secret": ENC_B64,
                "user_openid": "OPENID-9"
            }
        })
    }

    #[tokio::test]
    async fn qr_register_success_first_poll() {
        // The pinned bind key matches the golden ciphertext, so the completed
        // branch runs the real decrypt and returns the golden plaintext.
        let mut stub = StubIo::new()
            .with_create(vec![ok(json!({"retcode": 0, "data": {"task_id": "T-1"}}))])
            .with_poll(vec![ok(completed_body())]);
        stub.bind_key = Some(KEY_B64.to_string());

        let got = qr_register(&stub, "3.11.9", 600).await.unwrap();
        assert_eq!(
            got,
            Some(Registration {
                app_id: "APPID-9".into(),
                client_secret: PLAINTEXT.into(),
                user_openid: "OPENID-9".into(),
            })
        );

        // The key really did travel in the create body.
        assert_eq!(
            stub.requests.lock().unwrap()[0].1,
            json!({ "key": KEY_B64 })
        );

        // Exact user-facing output, matching the Python print() calls.
        assert_eq!(
            stub.printed(),
            vec![
                "".to_string(),
                "  Open this URL in QQ on your phone:\n  https://q.qq.com/qqbot/openclaw/connect.html?task_id=T-1&_wv=2&source=hermes".to_string(),
                "  Tip: pip install qrcode  to display a scannable QR code here".to_string(),
                "".to_string(),
                "".to_string(),
                "  QR scan complete! (App ID: APPID-9)".to_string(),
                "  Scanner's OpenID: OPENID-9".to_string(),
            ]
        );
        // No sleeping: the very first poll completed.
        assert!(stub.slept.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn qr_register_omits_openid_line_when_empty() {
        let mut stub = StubIo::new()
            .with_create(vec![ok(json!({"retcode": 0, "data": {"task_id": "T"}}))])
            .with_poll(vec![ok(json!({
                "retcode": 0,
                "data": {
                    "status": 2,
                    "bot_appid": "A",
                    "bot_encrypt_secret": ENC_B64,
                    "user_openid": ""
                }
            }))]);
        stub.bind_key = Some(KEY_B64.to_string());
        let got = qr_register(&stub, "3.11.9", 600).await.unwrap().unwrap();
        assert_eq!(got.user_openid, "");
        assert_eq!(got.client_secret, PLAINTEXT);
        assert!(!stub.printed().iter().any(|l| l.contains("OpenID")));
    }

    #[tokio::test]
    async fn qr_register_pending_then_success() {
        // Pending, then a transport error (swallowed), then completion. Each
        // non-terminal round sleeps one poll interval.
        let mut stub = StubIo::new()
            .with_create(vec![ok(json!({"retcode": 0, "data": {"task_id": "T-2"}}))])
            .with_poll(vec![
                ok(json!({"retcode": 0, "data": {"status": 1}})), // pending
                Err(OnboardError::Http("transient".into())),      // swallowed
                ok(json!({"retcode": 0, "data": {"status": 99}})), // ValueError, swallowed
                ok(completed_body()),
            ]);
        stub.bind_key = Some(KEY_B64.to_string());

        let got = qr_register(&stub, "3.11.9", 600).await.unwrap().unwrap();
        assert_eq!(got.client_secret, PLAINTEXT);
        // Three sleeps of ONBOARD_POLL_INTERVAL before the completion.
        assert_eq!(*stub.slept.lock().unwrap(), vec![2.0, 2.0, 2.0]);
        assert_eq!(stub.requests.lock().unwrap().len(), 5); // 1 create + 4 polls
    }

    #[tokio::test]
    async fn qr_register_decrypt_failure_propagates() {
        // A completed scan whose secret will not decrypt: Python let this
        // exception escape qr_register, so it is an Err here, not Ok(None).
        let mut stub = StubIo::new()
            .with_create(vec![ok(json!({"retcode": 0, "data": {"task_id": "T"}}))])
            .with_poll(vec![ok(completed_body())]);
        // A valid 32-byte key, but the wrong one for the golden ciphertext.
        stub.bind_key = Some("AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=".to_string());
        assert_eq!(
            qr_register(&stub, "3.11.9", 600).await,
            Err(OnboardError::Crypto(CryptoError::Decrypt))
        );
        // The success lines were never printed.
        assert!(!stub
            .printed()
            .iter()
            .any(|l| l.contains("QR scan complete")));
    }

    #[tokio::test]
    async fn qr_register_bad_base64_secret_propagates() {
        let mut stub = StubIo::new()
            .with_create(vec![ok(json!({"retcode": 0, "data": {"task_id": "T"}}))])
            .with_poll(vec![ok(json!({
                "retcode": 0,
                "data": {"status": 2, "bot_appid": "A", "bot_encrypt_secret": "not base64!"}
            }))]);
        stub.bind_key = Some(KEY_B64.to_string());
        assert_eq!(
            qr_register(&stub, "3.11.9", 600).await,
            Err(OnboardError::Crypto(CryptoError::InvalidBase64))
        );
    }

    #[tokio::test]
    async fn qr_register_timeout_returns_none() {
        // Deadline is 5 virtual seconds; each pending poll sleeps 2, so the
        // third loop check crosses it and the while/else arm returns None.
        let io = StubIo::new()
            .with_create(vec![ok(json!({"retcode": 0, "data": {"task_id": "T-3"}}))])
            .with_poll(vec![
                ok(json!({"retcode": 0, "data": {"status": 1}})),
                ok(json!({"retcode": 0, "data": {"status": 1}})),
                ok(json!({"retcode": 0, "data": {"status": 1}})),
            ]);
        assert_eq!(qr_register(&io, "3.11.9", 5).await.unwrap(), None);
        assert_eq!(io.requests.lock().unwrap().len(), 4); // 1 create + 3 polls
    }

    #[tokio::test]
    async fn qr_register_zero_timeout_never_polls() {
        // deadline == now, so the while condition is false immediately.
        let io = StubIo::new()
            .with_create(vec![ok(json!({"retcode": 0, "data": {"task_id": "T"}}))])
            .with_poll(vec![]);
        assert_eq!(qr_register(&io, "3.11.9", 0).await.unwrap(), None);
        assert_eq!(io.requests.lock().unwrap().len(), 1); // create only
    }

    #[tokio::test]
    async fn qr_register_expiry_refreshes_then_gives_up() {
        // Four expiries: three refresh, the fourth (refresh_count == 3) gives up.
        let expired = || ok(json!({"retcode": 0, "data": {"status": 3}}));
        let create = || ok(json!({"retcode": 0, "data": {"task_id": "T"}}));
        let io = StubIo::new()
            .with_create(vec![create(), create(), create(), create()])
            .with_poll(vec![expired(), expired(), expired(), expired()]);
        assert_eq!(qr_register(&io, "3.11.9", 600).await.unwrap(), None);

        // Four create calls, four polls.
        assert_eq!(io.requests.lock().unwrap().len(), 8);
        let printed = io.printed();
        let refresh: Vec<&String> = printed
            .iter()
            .filter(|l| l.contains("QR code expired"))
            .collect();
        assert_eq!(refresh.len(), 3);
        assert_eq!(refresh[0], "\n  QR code expired, refreshing... (1/3)");
        assert_eq!(refresh[1], "\n  QR code expired, refreshing... (2/3)");
        assert_eq!(refresh[2], "\n  QR code expired, refreshing... (3/3)");
    }

    #[tokio::test]
    async fn qr_register_create_failure_returns_none() {
        let io = StubIo::new().with_create(vec![Err(OnboardError::Http("boom".into()))]);
        assert_eq!(qr_register(&io, "3.11.9", 600).await.unwrap(), None);
        assert!(io.printed().is_empty()); // returns before the display block
    }

    #[tokio::test]
    async fn qr_register_qr_rendered_branch_text() {
        let mut stub = StubIo::new()
            .with_create(vec![ok(json!({"retcode": 0, "data": {"task_id": "T"}}))])
            .with_poll(vec![]);
        stub.render = true;
        assert_eq!(qr_register(&stub, "3.11.9", 0).await.unwrap(), None);
        let printed = stub.printed();
        assert_eq!(
            printed[1],
            "  Scan the QR code above, or open this URL directly:\n  https://q.qq.com/qqbot/openclaw/connect.html?task_id=T&_wv=2&source=hermes"
        );
        // No pip tip on the rendered branch.
        assert!(!printed.iter().any(|l| l.contains("pip install qrcode")));
    }
}
