//! Port of gateway/whatsapp_identity.py. Canonicalizes WhatsApp sender identity across phone and LID aliases.
//!
// Public API is ahead of its callers while the WhatsApp adapter is ported.
#![allow(dead_code)]
//!
//! WhatsApp's bridge can surface the same human under two different JID shapes
//! within a single conversation:
//!
//! - LID form: `999999999999999@lid`
//! - Phone form: `15551234567@s.whatsapp.net`
//!
//! Both the authorization path and the session-key path need to collapse these
//! aliases to a single stable identity. This module is the single source of truth
//! for that resolution so the two paths can never drift apart.
//!
//! Public helpers:
//!
//! - [`normalize_whatsapp_identifier`]: strip JID/LID/device/plus syntax down to
//!   the bare numeric identifier.
//! - [`to_whatsapp_jid`]: format a bare phone or raw target into a bridge-safe
//!   outbound JID.
//! - [`canonical_whatsapp_identifier`]: walk the bridge's `lid-mapping-*.json`
//!   files and return a stable canonical identity across phone/LID variants.
//! - [`expand_whatsapp_aliases`]: return the full alias set for an identifier.
//!   Used by authorization code that needs to match any known form of a sender
//!   against an allowlist.

use std::collections::{HashSet, VecDeque};
use std::path::{Path, PathBuf};

use serde_json::Value;

/// WhatsApp JIDs are alphanumeric (or plus-prefixed) with optional `@`, `.`,
/// `+` and `-` separators. Checked to prevent path traversal when resolving
/// lid-mapping filenames.
fn is_safe_identifier(candidate: &str) -> bool {
    !candidate.is_empty()
        && candidate
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '@' | '.' | '+' | '-'))
}

/// Strip WhatsApp JID/LID syntax down to its stable numeric identifier.
///
/// Accepts any of the identifier shapes the WhatsApp bridge may emit:
/// `"60123456789@s.whatsapp.net"`, `"60123456789:47@s.whatsapp.net"`,
/// `"60123456789@lid"`, or a bare `"+601****6789"` / `"60123456789"`.
/// Returns just the numeric identifier (`"60123456789"`) suitable for
/// equality comparisons.
pub fn normalize_whatsapp_identifier(value: &str) -> String {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return String::new();
    }

    if let Some(idx) = trimmed.find('+') {
        let mut unplussed = String::with_capacity(trimmed.len().saturating_sub(1));
        unplussed.push_str(&trimmed[..idx]);
        unplussed.push_str(&trimmed[idx + 1..]);
        let before_colon = unplussed.split(':').next().unwrap_or("");
        let before_at = before_colon.split('@').next().unwrap_or("");
        before_at.to_string()
    } else {
        let before_colon = trimmed.split(':').next().unwrap_or("");
        let before_at = before_colon.split('@').next().unwrap_or("");
        before_at.to_string()
    }
}

/// A target that is just a phone number: optional leading `+` then digits
/// and the usual human separators (spaces, dots, dashes, parens).
fn is_bare_phone(candidate: &str) -> bool {
    let s = candidate.strip_prefix('+').unwrap_or(candidate);
    if s.is_empty() {
        return false;
    }
    s.chars()
        .all(|c| c.is_ascii_digit() || c.is_whitespace() || matches!(c, '(' | ')' | '.' | '-'))
}

/// Normalize an outbound WhatsApp target to a bridge-safe JID.
///
/// Baileys' `jidDecode` crashes on a bare phone number: it expects a
/// fully-qualified JID such as `50766715226@s.whatsapp.net`. This helper
/// is the inverse of [`normalize_whatsapp_identifier`]: instead of
/// stripping a JID down to its numeric core for comparison, it builds the
/// JID a send must use.
///
/// Behavior:
///
/// - `"+50766715226"` / `"50766715226"` -> `"50766715226@s.whatsapp.net"`
/// - `"50766715226@s.whatsapp.net"` -> unchanged
/// - `"group-id@g.us"` / `"130631430344750@lid"` -> unchanged
/// - `"user:device@s.whatsapp.net"` style colon-before-`@` -> `@` form
/// - anything that is not a recognizable bare phone -> returned unchanged
///
/// Returns an empty string for empty/whitespace input.
pub fn to_whatsapp_jid(value: &str) -> String {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return String::new();
    }

    let mut normalized = trimmed.to_string();

    // Drop a device suffix before the domain: user:device@domain is a
    // legacy Baileys shape whose :device part is not addressable; collapse
    // it to user@domain. (Mirrors normalize_whatsapp_identifier, which
    // splits the bare id on : for the same reason.)
    if normalized.contains(':') && normalized.contains('@') {
        if let Some((prefix, domain)) = normalized.split_once('@') {
            let user = prefix.split(':').next().unwrap_or("");
            normalized = format!("{user}@{domain}");
        }
    }

    // Already a fully-qualified JID: leave it alone.
    if normalized.contains('@') {
        return normalized;
    }

    if is_bare_phone(&normalized) {
        let digits: String = normalized.chars().filter(|c| c.is_ascii_digit()).collect();
        if !digits.is_empty() {
            return format!("{digits}@s.whatsapp.net");
        }
    }

    normalized
}

/// Resolve the base Hermes home directory.
fn get_hermes_home() -> PathBuf {
    if let Ok(val) = std::env::var("HERMES_HOME") {
        let trimmed = val.trim();
        if !trimmed.is_empty() {
            return PathBuf::from(trimmed);
        }
    }
    #[cfg(windows)]
    {
        if let Ok(val) = std::env::var("LOCALAPPDATA") {
            let trimmed = val.trim();
            if !trimmed.is_empty() {
                return PathBuf::from(trimmed).join("hermes");
            }
        }
    }
    if let Ok(home) = std::env::var("HOME").or_else(|_| std::env::var("USERPROFILE")) {
        let trimmed = home.trim();
        if !trimmed.is_empty() {
            return PathBuf::from(trimmed).join(".hermes");
        }
    }
    PathBuf::from(".hermes")
}

/// Return true if path exists and has content worth honoring.
/// Populated directory or non-directory file counts; empty directory does not.
fn legacy_path_has_content(path: &Path) -> bool {
    match std::fs::symlink_metadata(path) {
        Ok(meta) => {
            if meta.is_symlink() {
                match std::fs::metadata(path) {
                    Ok(target_meta) => {
                        if target_meta.is_dir() {
                            match std::fs::read_dir(path) {
                                Ok(mut entries) => entries.next().is_some(),
                                Err(_) => true,
                            }
                        } else {
                            true
                        }
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
                    Err(_) => true,
                }
            } else if meta.is_dir() {
                match std::fs::read_dir(path) {
                    Ok(mut entries) => entries.next().is_some(),
                    Err(_) => true,
                }
            } else {
                true
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
        Err(_) => true,
    }
}

/// Resolve the WhatsApp session directory with backward-compatibility fallback.
/// Matches get_hermes_dir("platforms/whatsapp/session", "whatsapp/session").
pub fn get_whatsapp_session_dir() -> PathBuf {
    let home = get_hermes_home();
    let old_path = home.join("whatsapp/session");
    if legacy_path_has_content(&old_path) {
        old_path
    } else {
        home.join("platforms/whatsapp/session")
    }
}

/// Resolve WhatsApp phone/LID aliases via bridge session mapping files in a given directory.
pub fn expand_whatsapp_aliases_in_dir(identifier: &str, session_dir: &Path) -> HashSet<String> {
    let normalized = normalize_whatsapp_identifier(identifier);
    if normalized.is_empty() {
        return HashSet::new();
    }

    let mut resolved: HashSet<String> = HashSet::new();
    let mut queue: VecDeque<String> = VecDeque::new();
    queue.push_back(normalized);

    while let Some(current) = queue.pop_front() {
        if current.is_empty() || resolved.contains(&current) {
            continue;
        }

        // Defense-in-depth: reject identifiers that could sneak path
        // separators / traversal segments into the lid-mapping-{current}
        // filename below.
        if !is_safe_identifier(&current) {
            continue;
        }

        resolved.insert(current.clone());

        for suffix in ["", "_reverse"] {
            let mapping_path = session_dir.join(format!("lid-mapping-{current}{suffix}.json"));
            if !mapping_path.exists() {
                continue;
            }

            let text = match std::fs::read_to_string(&mapping_path) {
                Ok(t) => t,
                Err(exc) => {
                    tracing::debug!(
                        "whatsapp_identity: failed to read {}: {exc}",
                        mapping_path.display()
                    );
                    continue;
                }
            };

            let parsed: Value = match serde_json::from_str(&text) {
                Ok(v) => v,
                Err(exc) => {
                    tracing::debug!(
                        "whatsapp_identity: failed to read {}: {exc}",
                        mapping_path.display()
                    );
                    continue;
                }
            };

            let raw_str = match parsed {
                Value::String(s) => s,
                Value::Number(n) => n.to_string(),
                _ => String::new(),
            };

            let mapped = normalize_whatsapp_identifier(&raw_str);
            if !mapped.is_empty() && !resolved.contains(&mapped) {
                queue.push_back(mapped);
            }
        }
    }

    resolved
}

/// Resolve WhatsApp phone/LID aliases via bridge session mapping files.
///
/// Returns the set of all identifiers transitively reachable through the
/// bridge's `lid-mapping-*.json` files, starting from `identifier`. The result
/// always includes the normalized input itself (when valid), so callers can
/// safely check containment against the return value without a fallback branch.
///
/// Returns an empty set if `identifier` normalizes to empty.
pub fn expand_whatsapp_aliases(identifier: &str) -> HashSet<String> {
    let session_dir = get_whatsapp_session_dir();
    expand_whatsapp_aliases_in_dir(identifier, &session_dir)
}

/// Return a stable WhatsApp sender identity across phone-JID/LID variants in a given directory.
pub fn canonical_whatsapp_identifier_in_dir(identifier: &str, session_dir: &Path) -> String {
    let normalized = normalize_whatsapp_identifier(identifier);
    if normalized.is_empty() {
        return String::new();
    }

    let aliases = expand_whatsapp_aliases_in_dir(&normalized, session_dir);
    aliases
        .into_iter()
        .min_by(|a, b| (a.len(), a.as_str()).cmp(&(b.len(), b.as_str())))
        .unwrap_or(normalized)
}

/// Return a stable WhatsApp sender identity across phone-JID/LID variants.
///
/// WhatsApp may surface the same person under either a phone-format JID
/// (`60123456789@s.whatsapp.net`) or a LID (`1234567890@lid`). This applies to
/// a DM `chat_id` and to the `participant_id` of a member inside a group chat.
/// Both represent a user identity, and the bridge may flip between the two
/// for the same human.
///
/// This helper reads the bridge's `lid-mapping-*.json` files, walks the mapping
/// transitively, and picks the shortest (numeric-preferred) alias as the canonical
/// identity.
///
/// Returns an empty string if `identifier` normalizes to empty. If no mapping
/// files exist yet (fresh bridge install), returns the normalized input unchanged.
pub fn canonical_whatsapp_identifier(identifier: &str) -> String {
    let session_dir = get_whatsapp_session_dir();
    canonical_whatsapp_identifier_in_dir(identifier, &session_dir)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::fs;

    #[test]
    fn test_normalize_identifier_shapes() {
        assert_eq!(
            normalize_whatsapp_identifier("60123456789@s.whatsapp.net"),
            "60123456789"
        );
        assert_eq!(
            normalize_whatsapp_identifier("60123456789:47@s.whatsapp.net"),
            "60123456789"
        );
        assert_eq!(
            normalize_whatsapp_identifier("60123456789@lid"),
            "60123456789"
        );
        assert_eq!(normalize_whatsapp_identifier("+60123456789"), "60123456789");
        assert_eq!(normalize_whatsapp_identifier("60123456789"), "60123456789");
        assert_eq!(
            normalize_whatsapp_identifier("  +60123456789:47@s.whatsapp.net  "),
            "60123456789"
        );
        assert_eq!(
            normalize_whatsapp_identifier("999999999999999@lid"),
            "999999999999999"
        );
    }

    #[test]
    fn test_normalize_identifier_empty_and_edge_cases() {
        assert_eq!(normalize_whatsapp_identifier(""), "");
        assert_eq!(normalize_whatsapp_identifier("   "), "");
        assert_eq!(normalize_whatsapp_identifier("+"), "");
        assert_eq!(normalize_whatsapp_identifier("+:47@s.whatsapp.net"), "");
        assert_eq!(normalize_whatsapp_identifier("@s.whatsapp.net"), "");
        assert_eq!(normalize_whatsapp_identifier(":47@s.whatsapp.net"), "");
    }

    #[test]
    fn test_to_whatsapp_jid_bare_phone() {
        assert_eq!(
            to_whatsapp_jid("+50766715226"),
            "50766715226@s.whatsapp.net"
        );
        assert_eq!(to_whatsapp_jid("50766715226"), "50766715226@s.whatsapp.net");
        assert_eq!(
            to_whatsapp_jid("+1 (555) 123-4567"),
            "15551234567@s.whatsapp.net"
        );
        assert_eq!(
            to_whatsapp_jid("+1.555.123.4567"),
            "15551234567@s.whatsapp.net"
        );
    }

    #[test]
    fn test_to_whatsapp_jid_passthrough_and_device_suffix() {
        assert_eq!(
            to_whatsapp_jid("50766715226@s.whatsapp.net"),
            "50766715226@s.whatsapp.net"
        );
        assert_eq!(
            to_whatsapp_jid("123456789-987654321@g.us"),
            "123456789-987654321@g.us"
        );
        assert_eq!(
            to_whatsapp_jid("130631430344750@lid"),
            "130631430344750@lid"
        );
        assert_eq!(to_whatsapp_jid("status@broadcast"), "status@broadcast");
        assert_eq!(to_whatsapp_jid("123@newsletter"), "123@newsletter");
        assert_eq!(
            to_whatsapp_jid("user:device@s.whatsapp.net"),
            "user@s.whatsapp.net"
        );
        assert_eq!(
            to_whatsapp_jid("50766715226:1@s.whatsapp.net"),
            "50766715226@s.whatsapp.net"
        );
        assert_eq!(to_whatsapp_jid(""), "");
        assert_eq!(to_whatsapp_jid("   "), "");
        assert_eq!(to_whatsapp_jid("invalid phone!"), "invalid phone!");
    }

    #[test]
    fn test_expand_and_canonical_empty() {
        assert!(expand_whatsapp_aliases("").is_empty());
        assert!(expand_whatsapp_aliases("   ").is_empty());
        assert_eq!(canonical_whatsapp_identifier(""), "");
        assert_eq!(canonical_whatsapp_identifier("   "), "");
    }

    #[test]
    fn test_expand_and_canonical_fresh_install_no_mappings() {
        let temp_dir = tempfile_dir("no_mappings");
        let aliases = expand_whatsapp_aliases_in_dir("15551234567@s.whatsapp.net", &temp_dir);
        assert_eq!(aliases, HashSet::from(["15551234567".to_string()]));

        let canonical =
            canonical_whatsapp_identifier_in_dir("15551234567@s.whatsapp.net", &temp_dir);
        assert_eq!(canonical, "15551234567");

        let _ = fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn test_lid_mapping_bidirectional_and_transitive() {
        let temp_dir = tempfile_dir("bidirectional");
        fs::create_dir_all(&temp_dir).unwrap();

        // Phone: 15551234567, LID: 999999999999999
        let phone = "15551234567";
        let lid = "999999999999999";

        fs::write(
            temp_dir.join(format!("lid-mapping-{lid}.json")),
            json!(format!("{phone}@s.whatsapp.net")).to_string(),
        )
        .unwrap();
        fs::write(
            temp_dir.join(format!("lid-mapping-{phone}_reverse.json")),
            json!(lid).to_string(),
        )
        .unwrap();

        let aliases_from_lid = expand_whatsapp_aliases_in_dir(&format!("{lid}@lid"), &temp_dir);
        assert_eq!(
            aliases_from_lid,
            HashSet::from([lid.to_string(), phone.to_string()])
        );

        let aliases_from_phone =
            expand_whatsapp_aliases_in_dir(&format!("{phone}@s.whatsapp.net"), &temp_dir);
        assert_eq!(
            aliases_from_phone,
            HashSet::from([lid.to_string(), phone.to_string()])
        );

        // Shortest alias is picked as canonical identity
        let canonical_from_lid =
            canonical_whatsapp_identifier_in_dir(&format!("{lid}@lid"), &temp_dir);
        assert_eq!(canonical_from_lid, phone);

        let canonical_from_phone =
            canonical_whatsapp_identifier_in_dir(&format!("{phone}@s.whatsapp.net"), &temp_dir);
        assert_eq!(canonical_from_phone, phone);

        let _ = fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn test_transitive_multi_hop_resolution_and_cycle() {
        let temp_dir = tempfile_dir("transitive");
        fs::create_dir_all(&temp_dir).unwrap();

        // 111 -> 2222 -> 33333 -> 111 (cycle)
        fs::write(
            temp_dir.join("lid-mapping-111.json"),
            json!("2222").to_string(),
        )
        .unwrap();
        fs::write(
            temp_dir.join("lid-mapping-2222.json"),
            json!("33333").to_string(),
        )
        .unwrap();
        fs::write(
            temp_dir.join("lid-mapping-33333.json"),
            json!("111").to_string(),
        )
        .unwrap();

        let aliases = expand_whatsapp_aliases_in_dir("111", &temp_dir);
        assert_eq!(
            aliases,
            HashSet::from(["111".to_string(), "2222".to_string(), "33333".to_string()])
        );

        let canonical = canonical_whatsapp_identifier_in_dir("33333", &temp_dir);
        assert_eq!(canonical, "111");

        let _ = fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn test_traversal_and_malformed_ids_rejected() {
        let temp_dir = tempfile_dir("traversal");
        fs::create_dir_all(&temp_dir).unwrap();

        // Identifier with invalid chars should not resolve mapping files
        let aliases = expand_whatsapp_aliases_in_dir("../../etc/passwd", &temp_dir);
        assert!(aliases.is_empty());

        let canonical = canonical_whatsapp_identifier_in_dir("../../etc/passwd", &temp_dir);
        assert_eq!(canonical, "../../etc/passwd");

        let _ = fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn test_corrupted_json_handled_gracefully() {
        let temp_dir = tempfile_dir("corrupted");
        fs::create_dir_all(&temp_dir).unwrap();

        fs::write(
            temp_dir.join("lid-mapping-12345.json"),
            "not valid json {{{",
        )
        .unwrap();

        let aliases = expand_whatsapp_aliases_in_dir("12345", &temp_dir);
        assert_eq!(aliases, HashSet::from(["12345".to_string()]));

        let canonical = canonical_whatsapp_identifier_in_dir("12345", &temp_dir);
        assert_eq!(canonical, "12345");

        let _ = fs::remove_dir_all(&temp_dir);
    }

    fn tempfile_dir(sub: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        let unique = format!(
            "hermes_test_wa_id_{}_{}_{}",
            sub,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        );
        p.push(unique);
        p
    }
}
