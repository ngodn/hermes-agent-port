//! Port of gateway/hooks.py, adapted to a compiled runtime.
//!
// Public API is ahead of its callers (the lifecycle emit sites wire it).
#![allow(dead_code)]
//!
//! Event hook system: fire user handlers at key lifecycle points. Hooks live in
//! `$HERMES_HOME/hooks/<name>/`, each with a `HOOK.yaml` (name, description,
//! events) and a handler.
//!
//! Design change from the Python original (a deliberate decision, not a port
//! gap): the Python gateway imported each `handler.py` into its own interpreter
//! and called `handle(event_type, context)` in-process. A compiled Rust binary
//! has no embedded interpreter, so hooks here execute as SUBPROCESSES:
//!
//!   * the handler is resolved as `handler` (any executable), else `handler.py`
//!     / `handler.sh` / `handler.js` run through their interpreter;
//!   * it receives the event type as `argv[1]` and in `HERMES_HOOK_EVENT`, and
//!     the JSON context object on stdin;
//!   * for `emit_collect`, a handler that prints a JSON value on stdout returns
//!     that value (decision-style hooks: allow/deny/rewrite).
//!
//! This keeps HOOK.yaml discovery, wildcard routing (`command:*`), and the
//! emit / emit_collect contract identical. A function-only Python `handler.py`
//! migrates by adding a `if __name__ == "__main__":` entrypoint that reads the
//! context from stdin. Errors are logged and never block the pipeline.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde_json::Value;

/// Metadata about a loaded hook (for listing).
#[derive(Debug, Clone, PartialEq)]
pub struct HookMeta {
    pub name: String,
    pub description: String,
    pub events: Vec<String>,
    pub path: String,
}

/// Discovers, loads, and fires event hooks.
#[derive(Default)]
pub struct HookRegistry {
    /// event_type -> handler executables
    handlers: HashMap<String, Vec<PathBuf>>,
    loaded: Vec<HookMeta>,
}

impl HookRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn loaded_hooks(&self) -> &[HookMeta] {
        &self.loaded
    }

    /// Scan `$HERMES_HOME/hooks/` and load each hook's handler.
    pub fn discover_and_load(&mut self) {
        let dir = crate::config_file::hermes_home().join("hooks");
        self.discover_and_load_from(&dir);
    }

    /// Scan a specific hooks directory (used by tests).
    pub fn discover_and_load_from(&mut self, hooks_dir: &Path) {
        let Ok(entries) = std::fs::read_dir(hooks_dir) else {
            return;
        };
        // Sorted for deterministic load order (mirrors Python's sorted()).
        let mut dirs: Vec<PathBuf> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.is_dir())
            .collect();
        dirs.sort();

        for hook_dir in dirs {
            let manifest_path = hook_dir.join("HOOK.yaml");
            if !manifest_path.exists() {
                continue;
            }
            let Some(handler) = resolve_handler(&hook_dir) else {
                tracing::warn!(dir = %hook_dir.display(), "hook has no handler; skipping");
                continue;
            };
            let Ok(text) = std::fs::read_to_string(&manifest_path) else {
                continue;
            };
            let Ok(Value::Object(manifest)) = serde_yaml_ng::from_str::<Value>(&text) else {
                tracing::warn!(dir = %hook_dir.display(), "invalid HOOK.yaml; skipping");
                continue;
            };
            let default_name = hook_dir
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("")
                .to_string();
            let name = manifest
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or(&default_name)
                .to_string();
            let events: Vec<String> = manifest
                .get("events")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default();
            if events.is_empty() {
                tracing::warn!(hook = %name, "hook declares no events; skipping");
                continue;
            }
            for event in &events {
                self.handlers
                    .entry(event.clone())
                    .or_default()
                    .push(handler.clone());
            }
            self.loaded.push(HookMeta {
                name,
                description: manifest
                    .get("description")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                events,
                path: hook_dir.to_string_lossy().to_string(),
            });
        }
    }

    /// Handlers that fire for `event_type`: exact matches then `base:*` wildcards.
    fn resolve_handlers(&self, event_type: &str) -> Vec<PathBuf> {
        let mut handlers = self.handlers.get(event_type).cloned().unwrap_or_default();
        if let Some((base, _)) = event_type.split_once(':') {
            let wildcard = format!("{base}:*");
            if let Some(extra) = self.handlers.get(&wildcard) {
                handlers.extend(extra.iter().cloned());
            }
        }
        handlers
    }

    /// Fire all handlers for an event, discarding output. Never propagates an
    /// error into the caller.
    pub async fn emit(&self, event_type: &str, context: Option<Value>) {
        let context = context.unwrap_or_else(|| Value::Object(Default::default()));
        for handler in self.resolve_handlers(event_type) {
            if let Err(err) = run_handler(&handler, event_type, &context, false).await {
                tracing::warn!(event = event_type, %err, "hook handler error");
            }
        }
    }

    /// Fire handlers and return their non-null stdout JSON values, in order.
    /// Used for decision-style hooks (allow/deny/rewrite before dispatch).
    pub async fn emit_collect(&self, event_type: &str, context: Option<Value>) -> Vec<Value> {
        let context = context.unwrap_or_else(|| Value::Object(Default::default()));
        let mut results = Vec::new();
        for handler in self.resolve_handlers(event_type) {
            match run_handler(&handler, event_type, &context, true).await {
                Ok(Some(v)) if !v.is_null() => results.push(v),
                Ok(_) => {}
                Err(err) => tracing::warn!(event = event_type, %err, "hook handler error"),
            }
        }
        results
    }
}

/// Resolve the handler executable in a hook dir: prefer a bare `handler`, then a
/// scripted `handler.<ext>`.
fn resolve_handler(hook_dir: &Path) -> Option<PathBuf> {
    for name in ["handler", "handler.py", "handler.sh", "handler.js"] {
        let p = hook_dir.join(name);
        if p.is_file() {
            return Some(p);
        }
    }
    None
}

/// Build the command to run a handler: honor an executable bit, else pick an
/// interpreter by extension.
fn handler_command(handler: &Path) -> tokio::process::Command {
    let ext = handler.extension().and_then(|e| e.to_str()).unwrap_or("");
    let is_exec = is_executable(handler);
    let cmd = if is_exec {
        tokio::process::Command::new(handler)
    } else {
        match ext {
            "py" => {
                let python =
                    std::env::var("HERMES_HOOK_PYTHON").unwrap_or_else(|_| "python3".to_string());
                let mut c = tokio::process::Command::new(python);
                c.arg(handler);
                c
            }
            "js" => {
                let mut c = tokio::process::Command::new("node");
                c.arg(handler);
                c
            }
            "sh" => {
                let mut c = tokio::process::Command::new("sh");
                c.arg(handler);
                c
            }
            _ => tokio::process::Command::new(handler),
        }
    };
    cmd
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    path.metadata()
        .map(|m| m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable(path: &Path) -> bool {
    false
}

/// Run one handler subprocess. Returns the parsed stdout JSON when
/// `collect_output` is set and stdout is non-empty valid JSON.
async fn run_handler(
    handler: &Path,
    event_type: &str,
    context: &Value,
    collect_output: bool,
) -> std::io::Result<Option<Value>> {
    use tokio::io::AsyncWriteExt;

    let mut cmd = handler_command(handler);
    cmd.arg(event_type)
        .env("HERMES_HOOK_EVENT", event_type)
        .stdin(std::process::Stdio::piped())
        .stdout(if collect_output {
            std::process::Stdio::piped()
        } else {
            std::process::Stdio::null()
        })
        .stderr(std::process::Stdio::null());

    let mut child = cmd.spawn()?;
    if let Some(mut stdin) = child.stdin.take() {
        let payload = serde_json::to_vec(context).unwrap_or_else(|_| b"{}".to_vec());
        // Best-effort: a handler that ignores stdin may close it early.
        let _ = stdin.write_all(&payload).await;
        let _ = stdin.shutdown().await;
    }

    let output = child.wait_with_output().await?;
    if !collect_output {
        return Ok(None);
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let text = text.trim();
    if text.is_empty() {
        return Ok(None);
    }
    Ok(serde_json::from_str::<Value>(text).ok())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn temp_hooks_root() -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "hermes_hooks_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[cfg(unix)]
    fn write_hook(root: &Path, name: &str, events: &[&str], script: &str) {
        use std::os::unix::fs::PermissionsExt;
        let dir = root.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        let events_yaml = events
            .iter()
            .map(|e| format!("  - \"{e}\""))
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(
            dir.join("HOOK.yaml"),
            format!("name: {name}\ndescription: test hook\nevents:\n{events_yaml}\n"),
        )
        .unwrap();
        let handler = dir.join("handler.sh");
        let mut f = std::fs::File::create(&handler).unwrap();
        f.write_all(script.as_bytes()).unwrap();
        std::fs::set_permissions(&handler, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn discovers_loads_and_wildcard_routes() {
        let root = temp_hooks_root();
        // An exact-event hook and a wildcard-event hook.
        write_hook(
            &root,
            "exact",
            &["command:reset"],
            "#!/bin/sh\ncat >/dev/null\necho '\"exact-ran\"'\n",
        );
        write_hook(
            &root,
            "wild",
            &["command:*"],
            "#!/bin/sh\ncat >/dev/null\necho '\"wild-ran\"'\n",
        );

        let mut reg = HookRegistry::new();
        reg.discover_and_load_from(&root);
        assert_eq!(reg.loaded_hooks().len(), 2);

        // command:reset fires the exact hook then the wildcard hook.
        let results = reg.emit_collect("command:reset", None).await;
        assert!(results.iter().any(|v| v == "exact-ran"));
        assert!(results.iter().any(|v| v == "wild-ran"));

        // command:other fires only the wildcard.
        let results2 = reg.emit_collect("command:other", None).await;
        assert_eq!(results2, vec![Value::from("wild-ran")]);

        let _ = std::fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn handler_receives_event_and_context() {
        let root = temp_hooks_root();
        // Echo back the event arg and the piped context so we can assert both.
        write_hook(
            &root,
            "echoer",
            &["agent:start"],
            "#!/bin/sh\nctx=$(cat)\nprintf '{\"event\":\"%s\",\"ctx\":%s}' \"$1\" \"$ctx\"\n",
        );
        let mut reg = HookRegistry::new();
        reg.discover_and_load_from(&root);
        let ctx = serde_json::json!({"platform": "telegram"});
        let results = reg.emit_collect("agent:start", Some(ctx)).await;
        assert_eq!(results.len(), 1);
        assert_eq!(
            results[0].get("event").and_then(Value::as_str),
            Some("agent:start")
        );
        assert_eq!(
            results[0].pointer("/ctx/platform").and_then(Value::as_str),
            Some("telegram")
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn base_event_does_not_fire_for_subtype() {
        // A hook registered for the base "agent" must NOT fire for "agent:start"
        // (only exact matches and explicit wildcards).
        let root = temp_hooks_root();
        write_hook(
            &root,
            "baseonly",
            &["agent"],
            "#!/bin/sh\ncat >/dev/null\necho '\"nope\"'\n",
        );
        let mut reg = HookRegistry::new();
        reg.discover_and_load_from(&root);
        let results = reg.emit_collect("agent:start", None).await;
        assert!(results.is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn no_events_hook_is_skipped() {
        let root = temp_hooks_root();
        let dir = root.join("noevents");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("HOOK.yaml"), "name: noevents\n").unwrap();
        std::fs::write(dir.join("handler.sh"), "#!/bin/sh\ntrue\n").unwrap();
        let mut reg = HookRegistry::new();
        reg.discover_and_load_from(&root);
        assert!(reg.loaded_hooks().is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }
}
