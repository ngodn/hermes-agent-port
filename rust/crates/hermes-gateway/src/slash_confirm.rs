//! Route-scoped confirmation for destructive native slash commands.
//!
//! Pending state is deliberately process-local and keyed by the stable gateway
//! route, not the rotating transcript id. A recognized reply atomically takes
//! the pending action before any asynchronous work, so duplicate messages and
//! future button callbacks cannot execute it twice.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use yaml_edit::path::YamlPath;
use yaml_edit::YamlFile;

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(300);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Choice {
    Once,
    Always,
    Cancel,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ApprovedReset {
    pub title: Option<String>,
    pub always: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Resolution {
    NotHandled,
    Expired,
    Cancelled { command: String },
    Approved(ApprovedReset),
}

#[derive(Clone)]
struct PendingReset {
    #[allow(dead_code)] // consumed by the adapter-button resolver below
    confirm_id: String,
    command: String,
    title: Option<String>,
    created_at: Instant,
}

/// Owns all pending destructive-command confirmations for one gateway life.
/// Its interface is shared by HTTP, text-based push ingress, and the later
/// adapter-button callback path.
pub struct SlashConfirmations {
    pending: Mutex<HashMap<String, PendingReset>>,
    next_id: AtomicU64,
    timeout: Duration,
    config_path: PathBuf,
}

impl SlashConfirmations {
    pub fn new(config_path: PathBuf) -> Self {
        Self::with_timeout(config_path, DEFAULT_TIMEOUT)
    }

    fn with_timeout(config_path: PathBuf, timeout: Duration) -> Self {
        Self {
            pending: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
            timeout,
            config_path,
        }
    }

    /// Re-read the active config for every destructive command. This is what
    /// makes an Always Approve write effective without restarting the gateway.
    pub async fn required(&self) -> bool {
        let path = self.config_path.clone();
        tokio::task::spawn_blocking(move || confirmation_required(&path))
            .await
            .unwrap_or(true)
    }

    /// Register a reset, replacing any older prompt for the same stable route.
    /// Registration happens before the prompt is returned, closing the fast
    /// response race that future inline buttons would otherwise expose.
    pub fn register_reset(
        &self,
        route_key: &str,
        title: Option<String>,
        typed_prefix: &str,
    ) -> String {
        let confirm_id = self.next_id.fetch_add(1, Ordering::Relaxed).to_string();
        self.pending.lock().unwrap().insert(
            route_key.to_owned(),
            PendingReset {
                confirm_id,
                command: "new".into(),
                title,
                created_at: Instant::now(),
            },
        );
        confirmation_prompt("new", typed_prefix)
    }

    /// Drop one route's pending prompt at a session boundary without running
    /// it. Automatic expiry/reset must not let an old approval target the new
    /// conversation that inherited the stable route key.
    pub fn clear(&self, route_key: &str) {
        self.pending.lock().unwrap().remove(route_key);
    }

    /// Resolve a typed reply. `allowed` is evaluated against the command that
    /// originally created the prompt, so an unauthorized participant sharing a
    /// group route cannot approve another user's destructive command.
    pub fn resolve_text(
        &self,
        route_key: &str,
        text: &str,
        tool_approval_live: bool,
        allowed: impl FnOnce(&str) -> bool,
    ) -> Resolution {
        let choice = parse_choice(text);
        let mut pending = self.pending.lock().unwrap();
        let Some(entry) = pending.get(route_key) else {
            return Resolution::NotHandled;
        };
        if tool_approval_live || !allowed(&entry.command) {
            return Resolution::NotHandled;
        }
        let Some(choice) = choice else {
            if entry.created_at.elapsed() > self.timeout {
                pending.remove(route_key);
            }
            return Resolution::NotHandled;
        };
        let entry = pending
            .remove(route_key)
            .expect("pending entry disappeared");
        self.finish(entry, choice)
    }

    /// Exact-id resolver for adapter buttons. Platform adapters must pass the
    /// result of their callback authorization check as `allowed`.
    #[allow(dead_code)] // no native button adapter is wired in this checkpoint
    pub fn resolve_by_id(
        &self,
        route_key: &str,
        confirm_id: &str,
        choice: Choice,
        allowed: bool,
    ) -> Resolution {
        if !allowed {
            return Resolution::NotHandled;
        }
        let mut pending = self.pending.lock().unwrap();
        let Some(entry) = pending.get(route_key) else {
            return Resolution::NotHandled;
        };
        if entry.confirm_id != confirm_id {
            return Resolution::NotHandled;
        }
        let entry = pending
            .remove(route_key)
            .expect("pending entry disappeared");
        self.finish(entry, choice)
    }

    fn finish(&self, entry: PendingReset, choice: Choice) -> Resolution {
        if entry.created_at.elapsed() > self.timeout {
            return Resolution::Expired;
        }
        match choice {
            Choice::Cancel => Resolution::Cancelled {
                command: entry.command,
            },
            Choice::Once | Choice::Always => Resolution::Approved(ApprovedReset {
                title: entry.title,
                always: choice == Choice::Always,
            }),
        }
    }

    /// Persist the opt-out with a lossless YAML edit and an atomic, private
    /// replacement. The caller intentionally continues the already-approved
    /// reset if this best-effort preference write fails.
    pub async fn persist_opt_out(&self) -> anyhow::Result<()> {
        let path = self.config_path.clone();
        tokio::task::spawn_blocking(move || persist_opt_out(&path))
            .await
            .map_err(|error| anyhow::anyhow!("config writer task failed: {error}"))?
    }

    #[cfg(test)]
    fn pending_id(&self, route_key: &str) -> Option<String> {
        self.pending
            .lock()
            .unwrap()
            .get(route_key)
            .map(|entry| entry.confirm_id.clone())
    }
}

fn confirmation_required(path: &Path) -> bool {
    let config = crate::config_file::load_config_from(path);
    let Some(approvals) = config
        .get("approvals")
        .and_then(serde_json::Value::as_object)
    else {
        return true;
    };
    approvals
        .get("destructive_slash_confirm")
        .map(crate::python_value::truthy)
        .unwrap_or(true)
}

fn parse_choice(text: &str) -> Option<Choice> {
    let raw = text.trim();
    let normalized = raw.trim_start_matches(['!', '/']).to_ascii_lowercase();
    // MessageEvent.get_command() only recognizes slash-prefixed input. Native
    // Slack has already rewritten registry-known bang commands before this
    // point; the remaining bang forms only participate in the raw fallback.
    let command = if raw.starts_with('/') {
        normalized.split_whitespace().next().unwrap_or("")
    } else {
        ""
    };
    if matches!(command, "approve" | "yes" | "ok" | "confirm")
        || matches!(normalized.as_str(), "approve" | "approve once" | "once")
    {
        Some(Choice::Once)
    } else if matches!(command, "always" | "remember")
        || matches!(normalized.as_str(), "always" | "always approve")
    {
        Some(Choice::Always)
    } else if matches!(command, "cancel" | "no" | "deny" | "nevermind")
        || matches!(normalized.as_str(), "cancel" | "nevermind" | "no")
    {
        Some(Choice::Cancel)
    } else {
        None
    }
}

fn confirmation_prompt(command: &str, prefix: &str) -> String {
    format!(
        "⚠️ **Confirm /{command}**\n\n\
         This starts a fresh session and discards the current conversation history.\n\n\
         Choose:\n\
         • **Approve Once** - proceed this time only\n\
         • **Always Approve** - proceed and silence this prompt permanently\n\
         • **Cancel** - keep current conversation\n\n\
         _Text fallback: reply `{prefix}approve`, `{prefix}always`, or `{prefix}cancel`._"
    )
}

fn persist_opt_out(path: &Path) -> anyhow::Result<()> {
    static CONFIG_WRITE_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    let _guard = CONFIG_WRITE_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap();

    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(error.into()),
    };
    let mut document_text = text.as_str();
    if !text.trim().is_empty() {
        let parsed: serde_json::Value = serde_yaml_ng::from_str(&text)
            .map_err(|error| anyhow::anyhow!("invalid config.yaml: {error}"))?;
        anyhow::ensure!(
            parsed.is_object() || parsed.is_null(),
            "config.yaml top level is not a mapping"
        );
        if parsed.is_null() {
            document_text = "";
        }
    }

    let document =
        YamlFile::from_str(document_text).map_err(|error| anyhow::anyhow!(error.to_string()))?;
    let first = document.ensure_document();
    anyhow::ensure!(
        first.as_mapping().is_some(),
        "config.yaml top level is not a mapping"
    );
    first
        .try_set_path("approvals.destructive_slash_confirm", false)
        .map_err(|error| anyhow::anyhow!(error.to_string()))?;
    let rendered = document.to_string();
    let check: serde_json::Value = serde_yaml_ng::from_str(&rendered)
        .map_err(|error| anyhow::anyhow!("edited config.yaml is invalid: {error}"))?;
    anyhow::ensure!(
        check["approvals"]["destructive_slash_confirm"] == serde_json::Value::Bool(false),
        "edited config.yaml did not contain the requested setting"
    );
    crate::atomic_file::write_private_preserving_symlink(path, rendered.as_bytes())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(label: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "hermes-slash-confirm-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    #[tokio::test]
    async fn config_is_default_on_and_reloaded_after_opt_out() {
        let dir = temp_dir("dynamic");
        let path = dir.join("config.yaml");
        let manager = SlashConfirmations::new(path.clone());
        assert!(manager.required().await);
        manager.persist_opt_out().await.unwrap();
        assert!(!manager.required().await);
        assert_eq!(
            crate::config_file::load_config_from(&path)["approvals"]["destructive_slash_confirm"],
            false
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn null_config_is_treated_as_an_empty_mapping() {
        let dir = temp_dir("null");
        let path = dir.join("config.yaml");
        std::fs::write(&path, "null\n").unwrap();
        let manager = SlashConfirmations::new(path.clone());
        assert!(manager.required().await);
        manager.persist_opt_out().await.unwrap();
        assert!(!manager.required().await);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn config_gate_uses_python_truthiness() {
        let dir = temp_dir("truthiness");
        let path = dir.join("config.yaml");
        let manager = SlashConfirmations::new(path.clone());
        for value in ["false", "0", "''", "null"] {
            std::fs::write(
                &path,
                format!("approvals:\n  destructive_slash_confirm: {value}\n"),
            )
            .unwrap();
            assert!(!manager.required().await, "{value}");
        }
        for value in ["true", "1", "enabled"] {
            std::fs::write(
                &path,
                format!("approvals:\n  destructive_slash_confirm: {value}\n"),
            )
            .unwrap();
            assert!(manager.required().await, "{value}");
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn text_forms_match_the_python_intercept() {
        for text in [
            "/approve extra",
            "/yes",
            "/ok",
            "/confirm",
            "approve",
            "approve once",
            "once",
            "!approve",
        ] {
            assert_eq!(parse_choice(text), Some(Choice::Once), "{text}");
        }
        for text in [
            "/always extra",
            "/remember",
            "always",
            "always approve",
            "!always",
        ] {
            assert_eq!(parse_choice(text), Some(Choice::Always), "{text}");
        }
        for text in [
            "/cancel extra",
            "/no",
            "/deny",
            "/nevermind",
            "cancel",
            "nevermind",
            "no",
            "!cancel",
        ] {
            assert_eq!(parse_choice(text), Some(Choice::Cancel), "{text}");
        }
        for text in [
            "yes",
            "remember",
            "deny",
            "!yes",
            "!ok",
            "!confirm",
            "!remember",
            "approve later",
            "/unknown",
        ] {
            assert_eq!(parse_choice(text), None, "{text}");
        }
    }

    #[test]
    fn registration_supersedes_and_exact_id_resolution_is_once_only() {
        let manager = SlashConfirmations::new(PathBuf::from("unused"));
        let first = manager.register_reset("route", Some("first".into()), "/");
        assert!(first.contains("`/approve`"));
        let old_id = manager.pending_id("route").unwrap();
        let second = manager.register_reset("route", Some("second".into()), "!");
        assert!(second.contains("`!always`"));
        let current_id = manager.pending_id("route").unwrap();
        assert_ne!(old_id, current_id);
        assert_eq!(
            manager.resolve_by_id("route", &old_id, Choice::Once, true),
            Resolution::NotHandled
        );
        assert_eq!(
            manager.resolve_by_id("route", &current_id, Choice::Always, true),
            Resolution::Approved(ApprovedReset {
                title: Some("second".into()),
                always: true,
            })
        );
        assert_eq!(
            manager.resolve_by_id("route", &current_id, Choice::Always, true),
            Resolution::NotHandled
        );
    }

    #[test]
    fn concurrent_replies_approve_exactly_once() {
        let manager = std::sync::Arc::new(SlashConfirmations::new(PathBuf::from("unused")));
        manager.register_reset("route", None, "/");
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(8));
        let threads = (0..8)
            .map(|_| {
                let manager = manager.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    manager.resolve_text("route", "/approve", false, |_| true)
                })
            })
            .collect::<Vec<_>>();
        let approved = threads
            .into_iter()
            .map(|thread| thread.join().unwrap())
            .filter(|resolution| matches!(resolution, Resolution::Approved(_)))
            .count();
        assert_eq!(approved, 1);
    }

    #[test]
    fn authorization_and_tool_approval_precedence_do_not_consume() {
        let manager = SlashConfirmations::new(PathBuf::from("unused"));
        manager.register_reset("route", None, "/");
        assert_eq!(
            manager.resolve_text("route", "/approve", false, |_| false),
            Resolution::NotHandled
        );
        assert_eq!(
            manager.resolve_text("route", "/approve", true, |_| true),
            Resolution::NotHandled
        );
        assert_eq!(
            manager.resolve_text("route", "/approve", false, |command| command == "new"),
            Resolution::Approved(ApprovedReset {
                title: None,
                always: false,
            })
        );
    }

    #[test]
    fn session_boundary_clear_is_route_scoped() {
        let manager = SlashConfirmations::new(PathBuf::from("unused"));
        manager.register_reset("target", None, "/");
        manager.register_reset("other", None, "/");
        manager.clear("target");
        assert!(manager.pending_id("target").is_none());
        assert!(manager.pending_id("other").is_some());
    }

    #[test]
    fn unrelated_text_only_clears_an_expired_prompt() {
        let manager =
            SlashConfirmations::with_timeout(PathBuf::from("unused"), Duration::from_millis(1));
        manager.register_reset("route", None, "/");
        assert_eq!(
            manager.resolve_text("route", "keep chatting", false, |_| true),
            Resolution::NotHandled
        );
        assert!(manager.pending_id("route").is_some());
        std::thread::sleep(Duration::from_millis(3));
        assert_eq!(
            manager.resolve_text("route", "keep chatting", false, |_| true),
            Resolution::NotHandled
        );
        assert!(manager.pending_id("route").is_none());
    }

    #[test]
    fn recognized_expired_reply_is_consumed_without_action() {
        let manager =
            SlashConfirmations::with_timeout(PathBuf::from("unused"), Duration::from_millis(1));
        manager.register_reset("route", None, "/");
        std::thread::sleep(Duration::from_millis(3));
        assert_eq!(
            manager.resolve_text("route", "/approve", false, |_| true),
            Resolution::Expired
        );
        assert!(manager.pending_id("route").is_none());
    }

    #[test]
    fn lossless_edit_preserves_unrelated_layout_and_comments() {
        let dir = temp_dir("lossless");
        let path = dir.join("config.yaml");
        let original = "# operator note\nmodel:\n  default: 'kept-quoted' # model note\napprovals:\n  destructive_slash_confirm: true # gate note\n  other: yes\n";
        std::fs::write(&path, original).unwrap();
        persist_opt_out(&path).unwrap();
        let edited = std::fs::read_to_string(&path).unwrap();
        assert!(edited.contains("# operator note"));
        assert!(edited.contains("default: 'kept-quoted' # model note"));
        assert!(edited.contains("destructive_slash_confirm: false # gate note"));
        assert!(edited.contains("other: yes"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn invalid_config_is_never_overwritten() {
        let dir = temp_dir("invalid");
        let path = dir.join("config.yaml");
        let original = "model: [unterminated\n";
        std::fs::write(&path, original).unwrap();
        assert!(persist_opt_out(&path).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn config_symlink_survives_atomic_update() {
        use std::os::unix::fs::symlink;

        let dir = temp_dir("symlink");
        let target = dir.join("managed.yaml");
        let link = dir.join("config.yaml");
        std::fs::write(&target, "display:\n  skin: mono\n").unwrap();
        symlink(&target, &link).unwrap();
        persist_opt_out(&link).unwrap();
        assert!(std::fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(
            crate::config_file::load_config_from(&target)["approvals"]["destructive_slash_confirm"],
            false
        );
        std::fs::remove_dir_all(dir).unwrap();
    }
}
