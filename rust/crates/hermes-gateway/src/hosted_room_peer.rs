//! Port of gateway/hosted_room_peer.py.
//!
// Public API is ahead of its callers (wired later).
#![allow(dead_code)]
//! Typed contracts for autonomous cross-gateway hosted-room members. This ports
//! the self-contained pure pieces: the [`GatewayRoomCatalog`] capability value
//! type with its strict validating parser and sha256 catalog digest, RoomLink
//! URL validation and transport-security classification ([`validate_room_link_url`]
//! over urllib-style parsing plus by-hand IPv4/IPv6 loopback checks), the
//! [`HostedMemberDispatch`] recipient identity, route selection
//! ([`select_room_link`]), and the target-verifiable grant machinery
//! ([`issue_room_grant`] / [`verify_room_grant`] / [`decode_room_grant`]). HMAC is
//! implemented by hand over sha2 and the canonical JSON byte-matches Python
//! `json.dumps(ensure_ascii=True, sort_keys=True, separators=(",",":"))`. The
//! filesystem grant-secret minting (`gateway_room_grant_secret`,
//! `_gateway_room_grant_secret_for_home`) and the config/env-driven catalog and
//! endpoint builders (`catalog_mapping`, `local_catalog_mapping`,
//! `local_room_link_endpoint`, `_configured_room_link_url`) are deferred: they
//! couple to `hermes_constants`, `gateway.config`, and the config-resolving front
//! half of `execution_policy_mapping`, none of which are ported yet.

use std::collections::HashSet;
use std::net::IpAddr;
use std::str::FromStr;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Map, Number, Value};
use sha2::{Digest, Sha256};

use crate::hosted_room_execution_policy::{RoomExecutionPolicy, RoomExecutionPolicyError};

/// Version 2 grants carry authority/member lineage; not wire-compatible with v1.
pub const PROTOCOL_VERSION: i64 = 2;
pub const MAX_TOKEN_BYTES: usize = 16 * 1024;
pub const MAX_PROMPT_BYTES: usize = 256 * 1024;

/// Dispatch grants live at most a day; status/refresh grants at most a month.
pub const MAX_DISPATCH_GRANT_TTL_SECONDS: f64 = 24.0 * 60.0 * 60.0;
pub const MAX_STATUS_GRANT_TTL_SECONDS: f64 = 30.0 * 24.0 * 60.0 * 60.0;

const GRANT_SECRET_MIN_BYTES: usize = 32;

/// The exact grant payload key set (no refresh field).
const GRANT_FIELDS: [&str; 13] = [
    "version",
    "grant_id",
    "room_id",
    "home_install_id",
    "authority_gateway_id",
    "authority_epoch",
    "member_id",
    "target_install_id",
    "target_profile",
    "execution_policy_digest",
    "permissions",
    "issued_at",
    "expires_at",
];

const GRANT_PERMISSIONS: [&str; 4] = ["approve", "dispatch", "status", "stop"];

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Malformed or unauthorized peer-room input. Mirrors the Python
/// `HostedRoomPeerError(ValueError)` and its `HostedRoomGrantError` subclass:
/// [`HostedRoomPeerError::Grant`] stands in for the grant subclass, and
/// [`HostedRoomPeerError::Policy`] carries a [`RoomExecutionPolicyError`] that
/// propagates unchanged from the ported execution-policy parser, exactly as the
/// distinct Python exception would.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostedRoomPeerError {
    Peer(String),
    Grant(String),
    Policy(RoomExecutionPolicyError),
}

impl HostedRoomPeerError {
    /// The human-readable message text.
    pub fn message(&self) -> String {
        match self {
            HostedRoomPeerError::Peer(m) | HostedRoomPeerError::Grant(m) => m.clone(),
            HostedRoomPeerError::Policy(e) => e.0.clone(),
        }
    }
}

impl std::fmt::Display for HostedRoomPeerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message())
    }
}

impl std::error::Error for HostedRoomPeerError {}

impl From<RoomExecutionPolicyError> for HostedRoomPeerError {
    fn from(value: RoomExecutionPolicyError) -> Self {
        HostedRoomPeerError::Policy(value)
    }
}

fn peer(message: &str) -> HostedRoomPeerError {
    HostedRoomPeerError::Peer(message.to_string())
}

fn grant(message: &str) -> HostedRoomPeerError {
    HostedRoomPeerError::Grant(message.to_string())
}

type PeerResult<T> = Result<T, HostedRoomPeerError>;

// ---------------------------------------------------------------------------
// Crypto helpers (hand-rolled HMAC-SHA256, constant-time compare)
// ---------------------------------------------------------------------------

fn sha256(data: &[u8]) -> [u8; 32] {
    Sha256::digest(data).into()
}

fn sha256_hex(data: &[u8]) -> String {
    let mut out = String::with_capacity(64);
    for b in sha256(data) {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

/// HMAC-SHA256 = H((key XOR opad) || H((key XOR ipad) || msg)). SHA256 uses a
/// 64-byte block; a longer key is first hashed down to 32 bytes.
fn hmac_sha256(key: &[u8], msg: &[u8]) -> [u8; 32] {
    const BLOCK: usize = 64;
    let mut k = [0u8; BLOCK];
    if key.len() > BLOCK {
        k[..32].copy_from_slice(&sha256(key));
    } else {
        k[..key.len()].copy_from_slice(key);
    }
    let mut ipad = [0x36u8; BLOCK];
    let mut opad = [0x5cu8; BLOCK];
    for i in 0..BLOCK {
        ipad[i] ^= k[i];
        opad[i] ^= k[i];
    }
    let mut inner = Sha256::new();
    inner.update(ipad);
    inner.update(msg);
    let inner_digest = inner.finalize();
    let mut outer = Sha256::new();
    outer.update(opad);
    outer.update(inner_digest);
    outer.finalize().into()
}

/// Constant-time equality, mirroring `hmac.compare_digest`. Length mismatch
/// returns false immediately (as Python does for equal-length-only comparison).
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

// ---------------------------------------------------------------------------
// URL-safe base64 without padding
// ---------------------------------------------------------------------------

const B64_ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

/// Port of `_b64encode`: `base64.urlsafe_b64encode(value).rstrip(b"=")`.
fn b64encode(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = if chunk.len() > 1 { chunk[1] as u32 } else { 0 };
        let b2 = if chunk.len() > 2 { chunk[2] as u32 } else { 0 };
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(B64_ALPHABET[((n >> 18) & 63) as usize] as char);
        out.push(B64_ALPHABET[((n >> 12) & 63) as usize] as char);
        if chunk.len() > 1 {
            out.push(B64_ALPHABET[((n >> 6) & 63) as usize] as char);
        }
        if chunk.len() > 2 {
            out.push(B64_ALPHABET[(n & 63) as usize] as char);
        }
    }
    out
}

/// Port of `_b64decode`. Python re-pads then calls `urlsafe_b64decode`, whose
/// non-strict mode silently discards characters outside the alphabet; we mirror
/// that leniency (padding and stray characters are skipped) and reject only a
/// trailing group of a single sextet, which cannot encode any byte.
fn b64decode(value: &str) -> PeerResult<Vec<u8>> {
    let mut sextets: Vec<u8> = Vec::with_capacity(value.len());
    for c in value.bytes() {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'-' => 62,
            b'_' => 63,
            _ => continue,
        };
        sextets.push(v);
    }
    if sextets.len() % 4 == 1 {
        return Err(grant("room grant encoding is invalid"));
    }
    let mut out = Vec::with_capacity(sextets.len() * 3 / 4);
    let mut acc: u32 = 0;
    let mut nbits: u32 = 0;
    for v in sextets {
        acc = (acc << 6) | v as u32;
        nbits += 6;
        if nbits >= 8 {
            nbits -= 8;
            out.push((acc >> nbits) as u8);
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Canonical JSON (byte-matches Python json.dumps with ensure_ascii/sort_keys)
// ---------------------------------------------------------------------------

/// Format a finite f64 the way Python `repr`/`json.dumps` does for the fixed
/// range these payloads use. Rust's Display gives the shortest round-tripping
/// digit string but omits a trailing `.0` on integer-valued floats and never
/// uses exponent notation in this range, so we only need to re-append `.0`.
/// Values outside roughly `[1e-4, 1e16)` where Python switches to exponent form
/// are never produced here (timestamps and TTL sums stay well inside it).
fn python_float_repr(x: f64) -> String {
    if x.is_nan() {
        return "NaN".to_string();
    }
    if x.is_infinite() {
        return if x > 0.0 { "Infinity" } else { "-Infinity" }.to_string();
    }
    let s = format!("{x}");
    if s.contains('.') || s.contains('e') || s.contains('E') {
        s
    } else {
        format!("{s}.0")
    }
}

/// Append `s` to `out` as a JSON string with `ensure_ascii=True` escaping,
/// matching CPython's `py_encode_basestring_ascii`: the short escapes for the
/// control characters that have them, `\u00xx` for the rest below 0x20, literal
/// bytes for 0x20..=0x7e except `"` and `\`, and `\uXXXX` (surrogate pair beyond
/// the BMP) for everything at 0x7f and above.
fn write_json_string(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c if (c as u32) <= 0x7e => out.push(c),
            c => {
                let cp = c as u32;
                if cp <= 0xFFFF {
                    out.push_str(&format!("\\u{cp:04x}"));
                } else {
                    let v = cp - 0x10000;
                    let hi = 0xD800 + (v >> 10);
                    let lo = 0xDC00 + (v & 0x3FF);
                    out.push_str(&format!("\\u{hi:04x}\\u{lo:04x}"));
                }
            }
        }
    }
    out.push('"');
}

fn write_canonical(out: &mut String, value: &Value) {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Number(n) => {
            if n.is_f64() {
                out.push_str(&python_float_repr(n.as_f64().unwrap()));
            } else {
                out.push_str(&n.to_string());
            }
        }
        Value::String(s) => write_json_string(out, s),
        Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_canonical(out, item);
            }
            out.push(']');
        }
        Value::Object(map) => {
            out.push('{');
            // sort_keys=True. UTF-8 byte order equals Unicode code point order,
            // so a plain sort of the key strings matches Python.
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            for (i, key) in keys.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_json_string(out, key);
                out.push(':');
                write_canonical(out, &map[*key]);
            }
            out.push('}');
        }
    }
}

/// Port of `_canonical_json`.
fn canonical_json(value: &Value) -> Vec<u8> {
    let mut out = String::new();
    write_canonical(&mut out, value);
    out.into_bytes()
}

// ---------------------------------------------------------------------------
// Python-truthiness / coercion helpers
// ---------------------------------------------------------------------------

fn is_truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n
            .as_f64()
            .map(|f| f != 0.0)
            .unwrap_or_else(|| n.as_i64().map(|i| i != 0).unwrap_or(true)),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

/// Port of `str(value or "")`.
fn py_str_or_empty(value: &Value) -> String {
    if !is_truthy(value) {
        return String::new();
    }
    match value {
        Value::String(s) => s.clone(),
        Value::Bool(_) => "True".to_string(),
        Value::Number(n) => n.to_string(),
        other => other.to_string(),
    }
}

/// Port of Python `float(value)` for JSON values, returning None where Python
/// would raise TypeError/ValueError.
fn py_float(value: &Value) -> Option<f64> {
    match value {
        Value::Number(n) => n.as_f64(),
        Value::Bool(b) => Some(if *b { 1.0 } else { 0.0 }),
        Value::String(s) => s.trim().parse::<f64>().ok(),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Field validators (identifier / positive int / digest / exact fields)
// ---------------------------------------------------------------------------

/// True if `s` fully matches `^[A-Za-z0-9][A-Za-z0-9._:@/-]{0,255}$` (ASCII only,
/// total length 1..=256).
fn matches_identifier(s: &str) -> bool {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphanumeric() => {}
        _ => return false,
    }
    let mut count = 1usize;
    for c in chars {
        count += 1;
        if count > 256 {
            return false;
        }
        if !(c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | ':' | '@' | '/' | '-')) {
            return false;
        }
    }
    true
}

/// Port of `_identifier`: the value must be a JSON string, is stripped, and must
/// match the identifier regex.
fn identifier(value: &Value, field: &str) -> PeerResult<String> {
    let raw = match value {
        Value::String(s) => s,
        _ => return Err(peer(&format!("{field} must be a string"))),
    };
    let normalized = raw.trim();
    if !matches_identifier(normalized) {
        return Err(peer(&format!("{field} is invalid")));
    }
    Ok(normalized.to_string())
}

/// Port of `_positive_int` over a JSON value: rejects bools and non-integers,
/// requires >= 1.
fn positive_int(value: &Value, field: &str) -> PeerResult<i64> {
    match value {
        Value::Number(n) if !n.is_f64() => match n.as_i64() {
            Some(i) if i >= 1 => Ok(i),
            _ => Err(peer(&format!("{field} must be a positive integer"))),
        },
        _ => Err(peer(&format!("{field} must be a positive integer"))),
    }
}

/// Port of `_positive_int` for an already-typed integer argument.
fn positive_int_value(value: i64, field: &str) -> PeerResult<i64> {
    if value < 1 {
        return Err(peer(&format!("{field} must be a positive integer")));
    }
    Ok(value)
}

/// True if `s` is exactly 64 lowercase hex characters.
fn is_sha256_hex(s: &str) -> bool {
    s.len() == 64
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Port of `_digest`: a JSON string matching `^[0-9a-f]{64}$`.
fn digest(value: &Value, field: &str) -> PeerResult<String> {
    match value {
        Value::String(s) if is_sha256_hex(s) => Ok(s.clone()),
        _ => Err(peer(&format!("{field} must be a sha256 digest"))),
    }
}

/// Port of `_digest` for a `&str` input.
fn digest_str(value: &str, field: &str) -> PeerResult<String> {
    if is_sha256_hex(value) {
        Ok(value.to_string())
    } else {
        Err(peer(&format!("{field} must be a sha256 digest")))
    }
}

/// Port of `_exact_fields`: missing keys are reported first, then unknown ones.
fn exact_fields(
    object: &Map<String, Value>,
    required: &[&str],
    optional: &[&str],
    label: &str,
) -> PeerResult<()> {
    let fields: HashSet<&str> = object.keys().map(String::as_str).collect();
    let required_set: HashSet<&str> = required.iter().copied().collect();
    let optional_set: HashSet<&str> = optional.iter().copied().collect();

    let mut missing: Vec<&str> = required_set.difference(&fields).copied().collect();
    if !missing.is_empty() {
        missing.sort_unstable();
        return Err(peer(&format!(
            "{label} missing fields: {}",
            missing.join(", ")
        )));
    }
    let mut unknown: Vec<&str> = fields
        .iter()
        .filter(|f| !required_set.contains(*f) && !optional_set.contains(*f))
        .copied()
        .collect();
    if !unknown.is_empty() {
        unknown.sort_unstable();
        return Err(peer(&format!(
            "{label} unknown fields: {}",
            unknown.join(", ")
        )));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Link modes and transport security
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkMode {
    Direct,
    Overlay,
    Relay,
    Pull,
    Desktop,
}

impl LinkMode {
    /// Route preference: lower wins (direct < overlay < relay < pull < desktop).
    pub fn priority(self) -> u8 {
        match self {
            LinkMode::Direct => 0,
            LinkMode::Overlay => 1,
            LinkMode::Relay => 2,
            LinkMode::Pull => 3,
            LinkMode::Desktop => 4,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            LinkMode::Direct => "direct",
            LinkMode::Overlay => "overlay",
            LinkMode::Relay => "relay",
            LinkMode::Pull => "pull",
            LinkMode::Desktop => "desktop",
        }
    }

    pub fn parse(value: &str) -> Option<LinkMode> {
        match value {
            "direct" => Some(LinkMode::Direct),
            "overlay" => Some(LinkMode::Overlay),
            "relay" => Some(LinkMode::Relay),
            "pull" => Some(LinkMode::Pull),
            "desktop" => Some(LinkMode::Desktop),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransportSecurity {
    Tls,
    Loopback,
}

impl TransportSecurity {
    pub fn as_str(self) -> &'static str {
        match self {
            TransportSecurity::Tls => "tls",
            TransportSecurity::Loopback => "loopback",
        }
    }
}

// ---------------------------------------------------------------------------
// RoomLink URL validation and transport-security classification
// ---------------------------------------------------------------------------

/// The subset of `urllib.parse.urlsplit` that RoomLink validation depends on.
struct UrlParts {
    scheme: String,
    /// Present iff the netloc contained an `@` (userinfo), like `.username`
    /// being non-None.
    has_userinfo: bool,
    /// Lowercased host with brackets removed for IPv6, or empty when absent.
    hostname: String,
    /// Ok(Some(port)) / Ok(None) / Err when the port is malformed or out of
    /// range, mirroring `SplitResult.port` raising ValueError.
    port: Result<Option<u32>, ()>,
    has_query: bool,
    has_fragment: bool,
}

/// Split a URL the way `urllib.parse.urlsplit` does for the shapes we validate:
/// tab/CR/LF are stripped anywhere, a leading `scheme:` is lowercased when it is
/// a valid scheme token, then fragment, query, and `//netloc` are peeled off.
fn urlsplit(url: &str) -> UrlParts {
    let filtered: String = url
        .chars()
        .filter(|c| !matches!(c, '\t' | '\r' | '\n'))
        .collect();
    let mut rest = filtered.as_str();

    let mut scheme = String::new();
    if let Some(i) = rest.find(':') {
        if i > 0 {
            let prefix = &rest[..i];
            let mut prefix_chars = prefix.chars();
            let first_ok = prefix_chars
                .next()
                .map(|c| c.is_ascii_alphabetic())
                .unwrap_or(false);
            let rest_ok = prefix
                .chars()
                .skip(1)
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'));
            if first_ok && rest_ok {
                scheme = prefix.to_ascii_lowercase();
                rest = &rest[i + 1..];
            }
        }
    }

    let mut has_fragment = false;
    if let Some(h) = rest.find('#') {
        has_fragment = true;
        rest = &rest[..h];
    }
    let mut has_query = false;
    if let Some(q) = rest.find('?') {
        has_query = true;
        rest = &rest[..q];
    }

    let mut netloc = "";
    if let Some(stripped) = rest.strip_prefix("//") {
        let end = stripped.find('/').unwrap_or(stripped.len());
        netloc = &stripped[..end];
    }

    // Split off userinfo on the last '@'.
    let (has_userinfo, hostinfo) = match netloc.rfind('@') {
        Some(a) => (true, &netloc[a + 1..]),
        None => (false, netloc),
    };

    // Host and optional port. IPv6 literals are bracketed.
    let (host_raw, port_str): (&str, Option<&str>) = if hostinfo.starts_with('[') {
        match hostinfo.find(']') {
            Some(end) => {
                let host = &hostinfo[1..end];
                let after = &hostinfo[end + 1..];
                let port = after.strip_prefix(':');
                (host, port)
            }
            None => (hostinfo, None),
        }
    } else {
        match hostinfo.rfind(':') {
            Some(c) => (&hostinfo[..c], Some(&hostinfo[c + 1..])),
            None => (hostinfo, None),
        }
    };

    let port = match port_str {
        None => Ok(None),
        Some("") => Err(()),
        Some(p) if p.bytes().all(|b| b.is_ascii_digit()) => match p.parse::<u32>() {
            Ok(n) if n <= 65535 => Ok(Some(n)),
            _ => Err(()),
        },
        Some(_) => Err(()),
    };

    UrlParts {
        scheme,
        has_userinfo,
        hostname: host_raw.to_ascii_lowercase(),
        port,
        has_query,
        has_fragment,
    }
}

/// Validate a RoomLink endpoint URL and classify its transport protection.
/// Plaintext HTTP is accepted only when the host is loopback (`localhost`, a
/// `.localhost` name, or a loopback IP); every other endpoint must be HTTPS.
/// Takes a JSON value to mirror the Python `Any` argument and its
/// `str(value or "")` coercion.
pub fn validate_room_link_url(value: &Value) -> PeerResult<(String, TransportSecurity)> {
    let raw = py_str_or_empty(value);
    let raw = raw.trim().trim_end_matches('/').to_string();

    let parts = urlsplit(&raw);
    // urllib raises ValueError on a malformed/out-of-range port.
    if parts.port.is_err() {
        return Err(peer("target_url is invalid"));
    }
    let hostname = parts.hostname.trim_end_matches('.').to_string();
    if hostname.is_empty() || parts.has_userinfo {
        return Err(peer("target_url is invalid"));
    }
    if parts.has_query || parts.has_fragment {
        return Err(peer("target_url must not include query or fragment"));
    }
    if parts.scheme == "https" {
        return Ok((raw, TransportSecurity::Tls));
    }
    if parts.scheme != "http" {
        return Err(peer("target_url must use https"));
    }

    let mut loopback = hostname == "localhost" || hostname.ends_with(".localhost");
    if !loopback {
        loopback = IpAddr::from_str(&hostname)
            .map(|ip| ip.is_loopback())
            .unwrap_or(false);
    }
    if !loopback {
        return Err(peer("target_url must use https outside the local machine"));
    }
    Ok((raw, TransportSecurity::Loopback))
}

// ---------------------------------------------------------------------------
// GatewayRoomCatalog
// ---------------------------------------------------------------------------

/// Authenticated gateway capabilities inherited by its Bots.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GatewayRoomCatalog {
    pub installation_id: String,
    pub protocol_versions: Vec<i64>,
    pub link_modes: Vec<LinkMode>,
    pub persistent_process: bool,
    pub text: bool,
    pub attachments: bool,
    pub execution_policy: RoomExecutionPolicy,
    pub catalog_digest: String,
    pub endpoint_url: Option<String>,
    pub endpoint_reason: Option<String>,
    pub transport_security: Option<TransportSecurity>,
}

impl GatewayRoomCatalog {
    /// Port of `GatewayRoomCatalog.from_mapping`.
    pub fn from_mapping(value: &Value) -> PeerResult<GatewayRoomCatalog> {
        let object = match value {
            Value::Object(m) => m,
            _ => return Err(peer("capability catalog fields are invalid")),
        };
        exact_fields(
            object,
            &[
                "installation_id",
                "protocol_versions",
                "link_modes",
                "persistent_process",
                "text",
                "attachments",
                "execution_policy",
                "catalog_digest",
            ],
            &["endpoint"],
            "capability catalog",
        )?;

        let installation_id = identifier(&object["installation_id"], "installation_id")?;

        let versions_raw = match &object["protocol_versions"] {
            Value::Array(a) if !a.is_empty() => a,
            _ => return Err(peer("protocol_versions must be a non-empty list")),
        };
        let mut versions_set: Vec<i64> = Vec::new();
        for item in versions_raw {
            let v = positive_int(item, "protocol_version")?;
            if !versions_set.contains(&v) {
                versions_set.push(v);
            }
        }
        versions_set.sort_unstable();

        let links_raw = match &object["link_modes"] {
            Value::Array(a) if !a.is_empty() => a,
            _ => return Err(peer("link_modes must be a non-empty list")),
        };
        let mut links: Vec<LinkMode> = Vec::new();
        for item in links_raw {
            let mode = match item {
                Value::String(s) => LinkMode::parse(s),
                _ => None,
            };
            let mode = mode.ok_or_else(|| peer("link_modes contains an unsupported mode"))?;
            if !links.contains(&mode) {
                links.push(mode);
            }
        }

        for field in ["persistent_process", "text", "attachments"] {
            if !object[field].is_boolean() {
                return Err(peer(&format!("{field} must be a boolean")));
            }
        }

        // Validate + normalize the execution policy (RoomExecutionPolicyError
        // propagates as the Policy variant, as the distinct Python exception).
        let policy = RoomExecutionPolicy::from_mapping(&object["execution_policy"])?;

        let mut unsigned = Map::new();
        unsigned.insert(
            "installation_id".into(),
            Value::from(installation_id.clone()),
        );
        unsigned.insert(
            "protocol_versions".into(),
            Value::from(versions_set.clone()),
        );
        unsigned.insert(
            "link_modes".into(),
            Value::from(
                links
                    .iter()
                    .map(|m| Value::from(m.as_str()))
                    .collect::<Vec<_>>(),
            ),
        );
        unsigned.insert(
            "persistent_process".into(),
            object["persistent_process"].clone(),
        );
        unsigned.insert("text".into(), object["text"].clone());
        unsigned.insert("attachments".into(), object["attachments"].clone());
        unsigned.insert("execution_policy".into(), policy.as_mapping());

        let mut endpoint_url: Option<String> = None;
        let mut endpoint_reason: Option<String> = None;
        let mut transport_security: Option<TransportSecurity> = None;

        if let Some(endpoint) = object.get("endpoint") {
            let endpoint_obj = match endpoint {
                Value::Object(m) => m,
                _ => return Err(peer("endpoint capability is invalid")),
            };
            let available = match endpoint_obj.get("available") {
                Some(Value::Bool(b)) => *b,
                _ => return Err(peer("endpoint capability is invalid")),
            };
            let normalized_endpoint = if available {
                exact_fields(
                    endpoint_obj,
                    &["available", "url", "transport_security"],
                    &[],
                    "endpoint capability",
                )?;
                let (url, ts) = validate_room_link_url(&endpoint_obj["url"])?;
                let ts_matches = matches!(
                    &endpoint_obj["transport_security"],
                    Value::String(s) if s.as_str() == ts.as_str()
                );
                if !ts_matches {
                    return Err(peer("endpoint transport_security does not match its URL"));
                }
                endpoint_url = Some(url.clone());
                transport_security = Some(ts);
                let mut m = Map::new();
                m.insert("available".into(), Value::Bool(true));
                m.insert("url".into(), Value::from(url));
                m.insert("transport_security".into(), Value::from(ts.as_str()));
                Value::Object(m)
            } else {
                exact_fields(
                    endpoint_obj,
                    &["available", "reason"],
                    &[],
                    "endpoint capability",
                )?;
                let reason = identifier(&endpoint_obj["reason"], "endpoint.reason")?;
                endpoint_reason = Some(reason.clone());
                let mut m = Map::new();
                m.insert("available".into(), Value::Bool(false));
                m.insert("reason".into(), Value::from(reason));
                Value::Object(m)
            };
            unsigned.insert("endpoint".into(), normalized_endpoint);
        }

        let expected = sha256_hex(&canonical_json(&Value::Object(unsigned)));
        let supplied = digest(&object["catalog_digest"], "catalog_digest")?;
        if !constant_time_eq(expected.as_bytes(), supplied.as_bytes()) {
            return Err(peer("catalog_digest does not match the catalog"));
        }

        Ok(GatewayRoomCatalog {
            installation_id,
            protocol_versions: versions_set,
            link_modes: links,
            persistent_process: object["persistent_process"].as_bool().unwrap(),
            text: object["text"].as_bool().unwrap(),
            attachments: object["attachments"].as_bool().unwrap(),
            execution_policy: policy,
            catalog_digest: supplied,
            endpoint_url,
            endpoint_reason,
            transport_security,
        })
    }

    /// Port of `endpoint_mapping`: the normalized self-advertised endpoint.
    pub fn endpoint_mapping(&self) -> Value {
        let mut m = Map::new();
        if let Some(url) = &self.endpoint_url {
            m.insert("available".into(), Value::Bool(true));
            m.insert("url".into(), Value::from(url.clone()));
            m.insert(
                "transport_security".into(),
                self.transport_security
                    .map(|ts| Value::from(ts.as_str()))
                    .unwrap_or(Value::Null),
            );
        } else {
            m.insert("available".into(), Value::Bool(false));
            m.insert(
                "reason".into(),
                Value::from(
                    self.endpoint_reason
                        .clone()
                        .unwrap_or_else(|| "not_configured".to_string()),
                ),
            );
        }
        Value::Object(m)
    }
}

// ---------------------------------------------------------------------------
// HostedMemberDispatch
// ---------------------------------------------------------------------------

/// Recipient-validated identity for one remote room member attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostedMemberDispatch {
    pub protocol_version: i64,
    pub room_id: String,
    pub home_install_id: String,
    pub authority_gateway_id: String,
    pub authority_epoch: i64,
    pub member_id: String,
    pub target_install_id: String,
    pub target_profile: String,
    pub task_id: String,
    pub execution_generation: i64,
    pub source_event_seq: i64,
    pub cancellation_scope_id: String,
    pub prompt: String,
    pub prompt_digest: String,
    pub capability_digest: String,
    pub execution_policy_digest: String,
    pub trace_id: String,
}

impl HostedMemberDispatch {
    /// Port of `as_mapping`: the canonical wire mapping.
    pub fn as_mapping(&self) -> Value {
        let mut m = Map::new();
        m.insert(
            "protocol_version".into(),
            Value::from(self.protocol_version),
        );
        m.insert("room_id".into(), Value::from(self.room_id.clone()));
        m.insert(
            "home_install_id".into(),
            Value::from(self.home_install_id.clone()),
        );
        m.insert(
            "authority_gateway_id".into(),
            Value::from(self.authority_gateway_id.clone()),
        );
        m.insert("authority_epoch".into(), Value::from(self.authority_epoch));
        m.insert("member_id".into(), Value::from(self.member_id.clone()));
        m.insert(
            "target_install_id".into(),
            Value::from(self.target_install_id.clone()),
        );
        m.insert(
            "target_profile".into(),
            Value::from(self.target_profile.clone()),
        );
        m.insert("task_id".into(), Value::from(self.task_id.clone()));
        m.insert(
            "execution_generation".into(),
            Value::from(self.execution_generation),
        );
        m.insert(
            "source_event_seq".into(),
            Value::from(self.source_event_seq),
        );
        m.insert(
            "cancellation_scope_id".into(),
            Value::from(self.cancellation_scope_id.clone()),
        );
        m.insert("prompt".into(), Value::from(self.prompt.clone()));
        m.insert(
            "prompt_digest".into(),
            Value::from(self.prompt_digest.clone()),
        );
        m.insert(
            "capability_digest".into(),
            Value::from(self.capability_digest.clone()),
        );
        m.insert(
            "execution_policy_digest".into(),
            Value::from(self.execution_policy_digest.clone()),
        );
        m.insert("trace_id".into(), Value::from(self.trace_id.clone()));
        Value::Object(m)
    }

    /// Port of `HostedMemberDispatch.from_mapping`.
    pub fn from_mapping(value: &Value) -> PeerResult<HostedMemberDispatch> {
        let object = match value {
            Value::Object(m) => m,
            _ => return Err(peer("dispatch fields are invalid")),
        };
        exact_fields(
            object,
            &[
                "protocol_version",
                "room_id",
                "home_install_id",
                "authority_gateway_id",
                "authority_epoch",
                "member_id",
                "target_install_id",
                "target_profile",
                "task_id",
                "execution_generation",
                "source_event_seq",
                "cancellation_scope_id",
                "prompt",
                "prompt_digest",
                "capability_digest",
                "execution_policy_digest",
                "trace_id",
            ],
            &[],
            "dispatch",
        )?;

        let prompt = match &object["prompt"] {
            Value::String(s) if !s.trim().is_empty() => s.clone(),
            _ => return Err(peer("prompt must be a non-empty string")),
        };
        if prompt.len() > MAX_PROMPT_BYTES {
            return Err(peer("prompt is too large"));
        }
        let expected_prompt_digest = sha256_hex(prompt.as_bytes());
        let prompt_digest = digest(&object["prompt_digest"], "prompt_digest")?;
        if !constant_time_eq(expected_prompt_digest.as_bytes(), prompt_digest.as_bytes()) {
            return Err(peer("prompt_digest does not match prompt"));
        }

        Ok(HostedMemberDispatch {
            protocol_version: positive_int(&object["protocol_version"], "protocol_version")?,
            room_id: identifier(&object["room_id"], "room_id")?,
            home_install_id: identifier(&object["home_install_id"], "home_install_id")?,
            authority_gateway_id: identifier(
                &object["authority_gateway_id"],
                "authority_gateway_id",
            )?,
            authority_epoch: positive_int(&object["authority_epoch"], "authority_epoch")?,
            member_id: identifier(&object["member_id"], "member_id")?,
            target_install_id: identifier(&object["target_install_id"], "target_install_id")?,
            target_profile: identifier(&object["target_profile"], "target_profile")?,
            task_id: identifier(&object["task_id"], "task_id")?,
            execution_generation: positive_int(
                &object["execution_generation"],
                "execution_generation",
            )?,
            source_event_seq: positive_int(&object["source_event_seq"], "source_event_seq")?,
            cancellation_scope_id: identifier(
                &object["cancellation_scope_id"],
                "cancellation_scope_id",
            )?,
            prompt,
            prompt_digest,
            capability_digest: digest(&object["capability_digest"], "capability_digest")?,
            execution_policy_digest: digest(
                &object["execution_policy_digest"],
                "execution_policy_digest",
            )?,
            trace_id: identifier(&object["trace_id"], "trace_id")?,
        })
    }
}

// ---------------------------------------------------------------------------
// Route selection
// ---------------------------------------------------------------------------

/// One gateway-verified route candidate.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RoomLinkProbe {
    pub mode: LinkMode,
    pub verified: bool,
    pub encrypted: bool,
    pub latency_ms: f64,
}

/// Port of `select_room_link`: the fastest verified encrypted non-desktop route,
/// tie-broken by link priority then latency; else a synthetic desktop route when
/// the desktop is available; else None.
pub fn select_room_link(
    probes: &[RoomLinkProbe],
    desktop_available: bool,
) -> Option<RoomLinkProbe> {
    let mut best: Option<RoomLinkProbe> = None;
    for probe in probes {
        if !(probe.verified
            && probe.encrypted
            && probe.mode != LinkMode::Desktop
            && probe.latency_ms.is_finite()
            && probe.latency_ms >= 0.0)
        {
            continue;
        }
        match best {
            None => best = Some(*probe),
            Some(current) => {
                let cur_key = (current.mode.priority(), current.latency_ms);
                let new_key = (probe.mode.priority(), probe.latency_ms);
                // Replace only on a strictly smaller key so ties keep the first
                // candidate, matching Python's min().
                if new_key.0 < cur_key.0 || (new_key.0 == cur_key.0 && new_key.1 < cur_key.1) {
                    best = Some(*probe);
                }
            }
        }
    }
    if best.is_some() {
        return best;
    }
    if desktop_available {
        return Some(RoomLinkProbe {
            mode: LinkMode::Desktop,
            verified: true,
            encrypted: true,
            latency_ms: 0.0,
        });
    }
    None
}

// ---------------------------------------------------------------------------
// Grant issue / verify / decode
// ---------------------------------------------------------------------------

/// Port of `derive_room_grant_secret`: domain-separate room grants from the
/// configured API key. Requires at least 8 characters (Python `len(api_key)`
/// counts code points).
pub fn derive_room_grant_secret(api_key: &str) -> PeerResult<[u8; 32]> {
    if api_key.chars().count() < 8 {
        return Err(grant("room grants require a strong gateway API key"));
    }
    Ok(hmac_sha256(
        api_key.as_bytes(),
        b"hermes-hosted-room-grant-v1",
    ))
}

fn now_seconds() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// Port of `issue_room_grant`. `execution_policy_digest` is required here; the
/// Python `None` default derives it from `execution_policy_mapping`, whose
/// config-driven resolution is deferred, so callers pass the digest directly.
#[allow(clippy::too_many_arguments)]
pub fn issue_room_grant(
    secret: &[u8],
    grant_id: &str,
    room_id: &str,
    home_install_id: &str,
    authority_gateway_id: &str,
    authority_epoch: i64,
    member_id: &str,
    target_install_id: &str,
    target_profile: &str,
    execution_policy_digest: &str,
    permissions: &[&str],
    issued_at: Option<f64>,
    ttl_seconds: f64,
    status_ttl_seconds: Option<f64>,
    status_expires_at: Option<f64>,
) -> PeerResult<String> {
    if secret.len() < GRANT_SECRET_MIN_BYTES {
        return Err(grant("room grant secret must be at least 32 bytes"));
    }
    let now = issued_at.unwrap_or_else(now_seconds);
    let bounded_status_expiry = match status_expires_at {
        Some(v) => v,
        None => now + status_ttl_seconds.unwrap_or(ttl_seconds),
    };
    if !now.is_finite()
        || ttl_seconds <= 0.0
        || ttl_seconds > MAX_DISPATCH_GRANT_TTL_SECONDS
        || !bounded_status_expiry.is_finite()
        || bounded_status_expiry < now + ttl_seconds
        || bounded_status_expiry > now + MAX_STATUS_GRANT_TTL_SECONDS
    {
        return Err(grant("room grant lifetime is invalid"));
    }

    let mut allowed: Vec<String> = permissions
        .iter()
        .map(|p| p.to_string())
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    allowed.sort();
    let permitted: HashSet<&str> = GRANT_PERMISSIONS.iter().copied().collect();
    if allowed.is_empty() || !allowed.iter().all(|p| permitted.contains(p.as_str())) {
        return Err(grant("room grant permissions are invalid"));
    }

    let mut payload = Map::new();
    payload.insert("version".into(), Value::from(PROTOCOL_VERSION));
    payload.insert(
        "grant_id".into(),
        Value::from(identifier(&Value::from(grant_id), "grant_id")?),
    );
    payload.insert(
        "room_id".into(),
        Value::from(identifier(&Value::from(room_id), "room_id")?),
    );
    payload.insert(
        "home_install_id".into(),
        Value::from(identifier(
            &Value::from(home_install_id),
            "home_install_id",
        )?),
    );
    payload.insert(
        "authority_gateway_id".into(),
        Value::from(identifier(
            &Value::from(authority_gateway_id),
            "authority_gateway_id",
        )?),
    );
    payload.insert(
        "authority_epoch".into(),
        Value::from(positive_int_value(authority_epoch, "authority_epoch")?),
    );
    payload.insert(
        "member_id".into(),
        Value::from(identifier(&Value::from(member_id), "member_id")?),
    );
    payload.insert(
        "target_install_id".into(),
        Value::from(identifier(
            &Value::from(target_install_id),
            "target_install_id",
        )?),
    );
    payload.insert(
        "target_profile".into(),
        Value::from(identifier(&Value::from(target_profile), "target_profile")?),
    );
    payload.insert(
        "execution_policy_digest".into(),
        Value::from(digest_str(
            execution_policy_digest,
            "execution_policy_digest",
        )?),
    );
    payload.insert(
        "permissions".into(),
        Value::from(
            allowed
                .iter()
                .map(|p| Value::from(p.clone()))
                .collect::<Vec<_>>(),
        ),
    );
    payload.insert("issued_at".into(), float_value(now));
    payload.insert("expires_at".into(), float_value(now + ttl_seconds));
    payload.insert(
        "status_expires_at".into(),
        float_value(bounded_status_expiry),
    );

    let encoded = canonical_json(&Value::Object(payload));
    let signature = hmac_sha256(secret, &encoded);
    let token = format!("{}.{}", b64encode(&encoded), b64encode(&signature));
    if token.len() > MAX_TOKEN_BYTES {
        return Err(grant("room grant is too large"));
    }
    Ok(token)
}

/// Build a JSON float Number. Callers only pass finite values here.
fn float_value(x: f64) -> Value {
    Value::Number(Number::from_f64(x).unwrap_or_else(|| Number::from(0i64)))
}

/// Port of `verify_room_grant`: verify one grant against exact dispatch
/// coordinates, returning the decoded payload.
pub fn verify_room_grant(
    secret: &[u8],
    token: &str,
    dispatch: &HostedMemberDispatch,
    permission: &str,
    now: Option<f64>,
) -> PeerResult<Value> {
    let payload = decode_room_grant(secret, token, permission, now)?;

    let version_matches = payload
        .get("version")
        .and_then(|v| v.as_i64())
        .map(|v| v == dispatch.protocol_version)
        .unwrap_or(false);
    if !version_matches {
        return Err(grant("room grant protocol does not match dispatch"));
    }

    let expected: [(&str, Value); 8] = [
        ("room_id", Value::from(dispatch.room_id.clone())),
        (
            "home_install_id",
            Value::from(dispatch.home_install_id.clone()),
        ),
        (
            "authority_gateway_id",
            Value::from(dispatch.authority_gateway_id.clone()),
        ),
        ("authority_epoch", Value::from(dispatch.authority_epoch)),
        ("member_id", Value::from(dispatch.member_id.clone())),
        (
            "target_install_id",
            Value::from(dispatch.target_install_id.clone()),
        ),
        (
            "target_profile",
            Value::from(dispatch.target_profile.clone()),
        ),
        (
            "execution_policy_digest",
            Value::from(dispatch.execution_policy_digest.clone()),
        ),
    ];
    for (field, value) in &expected {
        if payload.get(*field) != Some(value) {
            return Err(grant("room grant scope does not match dispatch"));
        }
    }
    Ok(payload)
}

/// Port of `decode_room_grant`: verify signature, lifetime and operation.
pub fn decode_room_grant(
    secret: &[u8],
    token: &str,
    permission: &str,
    now: Option<f64>,
) -> PeerResult<Value> {
    if token.len() > MAX_TOKEN_BYTES {
        return Err(grant("room grant is invalid"));
    }
    let (encoded_token, signature_token) = match token.split_once('.') {
        Some(parts) => parts,
        None => return Err(grant("room grant is invalid")),
    };
    let encoded = b64decode(encoded_token)?;
    let supplied_signature = b64decode(signature_token)?;
    let expected_signature = hmac_sha256(secret, &encoded);
    if !constant_time_eq(&expected_signature, &supplied_signature) {
        return Err(grant("room grant signature is invalid"));
    }
    // Python does encoded.decode("ascii"); a non-ASCII byte raises and is
    // reported as an invalid payload.
    if !encoded.is_ascii() {
        return Err(grant("room grant payload is invalid"));
    }
    let text = match std::str::from_utf8(&encoded) {
        Ok(t) => t,
        Err(_) => return Err(grant("room grant payload is invalid")),
    };
    let payload: Value = match serde_json::from_str(text) {
        Ok(v) => v,
        Err(_) => return Err(grant("room grant payload is invalid")),
    };
    let object = match &payload {
        Value::Object(m) => m,
        _ => return Err(grant("room grant fields are invalid")),
    };
    let keys: HashSet<&str> = object.keys().map(String::as_str).collect();
    let base: HashSet<&str> = GRANT_FIELDS.iter().copied().collect();
    let mut refresh = base.clone();
    refresh.insert("status_expires_at");
    if keys != base && keys != refresh {
        return Err(grant("room grant fields are invalid"));
    }

    let checked_now = now.unwrap_or_else(now_seconds);
    if !checked_now.is_finite() {
        return Err(grant("room grant clock is invalid"));
    }

    let issued_at = object
        .get("issued_at")
        .and_then(py_float)
        .ok_or_else(|| grant("room grant lifetime is invalid"))?;
    let expires_at = object
        .get("expires_at")
        .and_then(py_float)
        .ok_or_else(|| grant("room grant lifetime is invalid"))?;
    let status_expires_at = match object.get("status_expires_at") {
        Some(v) => py_float(v).ok_or_else(|| grant("room grant lifetime is invalid"))?,
        None => expires_at,
    };
    if !(issued_at.is_finite()
        && expires_at.is_finite()
        && status_expires_at.is_finite()
        && issued_at < expires_at
        && expires_at <= status_expires_at)
    {
        return Err(grant("room grant lifetime is invalid"));
    }

    let operation_expires_at = if matches!(permission, "approve" | "status" | "stop") {
        status_expires_at
    } else {
        expires_at
    };
    if checked_now < issued_at - 30.0 || checked_now >= operation_expires_at {
        return Err(grant("room grant is expired or not active"));
    }

    let allows = match object.get("permissions") {
        Some(Value::Array(items)) => items
            .iter()
            .any(|item| matches!(item, Value::String(s) if s == permission)),
        _ => false,
    };
    if !allows {
        return Err(grant("room grant does not allow this operation"));
    }
    Ok(payload)
}

/// Port of `room_grant_needs_dispatch_refresh`: read-only timing check, returning
/// true on any parsing trouble (best-effort, never surfaces an error).
pub fn room_grant_needs_dispatch_refresh(
    token: &str,
    now: Option<f64>,
    leeway_seconds: f64,
) -> bool {
    let encoded_token = match token.split_once('.') {
        Some((head, _)) => head,
        None => return true,
    };
    let decoded = match b64decode(encoded_token) {
        Ok(d) => d,
        Err(_) => return true,
    };
    let text = match std::str::from_utf8(&decoded) {
        Ok(t) => t,
        Err(_) => return true,
    };
    let payload: Value = match serde_json::from_str(text) {
        Ok(v) => v,
        Err(_) => return true,
    };
    let expires_at = match payload.get("expires_at").and_then(py_float) {
        Some(v) => v,
        None => return true,
    };
    let checked_now = now.unwrap_or_else(now_seconds);
    checked_now + leeway_seconds.max(0.0) >= expires_at
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // The signed execution policy used across the catalog and dispatch fixtures.
    // Digest computed by running the real Python module.
    const POLICY_DIGEST: &str = "339c18cc8d1bbc7f8bf6e39a2d17bf7ab5456a2c971b6d8ba69cb71e5a0404ba";

    fn policy_mapping() -> Value {
        json!({
            "version": 1,
            "target_profile": "default",
            "enabled_toolsets": ["bot_room", "web"],
            "approval_mode": "manual",
            "max_iterations": 10,
            "policy_digest": POLICY_DIGEST,
        })
    }

    fn catalog_no_endpoint() -> Value {
        json!({
            "installation_id": "install-01",
            "protocol_versions": [2],
            "link_modes": ["direct"],
            "persistent_process": true,
            "text": true,
            "attachments": false,
            "execution_policy": policy_mapping(),
            "catalog_digest": "10bf7ebffd15902f9c7730fffbd3e67fa91266a52f0616fde364526947560aee",
        })
    }

    #[test]
    fn hmac_sha256_matches_rfc_reference() {
        // RFC 4231 test case 1: key = 0x0b*20, data = "Hi There".
        let key = [0x0bu8; 20];
        let mac = hmac_sha256(&key, b"Hi There");
        let hex: String = mac.iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(
            hex,
            "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7"
        );
    }

    #[test]
    fn catalog_canonical_json_and_digest_match_python() {
        // Golden canonical JSON + digest produced by the real Python module.
        let policy = RoomExecutionPolicy::from_mapping(&policy_mapping()).unwrap();
        let mut unsigned = Map::new();
        unsigned.insert("installation_id".into(), json!("install-01"));
        unsigned.insert("protocol_versions".into(), json!([2]));
        unsigned.insert("link_modes".into(), json!(["direct"]));
        unsigned.insert("persistent_process".into(), json!(true));
        unsigned.insert("text".into(), json!(true));
        unsigned.insert("attachments".into(), json!(false));
        unsigned.insert("execution_policy".into(), policy.as_mapping());
        let bytes = canonical_json(&Value::Object(unsigned));
        assert_eq!(
            std::str::from_utf8(&bytes).unwrap(),
            r#"{"attachments":false,"execution_policy":{"approval_mode":"manual","enabled_toolsets":["bot_room","web"],"max_iterations":10,"policy_digest":"339c18cc8d1bbc7f8bf6e39a2d17bf7ab5456a2c971b6d8ba69cb71e5a0404ba","target_profile":"default","version":1},"installation_id":"install-01","link_modes":["direct"],"persistent_process":true,"protocol_versions":[2],"text":true}"#
        );
        assert_eq!(
            sha256_hex(&bytes),
            "10bf7ebffd15902f9c7730fffbd3e67fa91266a52f0616fde364526947560aee"
        );
    }

    #[test]
    fn catalog_from_mapping_accepts_golden() {
        let cat = GatewayRoomCatalog::from_mapping(&catalog_no_endpoint()).unwrap();
        assert_eq!(cat.installation_id, "install-01");
        assert_eq!(cat.protocol_versions, vec![2]);
        assert_eq!(cat.link_modes, vec![LinkMode::Direct]);
        assert!(cat.persistent_process && cat.text && !cat.attachments);
        assert_eq!(cat.execution_policy.target_profile, "default");
        assert!(cat.endpoint_url.is_none());
    }

    #[test]
    fn catalog_with_endpoint_matches_python_digest() {
        let mut cat = catalog_no_endpoint();
        let obj = cat.as_object_mut().unwrap();
        obj.insert(
            "endpoint".into(),
            json!({
                "available": true,
                "url": "http://localhost:8080/rooms",
                "transport_security": "loopback",
            }),
        );
        obj.insert(
            "catalog_digest".into(),
            json!("793be9900ca6a321cbde58947f6dca676e660da2e38c96e24343f1ce689a8cff"),
        );
        let parsed = GatewayRoomCatalog::from_mapping(&cat).unwrap();
        assert_eq!(
            parsed.endpoint_url.as_deref(),
            Some("http://localhost:8080/rooms")
        );
        assert_eq!(parsed.transport_security, Some(TransportSecurity::Loopback));
    }

    #[test]
    fn catalog_rejects_tampered_digest() {
        let mut cat = catalog_no_endpoint();
        cat.as_object_mut()
            .unwrap()
            .insert("installation_id".into(), json!("other-install"));
        assert_eq!(
            GatewayRoomCatalog::from_mapping(&cat).unwrap_err(),
            peer("catalog_digest does not match the catalog")
        );
    }

    #[test]
    fn catalog_reports_missing_then_unknown_fields() {
        let mut cat = catalog_no_endpoint();
        cat.as_object_mut().unwrap().remove("text");
        assert_eq!(
            GatewayRoomCatalog::from_mapping(&cat).unwrap_err(),
            peer("capability catalog missing fields: text")
        );

        let mut cat = catalog_no_endpoint();
        cat.as_object_mut().unwrap().insert("junk".into(), json!(1));
        assert_eq!(
            GatewayRoomCatalog::from_mapping(&cat).unwrap_err(),
            peer("capability catalog unknown fields: junk")
        );
    }

    #[test]
    fn catalog_endpoint_transport_security_must_match_url() {
        let mut cat = catalog_no_endpoint();
        cat.as_object_mut().unwrap().insert(
            "endpoint".into(),
            json!({
                "available": true,
                "url": "http://localhost:8080/rooms",
                "transport_security": "tls",
            }),
        );
        assert_eq!(
            GatewayRoomCatalog::from_mapping(&cat).unwrap_err(),
            peer("endpoint transport_security does not match its URL")
        );
    }

    #[test]
    fn validate_url_classifies_transport() {
        let cases: &[(&str, Option<(&str, TransportSecurity)>)] = &[
            (
                "https://example.com/rooms",
                Some(("https://example.com/rooms", TransportSecurity::Tls)),
            ),
            (
                "https://example.com/rooms/",
                Some(("https://example.com/rooms", TransportSecurity::Tls)),
            ),
            (
                "https://Example.COM/Rooms",
                Some(("https://Example.COM/Rooms", TransportSecurity::Tls)),
            ),
            (
                "http://localhost:8080/rooms",
                Some(("http://localhost:8080/rooms", TransportSecurity::Loopback)),
            ),
            (
                "http://foo.localhost/x",
                Some(("http://foo.localhost/x", TransportSecurity::Loopback)),
            ),
            (
                "http://127.0.0.1:9000/x",
                Some(("http://127.0.0.1:9000/x", TransportSecurity::Loopback)),
            ),
            (
                "http://127.0.0.1./x",
                Some(("http://127.0.0.1./x", TransportSecurity::Loopback)),
            ),
            (
                "http://[::1]/x",
                Some(("http://[::1]/x", TransportSecurity::Loopback)),
            ),
            (
                "HTTP://localhost/x",
                Some(("HTTP://localhost/x", TransportSecurity::Loopback)),
            ),
            (
                "https://example.com",
                Some(("https://example.com", TransportSecurity::Tls)),
            ),
            ("http://example.com/x", None),
            ("http://10.0.0.1/x", None),
            ("https://user:pass@example.com/x", None),
            ("https://example.com/x?q=1", None),
            ("https://example.com/x#frag", None),
            ("ftp://example.com/x", None),
            ("https://example.com:99999/x", None),
            ("https://:8080/x", None),
            ("", None),
        ];
        for (input, expected) in cases {
            let got = validate_room_link_url(&json!(input));
            match expected {
                Some((url, ts)) => {
                    let (gu, gts) = got.unwrap_or_else(|e| panic!("{input}: {e}"));
                    assert_eq!(&gu, url, "url for {input}");
                    assert_eq!(&gts, ts, "ts for {input}");
                }
                None => assert!(got.is_err(), "{input} should be rejected"),
            }
        }
    }

    #[test]
    fn validate_url_error_messages() {
        assert_eq!(
            validate_room_link_url(&json!("http://example.com/x")).unwrap_err(),
            peer("target_url must use https outside the local machine")
        );
        assert_eq!(
            validate_room_link_url(&json!("ftp://example.com/x")).unwrap_err(),
            peer("target_url must use https")
        );
        assert_eq!(
            validate_room_link_url(&json!("https://example.com/x?q=1")).unwrap_err(),
            peer("target_url must not include query or fragment")
        );
        assert_eq!(
            validate_room_link_url(&json!("https://example.com:99999/x")).unwrap_err(),
            peer("target_url is invalid")
        );
        assert_eq!(
            validate_room_link_url(&json!("https://user:pass@example.com/x")).unwrap_err(),
            peer("target_url is invalid")
        );
    }

    fn dispatch_fixture() -> HostedMemberDispatch {
        HostedMemberDispatch::from_mapping(&json!({
            "protocol_version": 2,
            "room_id": "room-01",
            "home_install_id": "home-01",
            "authority_gateway_id": "auth-01",
            "authority_epoch": 1,
            "member_id": "member-01",
            "target_install_id": "install-01",
            "target_profile": "default",
            "task_id": "task-01",
            "execution_generation": 1,
            "source_event_seq": 1,
            "cancellation_scope_id": "cancel-01",
            "prompt": "hello room",
            "prompt_digest": "7050f166dc608e9d7e435dfcd36e2fdda843e5fafe50f7aed9556fb93483e23c",
            "capability_digest": "10bf7ebffd15902f9c7730fffbd3e67fa91266a52f0616fde364526947560aee",
            "execution_policy_digest": POLICY_DIGEST,
            "trace_id": "trace-01",
        }))
        .unwrap()
    }

    #[test]
    fn dispatch_prompt_digest_is_checked() {
        assert_eq!(dispatch_fixture().prompt, "hello room");
        assert_eq!(
            sha256_hex(b"hello room"),
            "7050f166dc608e9d7e435dfcd36e2fdda843e5fafe50f7aed9556fb93483e23c"
        );

        let mut bad = dispatch_fixture().as_mapping();
        bad.as_object_mut()
            .unwrap()
            .insert("prompt".into(), json!("tampered"));
        assert_eq!(
            HostedMemberDispatch::from_mapping(&bad).unwrap_err(),
            peer("prompt_digest does not match prompt")
        );
    }

    // 32-byte fixed secret used for the grant golden vectors.
    const GRANT_SECRET: &[u8; 32] = b"0123456789abcdef0123456789abcdef";
    const GRANT_TOKEN: &str = "eyJhdXRob3JpdHlfZXBvY2giOjEsImF1dGhvcml0eV9nYXRld2F5X2lkIjoiYXV0aC0wMSIsImV4ZWN1dGlvbl9wb2xpY3lfZGlnZXN0IjoiMzM5YzE4Y2M4ZDFiYmM3ZjhiZjZlMzlhMmQxN2JmN2FiNTQ1NmEyYzk3MWI2ZDhiYTY5Y2I3MWU1YTA0MDRiYSIsImV4cGlyZXNfYXQiOjE3MDAwMDM2MDAuMCwiZ3JhbnRfaWQiOiJncmFudC0wMSIsImhvbWVfaW5zdGFsbF9pZCI6ImhvbWUtMDEiLCJpc3N1ZWRfYXQiOjE3MDAwMDAwMDAuMCwibWVtYmVyX2lkIjoibWVtYmVyLTAxIiwicGVybWlzc2lvbnMiOlsiYXBwcm92ZSIsImRpc3BhdGNoIiwic3RhdHVzIiwic3RvcCJdLCJyb29tX2lkIjoicm9vbS0wMSIsInN0YXR1c19leHBpcmVzX2F0IjoxNzAwMDAzNjAwLjAsInRhcmdldF9pbnN0YWxsX2lkIjoiaW5zdGFsbC0wMSIsInRhcmdldF9wcm9maWxlIjoiZGVmYXVsdCIsInZlcnNpb24iOjJ9.G0wnzXgTI6-IXp5eznDFByWP11h9uB4lY-MNlD1h7xI";

    fn issue_golden() -> String {
        issue_room_grant(
            GRANT_SECRET,
            "grant-01",
            "room-01",
            "home-01",
            "auth-01",
            1,
            "member-01",
            "install-01",
            "default",
            POLICY_DIGEST,
            &["approve", "dispatch", "status", "stop"],
            Some(1700000000.0),
            3600.0,
            None,
            None,
        )
        .unwrap()
    }

    #[test]
    fn issue_room_grant_matches_python_token() {
        assert_eq!(issue_golden(), GRANT_TOKEN);
    }

    #[test]
    fn grant_payload_canonical_json_matches_python() {
        let (encoded, _) = GRANT_TOKEN.split_once('.').unwrap();
        let bytes = b64decode(encoded).unwrap();
        assert_eq!(
            std::str::from_utf8(&bytes).unwrap(),
            r#"{"authority_epoch":1,"authority_gateway_id":"auth-01","execution_policy_digest":"339c18cc8d1bbc7f8bf6e39a2d17bf7ab5456a2c971b6d8ba69cb71e5a0404ba","expires_at":1700003600.0,"grant_id":"grant-01","home_install_id":"home-01","issued_at":1700000000.0,"member_id":"member-01","permissions":["approve","dispatch","status","stop"],"room_id":"room-01","status_expires_at":1700003600.0,"target_install_id":"install-01","target_profile":"default","version":2}"#
        );
    }

    #[test]
    fn verify_room_grant_accepts_matching_dispatch() {
        let payload = verify_room_grant(
            GRANT_SECRET,
            GRANT_TOKEN,
            &dispatch_fixture(),
            "dispatch",
            Some(1700001000.0),
        )
        .unwrap();
        assert_eq!(payload["grant_id"], json!("grant-01"));
        assert_eq!(
            payload["permissions"],
            json!(["approve", "dispatch", "status", "stop"])
        );
    }

    #[test]
    fn verify_room_grant_rejects_scope_mismatch() {
        let mut dispatch = dispatch_fixture();
        dispatch.room_id = "room-99".to_string();
        assert_eq!(
            verify_room_grant(
                GRANT_SECRET,
                GRANT_TOKEN,
                &dispatch,
                "dispatch",
                Some(1700001000.0),
            )
            .unwrap_err(),
            grant("room grant scope does not match dispatch")
        );
    }

    #[test]
    fn decode_rejects_expired_and_tampered() {
        // Past the dispatch expiry.
        assert_eq!(
            decode_room_grant(GRANT_SECRET, GRANT_TOKEN, "dispatch", Some(1700003600.0))
                .unwrap_err(),
            grant("room grant is expired or not active")
        );
        // A different secret fails the signature check.
        assert_eq!(
            decode_room_grant(
                b"XXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXX",
                GRANT_TOKEN,
                "dispatch",
                Some(1700001000.0)
            )
            .unwrap_err(),
            grant("room grant signature is invalid")
        );
        // No separator.
        assert_eq!(
            decode_room_grant(GRANT_SECRET, "no-dot-token", "dispatch", None).unwrap_err(),
            grant("room grant is invalid")
        );
    }

    #[test]
    fn status_permission_uses_status_expiry() {
        // "status" is allowed and, since dispatch and status expiry are equal
        // here, it still rejects just past expiry but accepts before it.
        assert!(decode_room_grant(GRANT_SECRET, GRANT_TOKEN, "status", Some(1700002000.0)).is_ok());
        // An operation not in the permission list is rejected. This token grants
        // all four, so craft a narrower one.
        let narrow = issue_room_grant(
            GRANT_SECRET,
            "grant-02",
            "room-01",
            "home-01",
            "auth-01",
            1,
            "member-01",
            "install-01",
            "default",
            POLICY_DIGEST,
            &["status"],
            Some(1700000000.0),
            3600.0,
            None,
            None,
        )
        .unwrap();
        assert_eq!(
            decode_room_grant(GRANT_SECRET, &narrow, "dispatch", Some(1700001000.0)).unwrap_err(),
            grant("room grant does not allow this operation")
        );
    }

    #[test]
    fn issue_rejects_bad_lifetime_and_permissions() {
        // TTL over the dispatch cap.
        assert_eq!(
            issue_room_grant(
                GRANT_SECRET,
                "g",
                "r",
                "h",
                "a",
                1,
                "m",
                "i",
                "default",
                POLICY_DIGEST,
                &["dispatch"],
                Some(1700000000.0),
                MAX_DISPATCH_GRANT_TTL_SECONDS + 1.0,
                None,
                None,
            )
            .unwrap_err(),
            grant("room grant lifetime is invalid")
        );
        // Unknown permission.
        assert_eq!(
            issue_room_grant(
                GRANT_SECRET,
                "g",
                "r",
                "h",
                "a",
                1,
                "m",
                "i",
                "default",
                POLICY_DIGEST,
                &["superuser"],
                Some(1700000000.0),
                3600.0,
                None,
                None,
            )
            .unwrap_err(),
            grant("room grant permissions are invalid")
        );
        // Short secret.
        assert_eq!(
            issue_room_grant(
                b"short",
                "g",
                "r",
                "h",
                "a",
                1,
                "m",
                "i",
                "default",
                POLICY_DIGEST,
                &["dispatch"],
                Some(1700000000.0),
                3600.0,
                None,
                None,
            )
            .unwrap_err(),
            grant("room grant secret must be at least 32 bytes")
        );
    }

    #[test]
    fn derive_room_grant_secret_matches_python() {
        let secret = derive_room_grant_secret("supersecretkey").unwrap();
        let hex: String = secret.iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(
            hex,
            "097cc79bce5de264f06150577b17636b5c1c8fa0bbecf545a2be3acf06a54119"
        );
        assert_eq!(
            derive_room_grant_secret("short").unwrap_err(),
            grant("room grants require a strong gateway API key")
        );
    }

    #[test]
    fn refresh_timing_matches_python() {
        // expires_at = 1700003600; leeway default 300s.
        assert!(room_grant_needs_dispatch_refresh(
            GRANT_TOKEN,
            Some(1700003400.0),
            5.0 * 60.0
        ));
        assert!(!room_grant_needs_dispatch_refresh(
            GRANT_TOKEN,
            Some(1700000000.0),
            5.0 * 60.0
        ));
        // Malformed token: best-effort true.
        assert!(room_grant_needs_dispatch_refresh("garbage", None, 300.0));
    }

    #[test]
    fn select_room_link_prefers_priority_then_latency() {
        let probes = vec![
            RoomLinkProbe {
                mode: LinkMode::Pull,
                verified: true,
                encrypted: true,
                latency_ms: 1.0,
            },
            RoomLinkProbe {
                mode: LinkMode::Direct,
                verified: true,
                encrypted: true,
                latency_ms: 50.0,
            },
            RoomLinkProbe {
                mode: LinkMode::Direct,
                verified: true,
                encrypted: true,
                latency_ms: 20.0,
            },
            // Unencrypted and unverified are excluded.
            RoomLinkProbe {
                mode: LinkMode::Direct,
                verified: true,
                encrypted: false,
                latency_ms: 1.0,
            },
        ];
        let chosen = select_room_link(&probes, false).unwrap();
        assert_eq!(chosen.mode, LinkMode::Direct);
        assert_eq!(chosen.latency_ms, 20.0);

        // No viable candidate, desktop available.
        let none = vec![RoomLinkProbe {
            mode: LinkMode::Direct,
            verified: false,
            encrypted: true,
            latency_ms: 1.0,
        }];
        let desktop = select_room_link(&none, true).unwrap();
        assert_eq!(desktop.mode, LinkMode::Desktop);

        // Nothing at all.
        assert!(select_room_link(&none, false).is_none());
    }

    #[test]
    fn b64_roundtrips() {
        let data = b"\x00\x01\x02\xff\xfe some bytes";
        let encoded = b64encode(data);
        assert!(!encoded.contains('='));
        assert_eq!(b64decode(&encoded).unwrap(), data);
    }
}
