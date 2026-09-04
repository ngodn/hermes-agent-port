//! Port of gateway/hosted_room_execution_policy.py.
//!
// Public API is ahead of its callers (wired later).
#![allow(dead_code)]
//! Target-issued execution authority for RoomLink member turns. A hosted room's
//! target profile signs a small, immutable policy (allowed toolsets, approval
//! mode, iteration cap) with a sha256 digest over a canonical JSON form; the
//! agent and approval boundaries later verify that digest before acting. This
//! module ports the pure pieces: the [`RoomExecutionPolicy`] value type with its
//! strict validating parser and digest check, the canonical-JSON/digest helpers,
//! and the context-scoped "current policy" binding. The `execution_policy_mapping`
//! entry point in Python reads live gateway config to derive the four policy
//! inputs; that config-resolution front half is coupled to `gateway.run`,
//! `hermes_cli`, and `tools.approval` (none ported yet), so only its pure signing
//! tail is ported here as [`sign_execution_policy`].

use std::cell::RefCell;
use std::collections::HashSet;

use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

/// Only version 1 policies are recognized.
pub const POLICY_VERSION: i64 = 1;
/// Upper bound on the number of enabled toolsets in a policy.
pub const MAX_POLICY_TOOLSETS: usize = 128;
/// Iteration ceiling: 2^53 - 1, the largest integer that also round-trips
/// exactly through a JSON double, so the cap survives any JSON transport.
pub const MAX_POLICY_ITERATIONS: i64 = (1i64 << 53) - 1;

/// A RoomLink execution policy is malformed or no longer current. Mirrors the
/// Python `RoomExecutionPolicyError(ValueError)`; carries the same messages.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoomExecutionPolicyError(pub String);

impl std::fmt::Display for RoomExecutionPolicyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for RoomExecutionPolicyError {}

fn err(message: &str) -> RoomExecutionPolicyError {
    RoomExecutionPolicyError(message.to_string())
}

/// Python truthiness for a JSON value: None/false/0/""/[]/{} are falsy, the rest
/// truthy. NaN is truthy, mirroring `bool(float("nan"))`.
fn is_truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                i != 0
            } else if let Some(u) = n.as_u64() {
                u != 0
            } else if let Some(f) = n.as_f64() {
                f != 0.0
            } else {
                false
            }
        }
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

/// Port of Python `str(value or "")`: falsy values collapse to the empty string,
/// otherwise the value's `str()` form. For arrays/objects Python would produce a
/// bracketed repr; we fall back to the JSON text, which likewise never satisfies
/// the identifier regex, so the outcome (rejection) matches.
fn py_str_or_empty(value: &Value) -> String {
    if !is_truthy(value) {
        return String::new();
    }
    match value {
        Value::String(s) => s.clone(),
        // Only `true` is truthy, and Python renders it as "True".
        Value::Bool(_) => "True".to_string(),
        Value::Number(n) => n.to_string(),
        other => other.to_string(),
    }
}

/// True when `value` equals Python's `1` under `==`: int 1, float 1.0, or `True`.
fn python_eq_one(value: &Value) -> bool {
    match value {
        Value::Bool(b) => *b,
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                i == 1
            } else if let Some(u) = n.as_u64() {
                u == 1
            } else if let Some(f) = n.as_f64() {
                f == 1.0
            } else {
                false
            }
        }
        _ => false,
    }
}

/// True if `s` fully matches `^[A-Za-z0-9][A-Za-z0-9._:-]*$`. The regex only
/// admits ASCII, so any non-ASCII character fails.
fn is_valid_identifier(s: &str) -> bool {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphanumeric() => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | ':' | '-'))
}

/// Port of `_identifier` for an already-stringified input. Strips, then requires
/// a non-empty value of at most 128 characters matching the identifier regex.
fn identifier_from_str(raw: &str, field: &str) -> Result<String, RoomExecutionPolicyError> {
    let normalized = raw.trim();
    if normalized.is_empty() || normalized.chars().count() > 128 || !is_valid_identifier(normalized)
    {
        return Err(err(&format!("{field} is invalid")));
    }
    Ok(normalized.to_string())
}

/// Port of `_identifier(value, field=...)`: coerce with Python `str(value or "")`
/// then validate.
fn identifier(value: &Value, field: &str) -> Result<String, RoomExecutionPolicyError> {
    identifier_from_str(&py_str_or_empty(value), field)
}

/// Canonical JSON of the unsigned policy body, matching Python
/// `json.dumps(..., ensure_ascii=True, sort_keys=True, separators=(",", ":"))`.
/// Keys are emitted in sorted order and all field values are ASCII by
/// construction (identifiers, a fixed approval-mode word, integers), so
/// serde_json's compact escaping reproduces the Python bytes exactly.
fn canonical_unsigned(
    target_profile: &str,
    enabled_toolsets: &[String],
    approval_mode: &str,
    max_iterations: i64,
) -> Vec<u8> {
    let mut s = String::new();
    s.push('{');
    s.push_str("\"approval_mode\":");
    s.push_str(&serde_json::to_string(approval_mode).expect("string encodes"));
    s.push_str(",\"enabled_toolsets\":");
    s.push_str(&serde_json::to_string(enabled_toolsets).expect("string list encodes"));
    s.push_str(",\"max_iterations\":");
    s.push_str(&max_iterations.to_string());
    s.push_str(",\"target_profile\":");
    s.push_str(&serde_json::to_string(target_profile).expect("string encodes"));
    s.push_str(",\"version\":");
    s.push_str(&POLICY_VERSION.to_string());
    s.push('}');
    s.into_bytes()
}

/// Lowercase hex sha256 of `bytes`.
fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

/// Immutable target policy applied at the agent and approval boundaries. Mirrors
/// the frozen `RoomExecutionPolicy` dataclass.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoomExecutionPolicy {
    pub version: i64,
    pub target_profile: String,
    pub enabled_toolsets: Vec<String>,
    pub approval_mode: String,
    pub max_iterations: i64,
    pub policy_digest: String,
}

impl RoomExecutionPolicy {
    /// Port of `RoomExecutionPolicy.from_mapping`. Strictly validates a decoded
    /// policy mapping: exactly the six required keys, supported version, a valid
    /// target-profile identifier, a non-empty deduped toolset list (bounded by
    /// [`MAX_POLICY_TOOLSETS`] and containing `bot_room`), a known approval mode,
    /// an integer iteration count in `1..=MAX_POLICY_ITERATIONS`, and a
    /// `policy_digest` that matches the recomputed sha256 over the canonical
    /// unsigned body.
    pub fn from_mapping(value: &Value) -> Result<Self, RoomExecutionPolicyError> {
        const REQUIRED: [&str; 6] = [
            "version",
            "target_profile",
            "enabled_toolsets",
            "approval_mode",
            "max_iterations",
            "policy_digest",
        ];
        let obj = match value {
            Value::Object(m) => m,
            _ => return Err(err("execution policy fields are invalid")),
        };
        // set(value) != required: same count and every required key present.
        if obj.len() != REQUIRED.len() || !REQUIRED.iter().all(|k| obj.contains_key(*k)) {
            return Err(err("execution policy fields are invalid"));
        }
        if !python_eq_one(&obj["version"]) {
            return Err(err("execution policy version is unsupported"));
        }
        let target_profile = identifier(&obj["target_profile"], "target_profile")?;

        let raw_toolsets = match &obj["enabled_toolsets"] {
            Value::Array(a) => a,
            _ => return Err(err("enabled_toolsets are invalid")),
        };
        if raw_toolsets.is_empty() || raw_toolsets.len() > MAX_POLICY_TOOLSETS {
            return Err(err("enabled_toolsets are invalid"));
        }
        let mut toolsets: Vec<String> = Vec::with_capacity(raw_toolsets.len());
        for item in raw_toolsets {
            toolsets.push(identifier(item, "enabled_toolset")?);
        }
        toolsets.sort();
        let unique: HashSet<&String> = toolsets.iter().collect();
        if unique.len() != toolsets.len() || !toolsets.iter().any(|t| t == "bot_room") {
            return Err(err("enabled_toolsets are invalid"));
        }

        let approval_mode = py_str_or_empty(&obj["approval_mode"]).trim().to_lowercase();
        if !matches!(approval_mode.as_str(), "manual" | "smart" | "off") {
            return Err(err("approval_mode is invalid"));
        }

        let max_iterations = parse_max_iterations(&obj["max_iterations"])?;

        let expected = sha256_hex(&canonical_unsigned(
            &target_profile,
            &toolsets,
            &approval_mode,
            max_iterations,
        ));
        let supplied = py_str_or_empty(&obj["policy_digest"]).trim().to_lowercase();
        if supplied != expected {
            return Err(err("policy_digest does not match the execution policy"));
        }

        Ok(RoomExecutionPolicy {
            version: POLICY_VERSION,
            target_profile,
            enabled_toolsets: toolsets,
            approval_mode,
            max_iterations,
            policy_digest: supplied,
        })
    }

    /// Port of `as_mapping`: the policy as a plain JSON object.
    pub fn as_mapping(&self) -> Value {
        let mut map = Map::new();
        map.insert("version".to_string(), Value::from(self.version));
        map.insert(
            "target_profile".to_string(),
            Value::from(self.target_profile.clone()),
        );
        map.insert(
            "enabled_toolsets".to_string(),
            Value::from(self.enabled_toolsets.clone()),
        );
        map.insert(
            "approval_mode".to_string(),
            Value::from(self.approval_mode.clone()),
        );
        map.insert(
            "max_iterations".to_string(),
            Value::from(self.max_iterations),
        );
        map.insert(
            "policy_digest".to_string(),
            Value::from(self.policy_digest.clone()),
        );
        Value::Object(map)
    }
}

/// Port of the `max_iterations` guard: a real integer (not a bool, not a float)
/// within `1..=MAX_POLICY_ITERATIONS`.
fn parse_max_iterations(value: &Value) -> Result<i64, RoomExecutionPolicyError> {
    // JSON bools are their own value kind, so they are excluded here just as
    // Python excludes `bool` before its int check. A float number (is_f64) is
    // rejected the way Python rejects a non-int.
    let n = match value {
        Value::Number(n) if !n.is_f64() => n,
        _ => return Err(err("max_iterations is invalid")),
    };
    // Any in-range value fits in i64; an out-of-i64 u64 is above the cap anyway.
    let candidate = n.as_i64().unwrap_or(i64::MAX);
    if !(1..=MAX_POLICY_ITERATIONS).contains(&candidate) {
        return Err(err("max_iterations is invalid"));
    }
    Ok(candidate)
}

/// Pure signing tail of Python's `execution_policy_mapping`: given the four
/// already-resolved policy inputs, build the unsigned body, attach its sha256
/// digest, and round-trip through [`RoomExecutionPolicy::from_mapping`] so the
/// result is validated exactly like an externally supplied policy. Callers pass
/// `enabled_toolsets` pre-sorted and deduped with `bot_room` present, as the
/// Python caller does via `sorted({*platform_tools, "bot_room"})`.
///
/// The config-driven front half of `execution_policy_mapping` (loading gateway
/// config, resolving the turn limit, platform toolset, and approval mode) is
/// deferred until `gateway.run`, `hermes_cli`, and `tools.approval` are ported.
pub fn sign_execution_policy(
    target_profile: &str,
    enabled_toolsets: &[String],
    approval_mode: &str,
    max_iterations: i64,
) -> Result<Value, RoomExecutionPolicyError> {
    let target_profile = identifier_from_str(target_profile, "target_profile")?;
    let digest = sha256_hex(&canonical_unsigned(
        &target_profile,
        enabled_toolsets,
        approval_mode,
        max_iterations,
    ));

    let mut map = Map::new();
    map.insert("version".to_string(), Value::from(POLICY_VERSION));
    map.insert("target_profile".to_string(), Value::from(target_profile));
    map.insert(
        "enabled_toolsets".to_string(),
        Value::from(enabled_toolsets.to_vec()),
    );
    map.insert("approval_mode".to_string(), Value::from(approval_mode));
    map.insert("max_iterations".to_string(), Value::from(max_iterations));
    map.insert("policy_digest".to_string(), Value::from(digest));

    RoomExecutionPolicy::from_mapping(&Value::Object(map)).map(|p| p.as_mapping())
}

thread_local! {
    // Context-scoped current policy. Python uses a contextvars.ContextVar; a
    // thread-local preserves the set/reset(token) contract for synchronous use.
    static CURRENT_POLICY: RefCell<Option<RoomExecutionPolicy>> = const { RefCell::new(None) };
}

/// Restores the value that was current before the matching bind. Mirrors the
/// contextvars `Token` returned by `ContextVar.set`.
#[derive(Debug, Clone)]
pub struct PolicyToken {
    previous: Option<RoomExecutionPolicy>,
}

/// Port of `bind_room_execution_policy`: set the current policy, returning a
/// token that [`reset_room_execution_policy`] uses to restore the prior value.
pub fn bind_room_execution_policy(policy: RoomExecutionPolicy) -> PolicyToken {
    CURRENT_POLICY.with(|cell| {
        let previous = cell.borrow().clone();
        *cell.borrow_mut() = Some(policy);
        PolicyToken { previous }
    })
}

/// Port of `reset_room_execution_policy`: restore the value captured by `token`.
pub fn reset_room_execution_policy(token: PolicyToken) {
    CURRENT_POLICY.with(|cell| {
        *cell.borrow_mut() = token.previous;
    });
}

/// Port of `current_room_execution_policy`: the policy bound in this context, if
/// any.
pub fn current_room_execution_policy() -> Option<RoomExecutionPolicy> {
    CURRENT_POLICY.with(|cell| cell.borrow().clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn valid_mapping() -> Value {
        // Signed body for target_profile="acct", toolsets sorted with bot_room,
        // approval_mode="manual", max_iterations=10. Digest is the golden value
        // computed from the Python canonical-JSON form.
        sign_execution_policy(
            "acct",
            &["bot_room".to_string(), "web".to_string()],
            "manual",
            10,
        )
        .unwrap()
    }

    #[test]
    fn canonical_json_matches_python_golden() {
        let bytes = canonical_unsigned(
            "acct",
            &["bot_room".to_string(), "web".to_string()],
            "manual",
            10,
        );
        assert_eq!(
            std::str::from_utf8(&bytes).unwrap(),
            r#"{"approval_mode":"manual","enabled_toolsets":["bot_room","web"],"max_iterations":10,"target_profile":"acct","version":1}"#
        );
        assert_eq!(
            sha256_hex(&bytes),
            "c43bf09c8deaaf0f4505e0f132eb1a06c752a350260ad61c1daeeeb703ade7cf"
        );
    }

    #[test]
    fn from_mapping_accepts_valid_and_roundtrips() {
        let mapping = valid_mapping();
        let policy = RoomExecutionPolicy::from_mapping(&mapping).unwrap();
        assert_eq!(policy.version, 1);
        assert_eq!(policy.target_profile, "acct");
        assert_eq!(policy.enabled_toolsets, vec!["bot_room", "web"]);
        assert_eq!(policy.approval_mode, "manual");
        assert_eq!(policy.max_iterations, 10);
        // as_mapping reproduces the input mapping exactly.
        assert_eq!(policy.as_mapping(), mapping);
    }

    #[test]
    fn sign_sorts_and_requires_bot_room() {
        // bot_room missing -> from_mapping rejects even though signed.
        let signed = sign_execution_policy("acct", &["web".to_string()], "manual", 10);
        assert_eq!(signed.unwrap_err(), err("enabled_toolsets are invalid"));
    }

    #[test]
    fn rejects_extra_or_missing_keys() {
        let mut mapping = valid_mapping();
        mapping
            .as_object_mut()
            .unwrap()
            .insert("extra".into(), json!(1));
        assert_eq!(
            RoomExecutionPolicy::from_mapping(&mapping).unwrap_err(),
            err("execution policy fields are invalid")
        );

        let mut mapping = valid_mapping();
        mapping.as_object_mut().unwrap().remove("approval_mode");
        assert_eq!(
            RoomExecutionPolicy::from_mapping(&mapping).unwrap_err(),
            err("execution policy fields are invalid")
        );
    }

    #[test]
    fn rejects_bad_version() {
        let mut mapping = valid_mapping();
        mapping
            .as_object_mut()
            .unwrap()
            .insert("version".into(), json!(2));
        assert_eq!(
            RoomExecutionPolicy::from_mapping(&mapping).unwrap_err(),
            err("execution policy version is unsupported")
        );
    }

    #[test]
    fn version_accepts_python_one_equivalents() {
        // Python `value != 1` treats True and 1.0 as equal to 1; the returned
        // version is always the int 1 regardless, and the digest still matches
        // because the unsigned body always uses POLICY_VERSION.
        for v in [json!(1.0), json!(true)] {
            let mut mapping = valid_mapping();
            mapping.as_object_mut().unwrap().insert("version".into(), v);
            let policy = RoomExecutionPolicy::from_mapping(&mapping).unwrap();
            assert_eq!(policy.version, 1);
        }
        // false is not equal to 1.
        let mut mapping = valid_mapping();
        mapping
            .as_object_mut()
            .unwrap()
            .insert("version".into(), json!(false));
        assert_eq!(
            RoomExecutionPolicy::from_mapping(&mapping).unwrap_err(),
            err("execution policy version is unsupported")
        );
    }

    #[test]
    fn rejects_bad_approval_mode() {
        let signed = sign_execution_policy("acct", &["bot_room".to_string()], "loose", 10);
        assert_eq!(signed.unwrap_err(), err("approval_mode is invalid"));
    }

    #[test]
    fn accepts_all_approval_modes() {
        for mode in ["manual", "smart", "off"] {
            let signed =
                sign_execution_policy("acct", &["bot_room".to_string()], mode, 10).unwrap();
            let policy = RoomExecutionPolicy::from_mapping(&signed).unwrap();
            assert_eq!(policy.approval_mode, mode);
        }
    }

    #[test]
    fn rejects_bad_max_iterations() {
        // Zero, over the cap, float, and bool are all invalid.
        assert!(sign_execution_policy("acct", &["bot_room".to_string()], "manual", 0).is_err());
        assert!(sign_execution_policy(
            "acct",
            &["bot_room".to_string()],
            "manual",
            MAX_POLICY_ITERATIONS + 1
        )
        .is_err());

        let mut mapping = valid_mapping();
        mapping
            .as_object_mut()
            .unwrap()
            .insert("max_iterations".into(), json!(10.5));
        assert_eq!(
            RoomExecutionPolicy::from_mapping(&mapping).unwrap_err(),
            err("max_iterations is invalid")
        );

        let mut mapping = valid_mapping();
        mapping
            .as_object_mut()
            .unwrap()
            .insert("max_iterations".into(), json!(true));
        assert_eq!(
            RoomExecutionPolicy::from_mapping(&mapping).unwrap_err(),
            err("max_iterations is invalid")
        );
    }

    #[test]
    fn accepts_max_iterations_boundary() {
        let signed = sign_execution_policy(
            "acct",
            &["bot_room".to_string()],
            "manual",
            MAX_POLICY_ITERATIONS,
        )
        .unwrap();
        let policy = RoomExecutionPolicy::from_mapping(&signed).unwrap();
        assert_eq!(policy.max_iterations, MAX_POLICY_ITERATIONS);
    }

    #[test]
    fn rejects_duplicate_toolsets() {
        let signed = sign_execution_policy(
            "acct",
            &["bot_room".to_string(), "web".to_string(), "web".to_string()],
            "manual",
            10,
        );
        assert_eq!(signed.unwrap_err(), err("enabled_toolsets are invalid"));
    }

    #[test]
    fn rejects_too_many_toolsets() {
        let mut toolsets = vec!["bot_room".to_string()];
        for i in 0..MAX_POLICY_TOOLSETS {
            toolsets.push(format!("t{i}"));
        }
        assert!(toolsets.len() > MAX_POLICY_TOOLSETS);
        let signed = sign_execution_policy("acct", &toolsets, "manual", 10);
        assert_eq!(signed.unwrap_err(), err("enabled_toolsets are invalid"));
    }

    #[test]
    fn rejects_bad_identifier() {
        // Space is not allowed in the identifier regex.
        let signed = sign_execution_policy("bad profile", &["bot_room".to_string()], "manual", 10);
        assert_eq!(signed.unwrap_err(), err("target_profile is invalid"));
        // Empty target profile.
        let signed = sign_execution_policy("", &["bot_room".to_string()], "manual", 10);
        assert_eq!(signed.unwrap_err(), err("target_profile is invalid"));
        // A leading punctuation char fails the first-char alnum rule.
        let signed = sign_execution_policy(
            "acct",
            &[".hidden".to_string(), "bot_room".to_string()],
            "manual",
            10,
        );
        assert_eq!(signed.unwrap_err(), err("enabled_toolset is invalid"));
    }

    #[test]
    fn rejects_digest_mismatch() {
        let mut mapping = valid_mapping();
        mapping
            .as_object_mut()
            .unwrap()
            .insert("policy_digest".into(), json!("0".repeat(64)));
        assert_eq!(
            RoomExecutionPolicy::from_mapping(&mapping).unwrap_err(),
            err("policy_digest does not match the execution policy")
        );
    }

    #[test]
    fn rejects_tampered_field_under_original_digest() {
        // Change target_profile but keep the old digest -> mismatch.
        let mut mapping = valid_mapping();
        mapping
            .as_object_mut()
            .unwrap()
            .insert("target_profile".into(), json!("other"));
        assert_eq!(
            RoomExecutionPolicy::from_mapping(&mapping).unwrap_err(),
            err("policy_digest does not match the execution policy")
        );
    }

    #[test]
    fn context_bind_reset_current() {
        assert_eq!(current_room_execution_policy(), None);
        let policy = RoomExecutionPolicy::from_mapping(&valid_mapping()).unwrap();

        let token = bind_room_execution_policy(policy.clone());
        assert_eq!(current_room_execution_policy(), Some(policy.clone()));

        // Nested bind restores the outer policy on reset.
        let other = RoomExecutionPolicy::from_mapping(
            &sign_execution_policy("acct", &["bot_room".to_string()], "off", 3).unwrap(),
        )
        .unwrap();
        let inner = bind_room_execution_policy(other.clone());
        assert_eq!(current_room_execution_policy(), Some(other));
        reset_room_execution_policy(inner);
        assert_eq!(current_room_execution_policy(), Some(policy));

        reset_room_execution_policy(token);
        assert_eq!(current_room_execution_policy(), None);
    }
}
