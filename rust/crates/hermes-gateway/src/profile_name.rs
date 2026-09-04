//! Profile-name normalization and validation.
//!
// Public API is ahead of some callers (profile routing uses it today).
#![allow(dead_code)]
//!
//! Ported from `hermes_cli/profiles.py` (`normalize_profile_name` /
//! `validate_profile_name`). Named profiles are stored lowercase under
//! `profiles/<id>/`; the id doubles as an on-disk directory name, so validation
//! is a path-traversal guard: the id must match `[a-z0-9][a-z0-9_-]{0,63}` and
//! must not be one of a few reserved names. `default` is a special alias for the
//! built-in root profile (`~/.hermes`) and always passes.

/// Names that would create confusing on-disk collisions or get refused at
/// alias-creation time anyway.
const RESERVED_NAMES: [&str; 6] = ["hermes", "default", "test", "tmp", "root", "sudo"];

/// Why a profile name was rejected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProfileNameError {
    Empty,
    Invalid(String),
    Reserved(String),
}

impl std::fmt::Display for ProfileNameError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ProfileNameError::Empty => write!(f, "profile name cannot be empty"),
            ProfileNameError::Invalid(n) => write!(
                f,
                "Invalid profile name {n:?}. Must match [a-z0-9][a-z0-9_-]{{0,63}}"
            ),
            ProfileNameError::Reserved(n) => write!(
                f,
                "Profile name {n:?} is reserved (collides with the Hermes install \
                 or a common system binary). Pick a different name."
            ),
        }
    }
}

impl std::error::Error for ProfileNameError {}

/// Canonical profile id used on disk and in CLI `-p` argv. `default` (any case)
/// collapses to `"default"`; every other name is lowercased. Empty is rejected.
pub fn normalize_profile_name(name: &str) -> Result<String, ProfileNameError> {
    let stripped = name.trim();
    if stripped.is_empty() {
        return Err(ProfileNameError::Empty);
    }
    if stripped.eq_ignore_ascii_case("default") {
        return Ok("default".to_string());
    }
    Ok(stripped.to_lowercase())
}

/// True if `name` matches `^[a-z0-9][a-z0-9_-]{0,63}$` (the on-disk id regex).
fn is_valid_profile_id(name: &str) -> bool {
    let bytes = name.as_bytes();
    if bytes.is_empty() || bytes.len() > 64 {
        return false;
    }
    let first = bytes[0];
    let first_ok = first.is_ascii_lowercase() || first.is_ascii_digit();
    if !first_ok {
        return false;
    }
    bytes[1..]
        .iter()
        .all(|&b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
}

/// Validate a profile id as-given (strict lowercase). Callers accepting mixed
/// case should [`normalize_profile_name`] first. `default` is a pass-through.
pub fn validate_profile_name(name: &str) -> Result<(), ProfileNameError> {
    if name == "default" {
        return Ok(());
    }
    if !is_valid_profile_id(name) {
        return Err(ProfileNameError::Invalid(name.to_string()));
    }
    if RESERVED_NAMES.contains(&name) {
        return Err(ProfileNameError::Reserved(name.to_string()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_default_and_case() {
        assert_eq!(normalize_profile_name("Default").unwrap(), "default");
        assert_eq!(normalize_profile_name("  DEFAULT ").unwrap(), "default");
        assert_eq!(normalize_profile_name("MyProfile").unwrap(), "myprofile");
        assert_eq!(normalize_profile_name("  work ").unwrap(), "work");
        assert_eq!(normalize_profile_name("   "), Err(ProfileNameError::Empty));
    }

    #[test]
    fn validate_accepts_good_ids() {
        assert!(validate_profile_name("default").is_ok());
        assert!(validate_profile_name("work").is_ok());
        assert!(validate_profile_name("a1_b-c").is_ok());
        assert!(validate_profile_name("0abc").is_ok());
        assert!(validate_profile_name(&"a".repeat(64)).is_ok());
    }

    #[test]
    fn validate_rejects_bad_ids() {
        assert!(matches!(
            validate_profile_name("Work"),
            Err(ProfileNameError::Invalid(_))
        )); // uppercase
        assert!(matches!(
            validate_profile_name("_leading"),
            Err(ProfileNameError::Invalid(_))
        )); // first char must be alnum
        assert!(matches!(
            validate_profile_name("../etc"),
            Err(ProfileNameError::Invalid(_))
        )); // path traversal
        assert!(matches!(
            validate_profile_name(&"a".repeat(65)),
            Err(ProfileNameError::Invalid(_))
        )); // too long
        assert!(matches!(
            validate_profile_name(""),
            Err(ProfileNameError::Invalid(_))
        ));
    }

    #[test]
    fn validate_rejects_reserved() {
        for r in ["hermes", "test", "tmp", "root", "sudo"] {
            assert!(matches!(
                validate_profile_name(r),
                Err(ProfileNameError::Reserved(_))
            ));
        }
    }
}
