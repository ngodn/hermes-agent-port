//! Port of the self-contained authorization primitives of gateway/authz_mixin.py.
//!
// Public API is ahead of its callers (the runner authz path wires it).
#![allow(dead_code)]
//!
//! The inbound-message authorization building blocks that don't need the
//! GatewayRunner: allowlist parsing, per-profile-isolated gate-env reads, and
//! Nostr `npub` -> hex normalization (so an operator who lists their `npub1…`
//! authorizes the same identity as the inbound hex pubkey, #78428). The full
//! `_is_user_authorized` decision hangs off the adapter registry / pairing
//! store and lands with the runner.

use std::collections::HashSet;

use serde_json::Value;

/// Read an allow/deny gate env var. The per-profile secret-scope isolation (a
/// scoped miss under multiplex returning the default instead of falling through
/// to the process env) lands with the secret-scope subsystem; single-profile
/// deployments behave exactly as this `getenv`-with-default read.
pub fn platform_gate_env(name: &str, default: &str) -> String {
    if name.is_empty() {
        return default.to_string();
    }
    match std::env::var(name) {
        Ok(v) if !v.is_empty() => v.trim().to_string(),
        _ => default.trim().to_string(),
    }
}

/// Alias of [`platform_gate_env`] (same rules) for allowlist/auth env reads.
pub fn auth_env(name: &str, default: &str) -> String {
    platform_gate_env(name, default)
}

/// Parse an allowlist value (a YAML sequence or a comma-separated scalar/env
/// string) into a set of trimmed non-empty strings.
pub fn coerce_allow_set(raw: Option<&Value>) -> HashSet<String> {
    let mut out = HashSet::new();
    match raw {
        None | Some(Value::Null) => {}
        Some(Value::Array(items)) => {
            for part in items {
                let s = value_to_str(part);
                let s = s.trim();
                if !s.is_empty() {
                    out.insert(s.to_string());
                }
            }
        }
        Some(other) => {
            let s = value_to_str(other);
            for part in s.split(',') {
                let part = part.trim();
                if !part.is_empty() {
                    out.insert(part.to_string());
                }
            }
        }
    }
    out
}

fn value_to_str(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        Value::Bool(b) => {
            if *b {
                "True".into()
            } else {
                "False".into()
            }
        }
        other => other.to_string(),
    }
}

// ── Nostr npub -> hex (bech32) ───────────────────────────────────────────────

const BECH32_CHARSET: &str = "qpzry9x8gf2tvdw0s3jn54khce6mua7l";

fn bech32_polymod(values: &[u32]) -> u32 {
    let generator = [
        0x3B6A57B2u32,
        0x26508E6D,
        0x1EA119FA,
        0x3D4233DD,
        0x2A1462B3,
    ];
    let mut chk: u32 = 1;
    for &value in values {
        let top = chk >> 25;
        chk = (chk & 0x1FF_FFFF) << 5 ^ value;
        for (i, g) in generator.iter().enumerate() {
            if (top >> i) & 1 != 0 {
                chk ^= *g;
            }
        }
    }
    chk
}

fn bech32_hrp_expand(hrp: &str) -> Vec<u32> {
    let mut out: Vec<u32> = hrp.bytes().map(|c| (c >> 5) as u32).collect();
    out.push(0);
    out.extend(hrp.bytes().map(|c| (c & 31) as u32));
    out
}

/// Regroup `data` from `frombits`-bit groups to `tobits`-bit groups. Returns
/// `None` on an invalid value or (without `pad`) leftover bits.
fn convertbits(data: &[u32], frombits: u32, tobits: u32, pad: bool) -> Option<Vec<u32>> {
    let mut acc: u32 = 0;
    let mut bits: u32 = 0;
    let mut ret = Vec::new();
    let maxv = (1u32 << tobits) - 1;
    for &value in data {
        if (value >> frombits) != 0 {
            return None;
        }
        acc = (acc << frombits) | value;
        bits += frombits;
        while bits >= tobits {
            bits -= tobits;
            ret.push((acc >> bits) & maxv);
        }
    }
    if pad {
        if bits > 0 {
            ret.push((acc << (tobits - bits)) & maxv);
        }
    } else if bits >= frombits || ((acc << (tobits - bits)) & maxv) != 0 {
        return None;
    }
    Some(ret)
}

/// Decode an `npub1…` bech32 string to a 64-char hex pubkey, else `None`.
pub fn npub_to_hex(npub: &str) -> Option<String> {
    let npub = npub.trim().to_lowercase();
    let data_part = npub.strip_prefix("npub1")?;
    let data: Option<Vec<u32>> = data_part
        .chars()
        .map(|c| BECH32_CHARSET.find(c).map(|i| i as u32))
        .collect();
    let data = data?;
    let mut checked = bech32_hrp_expand("npub");
    checked.extend(&data);
    if bech32_polymod(&checked) != 1 {
        return None;
    }
    if data.len() < 6 {
        return None;
    }
    let decoded = convertbits(&data[..data.len() - 6], 5, 8, false)?;
    if decoded.len() != 32 {
        return None;
    }
    Some(
        decoded
            .iter()
            .map(|b| format!("{:02x}", *b as u8))
            .collect(),
    )
}

/// Expand `npub` entries in an allowlist to their hex form. Hex entries pass
/// through unchanged; each valid `npub1…` entry adds its 64-char hex pubkey so
/// either form authorizes the same identity. Invalid entries are kept as-is.
pub fn normalize_nostr_allow_entries(entries: &HashSet<String>) -> HashSet<String> {
    let mut expanded = entries.clone();
    for entry in entries {
        if entry.to_lowercase().starts_with("npub1") {
            if let Some(hex) = npub_to_hex(entry) {
                expanded.insert(hex);
            }
        }
    }
    expanded
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn allow_set_from_list_and_csv() {
        assert_eq!(
            coerce_allow_set(Some(&json!(["123", " 456 ", ""]))),
            ["123", "456"].iter().map(|s| s.to_string()).collect()
        );
        // Scalar string splits on commas (not chars).
        assert_eq!(
            coerce_allow_set(Some(&json!("123,456"))),
            ["123", "456"].iter().map(|s| s.to_string()).collect()
        );
        assert!(coerce_allow_set(None).is_empty());
    }

    #[test]
    fn npub_decodes_to_hex() {
        // A known Nostr test vector (BIP-340 / NIP-19): this npub decodes to
        // the 64-char hex pubkey below.
        let npub = "npub180cvv07tjdrrgpa0j7j7tmnyl2yr6yr7l8j4s3evf6u64th6gkwsyjh6w6";
        let hex = npub_to_hex(npub).unwrap();
        assert_eq!(
            hex,
            "3bf0c63fcb93463407af97a5e5ee64fa883d107ef9e558472c4eb9aaaefa459d"
        );
        assert_eq!(hex.len(), 64);
    }

    #[test]
    fn npub_rejects_bad_input() {
        assert_eq!(npub_to_hex("not-an-npub"), None);
        assert_eq!(npub_to_hex("npub1invalid"), None);
        // A hex pubkey is not an npub -> None (caller keeps hex as-is).
        assert_eq!(npub_to_hex("3bf0c63f"), None);
    }

    #[test]
    fn nostr_allowlist_expands_npub() {
        let mut entries = HashSet::new();
        entries
            .insert("npub180cvv07tjdrrgpa0j7j7tmnyl2yr6yr7l8j4s3evf6u64th6gkwsyjh6w6".to_string());
        entries.insert("deadbeef".to_string()); // a plain entry kept as-is
        let expanded = normalize_nostr_allow_entries(&entries);
        assert!(
            expanded.contains("3bf0c63fcb93463407af97a5e5ee64fa883d107ef9e558472c4eb9aaaefa459d")
        );
        assert!(expanded.contains("deadbeef"));
        // The original npub is retained too.
        assert!(expanded.len() >= 3);
    }

    #[test]
    fn gate_env_reads_with_default() {
        std::env::remove_var("HERMES_TEST_GATE_XYZ");
        assert_eq!(
            platform_gate_env("HERMES_TEST_GATE_XYZ", "fallback"),
            "fallback"
        );
        std::env::set_var("HERMES_TEST_GATE_XYZ", "  value  ");
        assert_eq!(
            platform_gate_env("HERMES_TEST_GATE_XYZ", "fallback"),
            "value"
        );
        std::env::remove_var("HERMES_TEST_GATE_XYZ");
        assert_eq!(platform_gate_env("", "d"), "d");
    }
}
