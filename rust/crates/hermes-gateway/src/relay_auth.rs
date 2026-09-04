//! Port of gateway/relay/auth.py.
//!
// Public API is ahead of its callers (wired later).
#![allow(dead_code)]
//! Gateway-side relay authentication primitives. EXPERIMENTAL.
//!
//! The connector<->gateway channel is authenticated because a gateway may be
//! customer-managed and internet-exposed. This module is the gateway half of two
//! HMAC schemes whose wire bytes must match the connector's TypeScript exactly:
//!
//! 1. WS upgrade auth (gateway -> connector): the gateway presents
//!    `Authorization: Bearer <token>` on the `/relay` WebSocket upgrade, where
//!    `token = make_upgrade_token(gateway_id, secret)`. The token is
//!    `base64url(f"{payload}:{exp}:{sig}")` with
//!    `sig = HMAC_SHA256(f"{payload}:{exp}", secret).hexdigest()` and
//!    `payload == gateway_id`.
//!
//! 2. Inbound delivery signature (connector -> gateway): the connector signs each
//!    inbound POST with the per-tenant delivery key, carried as
//!    `x-relay-timestamp` + `x-relay-signature` headers; the gateway verifies
//!    before accepting the event.
//!    `sig = HMAC_SHA256(f"{ts}.{body_json}", key).hexdigest()` over the exact
//!    request body bytes, with a replay-window skew check.
//!
//! Both schemes use a multi-secret verify list (primary first, then a secondary
//! during a rotation window) so a secret rotation does not invalidate outstanding
//! tokens.
//!
//! This is self-contained: the Python module had no internal imports. HMAC-SHA256
//! is implemented by hand over sha2 (same idiom as `hosted_room_peer.rs`), the
//! signature encoding is lowercase hex, and the token body uses URL-safe base64
//! without padding (Node's `Buffer.toString("base64url")`).

use std::time::{SystemTime, UNIX_EPOCH};

use sha2::{Digest, Sha256};

/// Header names the connector uses for inbound delivery signatures.
pub const DELIVERY_TS_HEADER: &str = "x-relay-timestamp";
pub const DELIVERY_SIG_HEADER: &str = "x-relay-signature";

/// Default replay window for an inbound delivery signature (connector default).
pub const DEFAULT_MAX_SKEW_SECONDS: i64 = 300;
/// Default TTL for an upgrade token (connector `makeUpgradeToken` default).
pub const DEFAULT_UPGRADE_TTL_SECONDS: i64 = 300;

// ---------------------------------------------------------------------------
// Crypto helpers (hand-rolled HMAC-SHA256, constant-time compare)
// ---------------------------------------------------------------------------

fn sha256(data: &[u8]) -> [u8; 32] {
    Sha256::digest(data).into()
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

fn hex_lower(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

/// Constant-time equality, mirroring `hmac.compare_digest`. Length mismatch
/// returns false immediately (Python only constant-time compares equal lengths;
/// the caller has already filtered unequal lengths out).
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

/// Port of `_hmac_hex`: HMAC-SHA256 hex digest of `payload` under `secret`, both
/// encoded as UTF-8.
fn hmac_hex(payload: &str, secret: &str) -> String {
    hex_lower(&hmac_sha256(secret.as_bytes(), payload.as_bytes()))
}

/// HMAC-SHA256 hex digest, mirroring the connector's `sign`.
pub fn sign(payload: &str, secret: &str) -> String {
    hmac_hex(payload, secret)
}

// ---------------------------------------------------------------------------
// Parsing helpers that mirror Python builtins
// ---------------------------------------------------------------------------

fn is_ascii_hex_ws(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c)
}

fn hex_nibble(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// Port of `bytes.fromhex`: two hex digits per byte, ASCII whitespace allowed
/// only at byte boundaries (never between the two nibbles of one byte). Returns
/// None where Python raises ValueError (odd digit count or a stray character).
fn from_hex(s: &str) -> Option<Vec<u8>> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len() / 2);
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        if is_ascii_hex_ws(c) {
            i += 1;
            continue;
        }
        let hi = hex_nibble(c)?;
        i += 1;
        if i >= bytes.len() {
            return None;
        }
        let lo = hex_nibble(bytes[i])?;
        i += 1;
        out.push((hi << 4) | lo);
    }
    Some(out)
}

/// Port of Python `int(str)` for base 10: surrounding whitespace is stripped, an
/// optional single `+`/`-` sign is allowed, and single underscores may separate
/// digits. Returns None where Python raises ValueError (and on i64 overflow,
/// which real tokens never reach).
fn py_int(s: &str) -> Option<i64> {
    let trimmed = s.trim();
    let (negative, digits_part) = if let Some(rest) = trimmed.strip_prefix('-') {
        (true, rest)
    } else if let Some(rest) = trimmed.strip_prefix('+') {
        (false, rest)
    } else {
        (false, trimmed)
    };
    if digits_part.is_empty() {
        return None;
    }
    let raw = digits_part.as_bytes();
    let mut digits = String::with_capacity(raw.len());
    let mut prev_underscore = false;
    for (idx, &c) in raw.iter().enumerate() {
        if c == b'_' {
            // Underscore cannot lead, trail, or repeat; it must sit between digits.
            if idx == 0 || prev_underscore {
                return None;
            }
            prev_underscore = true;
            continue;
        }
        if !c.is_ascii_digit() {
            return None;
        }
        digits.push(c as char);
        prev_underscore = false;
    }
    if prev_underscore || digits.is_empty() {
        return None;
    }
    let value: i64 = digits.parse().ok()?;
    Some(if negative { -value } else { value })
}

// ---------------------------------------------------------------------------
// URL-safe base64 (encode without padding; lenient decode like Python)
// ---------------------------------------------------------------------------

const B64_ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

/// `base64.urlsafe_b64encode(value).rstrip(b"=")` (matches Node base64url).
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

/// Decode the way `verify_token` does. Python restores padding then calls
/// `urlsafe_b64decode`, which translates `-_` to `+/` before a non-strict
/// standard decode. Non-strict decode discards any character outside the
/// alphabet (including the restored `=` padding) and both `-`/`+` map to 62 and
/// `_`/`/` map to 63. A trailing group of a single sextet cannot form a byte, so
/// Python raises there; we return None to match. Padding never changes the
/// output, so decoding the raw token directly is equivalent.
fn b64_urlsafe_decode(value: &str) -> Option<Vec<u8>> {
    let mut sextets: Vec<u8> = Vec::with_capacity(value.len());
    for c in value.bytes() {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'-' | b'+' => 62,
            b'_' | b'/' => 63,
            _ => continue,
        };
        sextets.push(v);
    }
    if sextets.len() % 4 == 1 {
        return None;
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
    Some(out)
}

// ---------------------------------------------------------------------------
// Clock
// ---------------------------------------------------------------------------

/// `int(time.time())`: unix seconds, truncated toward zero (floor for positive).
fn now_seconds() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Signature verify / token mint + verify / delivery verify
// ---------------------------------------------------------------------------

/// Port of `verify_signature`: constant-time check that `sig_hex` is a valid
/// HMAC of `payload` under ANY of `secrets` (rotation window). A sig that does
/// not parse as hex, is empty, or is not 32 bytes fails; empty secrets are
/// skipped. Mirrors the connector's `verifySignature`.
pub fn verify_signature(payload: &str, sig_hex: &str, secrets: &[&str]) -> bool {
    let sig_buf = match from_hex(sig_hex) {
        Some(buf) => buf,
        None => return false,
    };
    if sig_buf.is_empty() {
        return false;
    }
    for secret in secrets {
        if secret.is_empty() {
            continue;
        }
        // Python computes bytes.fromhex(_hmac_hex(...)), which is always the raw
        // 32-byte digest; length-mismatched candidates are skipped.
        let expected = hmac_sha256(secret.as_bytes(), payload.as_bytes());
        if expected.len() != sig_buf.len() {
            continue;
        }
        if constant_time_eq(&sig_buf, &expected) {
            return true;
        }
    }
    false
}

/// Port of `make_token`: build a signed, optionally-expiring token,
/// `base64url(f"{payload}:{exp}:{sig}")` where `exp` is a unix-seconds expiry
/// (0 = never) and `sig = HMAC_SHA256(f"{payload}:{exp}", secret)`. base64url is
/// unpadded to match Node's `Buffer.toString("base64url")`.
pub fn make_token(payload: &str, secret: &str, ttl_seconds: i64) -> String {
    let exp = if ttl_seconds > 0 {
        now_seconds() + ttl_seconds
    } else {
        0
    };
    let signed = format!("{payload}:{exp}");
    let sig = hmac_hex(&signed, secret);
    let raw = format!("{signed}:{sig}");
    b64encode(raw.as_bytes())
}

/// Port of `make_upgrade_token`: the WS-upgrade bearer token a gateway sends,
/// with `payload = gateway_id`. Pass [`DEFAULT_UPGRADE_TTL_SECONDS`] for the
/// Python default TTL.
pub fn make_upgrade_token(gateway_id: &str, secret: &str, ttl_seconds: i64) -> String {
    make_token(gateway_id, secret, ttl_seconds)
}

/// Port of `verify_token`: verify a token built by [`make_token`]; return the
/// payload or None. Splits from the right so a payload may itself contain colons.
/// Rejects an expired token and any signature that does not match a secret in the
/// verify list.
pub fn verify_token(token: &str, secrets: &[&str]) -> Option<String> {
    let decoded_bytes = b64_urlsafe_decode(token)?;
    let decoded = std::str::from_utf8(&decoded_bytes).ok()?;
    let parts: Vec<&str> = decoded.split(':').collect();
    if parts.len() < 3 {
        return None;
    }
    let sig = parts[parts.len() - 1];
    let exp = py_int(parts[parts.len() - 2])?;
    let payload = parts[..parts.len() - 2].join(":");
    if exp != 0 && now_seconds() > exp {
        return None;
    }
    let signed = format!("{payload}:{exp}");
    if verify_signature(&signed, sig, secrets) {
        Some(payload)
    } else {
        None
    }
}

/// Port of `_delivery_payload`: the signed material for an inbound delivery,
/// `f"{ts}.{body_json}"`.
fn delivery_payload(ts: i64, body_json: &str) -> String {
    format!("{ts}.{body_json}")
}

/// Port of `verify_delivery_signature`: verify a connector->gateway inbound
/// delivery signature.
///
/// `body_json` MUST be the exact request body bytes decoded as UTF-8, since the
/// connector signs over the literal serialized body (no re-serialization). Checks
/// the timestamp is within `max_skew_seconds` of now and the HMAC matches any key
/// in the rotation verify list. A None or empty timestamp/signature fails.
/// `now` overrides the clock (the Python keyword-only `now`); pass
/// [`DEFAULT_MAX_SKEW_SECONDS`] for the Python default skew window.
pub fn verify_delivery_signature(
    body_json: &str,
    timestamp: Option<&str>,
    signature: Option<&str>,
    verify_keys: &[&str],
    max_skew_seconds: i64,
    now: Option<i64>,
) -> bool {
    // `if not timestamp or not signature`: None or the empty string is falsy.
    let timestamp = match timestamp {
        Some(t) if !t.is_empty() => t,
        _ => return false,
    };
    let signature = match signature {
        Some(s) if !s.is_empty() => s,
        _ => return false,
    };
    let ts = match py_int(timestamp) {
        Some(v) => v,
        None => return false,
    };
    let current = now.unwrap_or_else(now_seconds);
    if (current as i128 - ts as i128).abs() > max_skew_seconds as i128 {
        return false;
    }
    verify_signature(&delivery_payload(ts, body_json), signature, verify_keys)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECRET: &str = "s3cr3t-secret";

    #[test]
    fn hmac_sha256_matches_rfc_reference() {
        // RFC 4231 test case 1: key = 0x0b*20, data = "Hi There".
        let key = [0x0bu8; 20];
        let mac = hex_lower(&hmac_sha256(&key, b"Hi There"));
        assert_eq!(
            mac,
            "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7"
        );
    }

    // Golden vectors below were produced by running the real Python module,
    // `gateway/relay/auth.py`, on this repo (Python 3.14).

    #[test]
    fn sign_matches_python() {
        assert_eq!(
            sign("gateway-123:0", SECRET),
            "d9b5d344e632b3142b39d1a5db66068794b6ddf413647766f73016e45c810c47"
        );
    }

    #[test]
    fn make_token_matches_python_golden() {
        // ttl=0 => exp=0 (never expires), so the token bytes are deterministic.
        assert_eq!(
            make_token("gateway-123", SECRET, 0),
            "Z2F0ZXdheS0xMjM6MDpkOWI1ZDM0NGU2MzJiMzE0MmIzOWQxYTVkYjY2MDY4Nzk0YjZkZGY0MTM2NDc3NjZmNzMwMTZlNDVjODEwYzQ3"
        );
        // make_upgrade_token is make_token with payload = gateway_id.
        assert_eq!(
            make_upgrade_token("gateway-123", SECRET, 0),
            make_token("gateway-123", SECRET, 0)
        );
    }

    #[test]
    fn verify_token_roundtrip_and_rotation() {
        let token = "Z2F0ZXdheS0xMjM6MDpkOWI1ZDM0NGU2MzJiMzE0MmIzOWQxYTVkYjY2MDY4Nzk0YjZkZGY0MTM2NDc3NjZmNzMwMTZlNDVjODEwYzQ3";
        assert_eq!(
            verify_token(token, &[SECRET]),
            Some("gateway-123".to_string())
        );
        // Wrong secret only -> rejected.
        assert_eq!(verify_token(token, &["nope"]), None);
        // Rotation window: secondary secret still verifies.
        assert_eq!(
            verify_token(token, &["nope", SECRET]),
            Some("gateway-123".to_string())
        );
    }

    #[test]
    fn verify_token_payload_with_colons() {
        // Right-split keeps a colon-containing payload intact.
        let token = "YTpiOmM6MDowMzc3ZjczYjdkZDRmOGQ5NGU3ZWQ0OGI4YTcyNjUyNzg2ZDc1YTIzNzg5ZGQ2ZjgyZTI4NmU2M2Y5NGYyNWNj";
        assert_eq!(verify_token(token, &[SECRET]), Some("a:b:c".to_string()));
    }

    #[test]
    fn verify_token_empty_payload_returns_empty_string() {
        // An empty payload verifies and returns "" (not None).
        let token = "OjA6YzI2MDFiMWMyMzBmNGQ0MGUwYjVhODViMTNkMjU0NWNiNDM5OWYzZGEwMTg5NmY2Y2YzOGQwMjczZmM1ZmI3Mw";
        assert_eq!(verify_token(token, &[SECRET]), Some(String::new()));
    }

    #[test]
    fn verify_token_expired_returns_none() {
        // Token with exp=100 (in the past), so it is rejected.
        let token = "Z3c6MTAwOmNiOWE2ZDJmMDU2NjEzYTllYjZjMzYyYmQwYjMxZTczNDJlNjZiMTM1MDcxNWQxMDdmNGNmNjI0MzZjNDE3NzA";
        assert_eq!(verify_token(token, &[SECRET]), None);
    }

    #[test]
    fn make_verify_roundtrip_self_consistent() {
        // A live TTL token round-trips through verify against the current clock.
        let token = make_token("gw:with:colons", SECRET, 300);
        assert_eq!(
            verify_token(&token, &[SECRET]),
            Some("gw:with:colons".to_string())
        );
        assert_eq!(verify_token(&token, &["other"]), None);
    }

    #[test]
    fn verify_signature_edge_cases() {
        let good = sign("gateway-123:0", SECRET);
        assert!(verify_signature("gateway-123:0", &good, &[SECRET]));
        // Empty signature -> false.
        assert!(!verify_signature("x", "", &[SECRET]));
        // Odd hex digit count -> ValueError in Python -> false.
        assert!(!verify_signature("x", "abc", &[SECRET]));
        // Valid hex but wrong length (2 bytes) -> length mismatch -> false.
        assert!(!verify_signature("gateway-123:0", "0b0b", &[SECRET]));
        // Empty secret is skipped; the following good secret still verifies.
        assert!(verify_signature("gateway-123:0", &good, &["", SECRET]));
    }

    #[test]
    fn verify_delivery_signature_golden() {
        let body = r#"{"a":1}"#;
        let ts = 1_700_000_000i64;
        let key = "deliverykey";
        let sig = "ef089920fb6ec458621c4819064589ddc0cd719ca5598da680108a2fdfd3b027";
        // sign matches the golden.
        assert_eq!(sign(&format!("{ts}.{body}"), key), sig);
        // Within skew (now == ts) -> valid.
        assert!(verify_delivery_signature(
            body,
            Some(&ts.to_string()),
            Some(sig),
            &[key],
            DEFAULT_MAX_SKEW_SECONDS,
            Some(ts)
        ));
        // Exactly at the edge (skew == 300) -> still valid.
        assert!(verify_delivery_signature(
            body,
            Some(&ts.to_string()),
            Some(sig),
            &[key],
            DEFAULT_MAX_SKEW_SECONDS,
            Some(ts + 300)
        ));
        // Beyond the window (skew == 301) -> rejected.
        assert!(!verify_delivery_signature(
            body,
            Some(&ts.to_string()),
            Some(sig),
            &[key],
            DEFAULT_MAX_SKEW_SECONDS,
            Some(ts + 301)
        ));
        // Missing/empty headers -> rejected.
        assert!(!verify_delivery_signature(
            body,
            None,
            Some(sig),
            &[key],
            DEFAULT_MAX_SKEW_SECONDS,
            Some(ts)
        ));
        assert!(!verify_delivery_signature(
            body,
            Some(""),
            Some(sig),
            &[key],
            DEFAULT_MAX_SKEW_SECONDS,
            Some(ts)
        ));
        // Non-integer timestamp -> rejected.
        assert!(!verify_delivery_signature(
            body,
            Some("not-a-number"),
            Some(sig),
            &[key],
            DEFAULT_MAX_SKEW_SECONDS,
            Some(ts)
        ));
    }

    #[test]
    fn py_int_matches_python_semantics() {
        assert_eq!(py_int(" 5 "), Some(5));
        assert_eq!(py_int("+5"), Some(5));
        assert_eq!(py_int("-5"), Some(-5));
        assert_eq!(py_int("1_000"), Some(1000));
        assert_eq!(py_int("0x5"), None);
        assert_eq!(py_int(""), None);
        assert_eq!(py_int("5.0"), None);
        assert_eq!(py_int("_5"), None);
        assert_eq!(py_int("5_"), None);
        assert_eq!(py_int("1__0"), None);
    }

    #[test]
    fn from_hex_whitespace_rules() {
        // Whitespace allowed at byte boundaries, not within a byte.
        assert_eq!(from_hex("  0b0b "), Some(vec![0x0b, 0x0b]));
        assert_eq!(from_hex("0b 0b"), Some(vec![0x0b, 0x0b]));
        assert_eq!(from_hex("0 b0b"), None);
        assert_eq!(from_hex("0b0b0"), None);
        assert_eq!(from_hex("zz"), None);
    }
}
