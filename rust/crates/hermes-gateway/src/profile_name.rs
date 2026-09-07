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

/// Infer the running profile from its home, as get_active_profile_name does.
/// The sticky active_profile file selects a launch profile elsewhere; it must
/// not override the home of an already running gateway. Missing path tails
/// remain resolvable, so a provisioned state.db is not required for identity.
pub fn active_profile_name(
    home: &std::path::Path,
    root: &std::path::Path,
) -> std::io::Result<String> {
    let resolve = |path: &std::path::Path| -> std::io::Result<std::path::PathBuf> {
        let absolute = if path.is_absolute() {
            path.to_path_buf()
        } else {
            std::env::current_dir()?.join(path)
        };
        let text = absolute.to_str().ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidData, "non-Unicode profile path")
        })?;
        crate::file_read_safety::realpath_abs(text.to_owned())
    };
    let home = resolve(home)?;
    if home == resolve(root)? {
        return Ok("default".into());
    }
    let profiles = resolve(&root.join("profiles"))?;
    if let Ok(relative) = home.strip_prefix(profiles) {
        if relative.components().count() == 1 {
            if let Some(name) = relative.to_str() {
                // Python's regex '$' accepts one terminal newline. Inference
                // checks only the ID syntax, not the provisioning reserved list.
                if is_valid_profile_id(name.strip_suffix('\n').unwrap_or(name)) {
                    return Ok(name.into());
                }
            }
        }
    }
    Ok("custom".into())
}

/// Prompt identity for an explicit agent home, from _profile_name_for_home.
/// Unlike ambient profile inference, this accepts any first path component
/// below profiles (including nested homes), and calls unrelated homes default.
pub fn agent_profile_name(home: &std::path::Path, root: &std::path::Path) -> String {
    let resolve = |path: &std::path::Path| -> Option<std::path::PathBuf> {
        let absolute = if path.is_absolute() {
            path.to_owned()
        } else {
            std::env::current_dir().ok()?.join(path)
        };
        crate::file_read_safety::realpath_abs(absolute.to_str()?.to_owned()).ok()
    };
    let name = || -> Option<String> {
        let home = resolve(home)?;
        let profiles = resolve(&root.join("profiles"))?;
        let relative = home.strip_prefix(profiles).ok()?;
        relative
            .components()
            .next()?
            .as_os_str()
            .to_str()
            .map(str::to_owned)
    };
    name().unwrap_or_else(|| "default".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn active_identity_comes_from_home_without_provisioning_or_normalization() {
        let root =
            std::env::temp_dir().join(format!("hermes-profile-inference-{}", std::process::id()));
        for (relative, expected) in [
            ("", "default"),
            ("profiles/work", "work"),
            ("profiles/Work", "custom"),
            ("profiles/python", "python"),
            ("profiles/work/nested", "custom"),
            ("elsewhere", "custom"),
            ("profiles/work\n", "work\n"),
            ("profiles/work\n\n", "custom"),
            ("profiles/work/../other", "other"),
        ] {
            assert_eq!(
                active_profile_name(&root.join(relative), &root).unwrap(),
                expected
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn active_identity_resolves_symlinks_before_parent_components() {
        let root =
            std::env::temp_dir().join(format!("hermes-profile-links-{}", std::process::id()));
        std::fs::create_dir_all(root.join("profiles/work/nested")).unwrap();
        std::os::unix::fs::symlink(root.join("profiles/work/nested"), root.join("alias")).unwrap();
        assert_eq!(
            active_profile_name(&root.join("alias/.."), &root).unwrap(),
            "work"
        );
        std::os::unix::fs::symlink("loop", root.join("loop")).unwrap();
        assert!(active_profile_name(&root.join("loop"), &root).is_err());
        std::fs::remove_dir_all(root).unwrap();
    }

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
