//! Port of gateway/cwd_placeholder.py.
//!
// Public API is ahead of its callers (the terminal-backend wiring uses it).
#![allow(dead_code)]
//!
//! Resolve gateway `terminal.cwd` placeholder values to `TERMINAL_CWD`.
//!
//! When `terminal.cwd` is unset or a placeholder (`.`, `auto`, `cwd`), the
//! gateway must not blindly map the host home directory into container
//! backends. Docker with workspace mounting still needs an explicit host path
//! signal (`MESSAGING_CWD` or an absolute config path) for the terminal tool to
//! map `/host/project` -> `/workspace`.

/// The values treated as "no explicit cwd, resolve it for me".
pub const CWD_PLACEHOLDERS: [&str; 3] = [".", "auto", "cwd"];

fn is_placeholder(value: &str) -> bool {
    CWD_PLACEHOLDERS.contains(&value)
}

/// Return the `TERMINAL_CWD` value to set, or `None` to leave it unset.
///
/// Cases:
///   - **local** + placeholder -> `messaging_cwd` or `home_fallback`
///   - **docker** + placeholder + mount on + host `messaging_cwd` -> host path
///     (for the terminal tool's `/workspace` mapping)
///   - **docker** + placeholder + mount off -> `None` (sandbox default)
///   - other non-local backends + placeholder -> `None`
pub fn resolve_placeholder_terminal_cwd(
    configured_cwd: &str,
    terminal_backend: &str,
    messaging_cwd: Option<&str>,
    docker_mount_cwd_to_workspace: bool,
    home_fallback: &str,
) -> Option<String> {
    if !configured_cwd.is_empty() && !is_placeholder(configured_cwd) {
        return Some(configured_cwd.to_string());
    }

    let backend = terminal_backend.trim().to_lowercase();
    let backend = if backend.is_empty() {
        "local".to_string()
    } else {
        backend
    };

    if backend == "local" {
        let messaging = messaging_cwd.unwrap_or("").trim();
        return Some(if messaging.is_empty() {
            home_fallback.to_string()
        } else {
            messaging.to_string()
        });
    }

    if backend == "docker" && docker_mount_cwd_to_workspace {
        let messaging = messaging_cwd.unwrap_or("").trim();
        if !messaging.is_empty() && !is_placeholder(messaging) {
            return Some(messaging.to_string());
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_cwd_passes_through() {
        assert_eq!(
            resolve_placeholder_terminal_cwd("/srv/app", "docker", None, false, "/home/u"),
            Some("/srv/app".to_string())
        );
    }

    #[test]
    fn local_placeholder_prefers_messaging_then_home() {
        assert_eq!(
            resolve_placeholder_terminal_cwd(".", "local", Some("/msg/dir"), false, "/home/u"),
            Some("/msg/dir".to_string())
        );
        assert_eq!(
            resolve_placeholder_terminal_cwd("auto", "local", None, false, "/home/u"),
            Some("/home/u".to_string())
        );
        // Empty backend defaults to local.
        assert_eq!(
            resolve_placeholder_terminal_cwd("cwd", "", Some("  "), false, "/home/u"),
            Some("/home/u".to_string())
        );
    }

    #[test]
    fn docker_placeholder_with_mount_uses_host_messaging() {
        assert_eq!(
            resolve_placeholder_terminal_cwd(".", "docker", Some("/host/project"), true, "/home/u"),
            Some("/host/project".to_string())
        );
        // Mount on but messaging itself a placeholder -> None (no host signal).
        assert_eq!(
            resolve_placeholder_terminal_cwd(".", "docker", Some("auto"), true, "/home/u"),
            None
        );
    }

    #[test]
    fn docker_placeholder_without_mount_is_none() {
        assert_eq!(
            resolve_placeholder_terminal_cwd(
                ".",
                "Docker",
                Some("/host/project"),
                false,
                "/home/u"
            ),
            None
        );
    }

    #[test]
    fn other_backend_placeholder_is_none() {
        assert_eq!(
            resolve_placeholder_terminal_cwd("cwd", "podman", Some("/x"), true, "/home/u"),
            None
        );
    }
}
