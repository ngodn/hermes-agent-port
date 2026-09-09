//! Port of gateway/hooks.py, adapted to a compiled runtime.
//!
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
//! emit / emit_collect contract identical. Errors are logged and never block the
//! pipeline.
//!
//! Profile isolation. When a [`HookRuntime`] is installed on the registry (the
//! production path for a profile-scoped gateway), each handler subprocess starts
//! from an environment cleared of foreign profile secrets: only genuinely-global
//! names (see [`crate::secret_scope::is_global_env`]) are re-inherited, the
//! prepared profile environment is layered on, and `HERMES_HOME` is forced to
//! the selected profile home. In that mode a Python `handler.py` runs through
//! `python -m hermes_cli.rust_hook_runner <handler-path>` (so a function-only
//! handler needs no `if __name__ == "__main__":` shim); executable, shell, and
//! JavaScript handlers keep their direct modes. With no runtime installed the
//! registry retains its earlier inherited-environment behavior for isolated
//! tests and compatibility callers.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::Value;

/// Upper bound on how long a single handler subprocess may run before it is
/// killed. Hooks are fire-and-forget observers, so a hung handler must never
/// wedge the lifecycle event that fired it.
const HANDLER_TIMEOUT: Duration = Duration::from_secs(30);

/// Metadata about a loaded hook (for listing).
#[derive(Debug, Clone, PartialEq)]
pub struct HookMeta {
    pub name: String,
    pub description: String,
    pub events: Vec<String>,
    pub path: String,
}

/// Profile-scoped runtime for executing hook handler subprocesses.
///
/// Carries everything a handler needs to run under exactly one profile's
/// identity: the selected profile home, the prepared per-profile environment,
/// the Python interpreter for `handler.py` handlers, and the Hermes repository
/// root (so `python -m hermes_cli.rust_hook_runner` resolves). When installed on
/// a [`HookRegistry`], it drives the environment isolation described in the
/// module docs.
#[derive(Debug, Clone)]
pub struct HookRuntime {
    /// The selected profile's `HERMES_HOME`. Forced into every handler's env as
    /// the last word on profile identity.
    pub profile_home: PathBuf,
    /// The prepared profile environment (provider keys and per-profile settings)
    /// layered on top of the re-inherited global names.
    pub profile_env: HashMap<String, String>,
    /// Python interpreter used to run `handler.py` handlers through the runner.
    pub python: String,
    /// Hermes repository root, used as the working directory so
    /// `-m hermes_cli.rust_hook_runner` resolves.
    pub repo_root: PathBuf,
}

/// Discovers, loads, and fires event hooks.
#[derive(Default)]
pub struct HookRegistry {
    /// event_type -> handler executables
    handlers: HashMap<String, Vec<PathBuf>>,
    loaded: Vec<HookMeta>,
    /// Profile-scoped execution context. `None` keeps the plain inherited
    /// environment (the not-yet-wired / test path).
    runtime: Option<HookRuntime>,
}

impl HookRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Install the profile-scoped runtime that isolates handler environments.
    pub fn with_runtime(mut self, runtime: HookRuntime) -> Self {
        self.runtime = Some(runtime);
        self
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
            if let Err(err) = run_handler(
                &handler,
                event_type,
                &context,
                false,
                self.runtime.as_ref(),
                HANDLER_TIMEOUT,
            )
            .await
            {
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
            match run_handler(
                &handler,
                event_type,
                &context,
                true,
                self.runtime.as_ref(),
                HANDLER_TIMEOUT,
            )
            .await
            {
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

/// Build the command to run a handler.
///
/// With a [`HookRuntime`] installed, a Python `handler.py` runs through
/// `python -m hermes_cli.rust_hook_runner <handler-path>` from the repo root, so
/// a function-only handler needs no `__main__` shim. Executable, shell, and
/// JavaScript handlers keep their direct modes in every case. With no runtime,
/// Python falls back to invoking the file directly with a plain interpreter.
fn handler_command(handler: &Path, runtime: Option<&HookRuntime>) -> tokio::process::Command {
    let ext = handler.extension().and_then(|e| e.to_str()).unwrap_or("");

    // Python handlers route through the runner module when a profile runtime is
    // installed. This takes precedence over the executable bit: a handler.py is
    // always driven by the runner in that mode.
    if ext == "py" {
        if let Some(rt) = runtime {
            let mut c = tokio::process::Command::new(&rt.python);
            c.arg("-m")
                .arg("hermes_cli.rust_hook_runner")
                .arg(handler)
                .current_dir(&rt.repo_root);
            return c;
        }
    }

    let is_exec = is_executable(handler);
    if is_exec {
        return tokio::process::Command::new(handler);
    }
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
}

/// Isolate a handler's environment under one profile: clear inherited state,
/// re-inherit only genuinely-global names, layer the prepared profile
/// environment, then force `HERMES_HOME` to the selected profile home.
fn apply_isolated_env(cmd: &mut tokio::process::Command, rt: &HookRuntime) {
    cmd.env_clear();
    for (key, value) in std::env::vars_os() {
        let Some(name) = key.to_str() else {
            continue;
        };
        if crate::secret_scope::is_global_env(name) {
            cmd.env(key, value);
        }
    }
    for (key, value) in &rt.profile_env {
        cmd.env(key, value);
    }
    // HERMES_HOME is the last word on profile identity: a profile_env entry must
    // not be able to point the handler at another profile's home.
    cmd.env("HERMES_HOME", &rt.profile_home);
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
    runtime: Option<&HookRuntime>,
    timeout: Duration,
) -> std::io::Result<Option<Value>> {
    use tokio::io::AsyncWriteExt;

    let mut cmd = handler_command(handler, runtime);
    // Isolate the environment before layering on the always-present markers, so
    // env_clear cannot wipe HERMES_HOOK_EVENT.
    if let Some(rt) = runtime {
        apply_isolated_env(&mut cmd, rt);
    }
    cmd.arg(event_type)
        .env("HERMES_HOOK_EVENT", event_type)
        .stdin(std::process::Stdio::piped())
        .stdout(if collect_output {
            std::process::Stdio::piped()
        } else {
            std::process::Stdio::null()
        })
        .stderr(std::process::Stdio::null())
        // A fire-and-forget event must never leak a hung handler: if we drop the
        // child (on timeout or an early return) the OS process is killed too.
        .kill_on_drop(true);
    #[cfg(unix)]
    cmd.process_group(0);
    #[cfg(windows)]
    cmd.creation_flags(0x08000000);

    let mut child = cmd.spawn()?;
    let pid = child.id();

    // Write the compact JSON context from a detached task so a handler that
    // never reads stdin cannot wedge us on a full pipe buffer.
    if let Some(mut stdin) = child.stdin.take() {
        let payload = serde_json::to_vec(context).unwrap_or_else(|_| b"{}".to_vec());
        tokio::spawn(async move {
            let _ = stdin.write_all(&payload).await;
            let _ = stdin.shutdown().await;
        });
    }

    let output = match tokio::time::timeout(timeout, child.wait_with_output()).await {
        Ok(result) => result?,
        Err(_) => {
            #[cfg(unix)]
            if let Some(pid) = pid {
                // The handler owns this process group, so descendants cannot
                // survive after its bounded observer task is abandoned.
                unsafe {
                    libc::kill(-(pid as i32), libc::SIGKILL);
                }
            }
            #[cfg(windows)]
            if let Some(pid) = pid {
                let mut kill = tokio::process::Command::new("taskkill");
                kill.args(["/T", "/F", "/PID", &pid.to_string()])
                    .stdin(std::process::Stdio::null())
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .kill_on_drop(true)
                    .creation_flags(0x08000000);
                let _ = tokio::time::timeout(Duration::from_secs(2), kill.status()).await;
            }
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "hook handler timed out",
            ));
        }
    };

    if !output.status.success() {
        return Err(std::io::Error::other(format!(
            "hook handler exited with status {}",
            output.status
        )));
    }

    if !collect_output {
        return Ok(None);
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let text = text.trim();
    if text.is_empty() {
        return Ok(None);
    }
    serde_json::from_str::<Value>(text)
        .map(Some)
        .map_err(|error| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("hook handler returned invalid JSON: {error}"),
            )
        })
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

    fn test_runtime(profile_home: &Path) -> HookRuntime {
        HookRuntime {
            profile_home: profile_home.to_path_buf(),
            profile_env: HashMap::new(),
            python: "python3".to_string(),
            repo_root: PathBuf::from("/nonexistent/repo/root"),
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn profile_env_isolation_and_home_identity() {
        // A foreign profile secret sitting in the process environment must not
        // reach the handler; the prepared profile env must; HERMES_HOME must be
        // forced to the selected profile home.
        std::env::set_var("ZZZ_FOREIGN_SECRET", "leaked-value");

        let root = temp_hooks_root();
        write_hook(
            &root,
            "envprobe",
            &["agent:start"],
            "#!/bin/sh\ncat >/dev/null\nprintf '{\"home\":\"%s\",\"marker\":\"%s\",\"leak\":\"%s\"}' \
             \"$HERMES_HOME\" \"${PROFILE_MARKER:-MISSING}\" \"${ZZZ_FOREIGN_SECRET:-MISSING}\"\n",
        );

        let profile_home = root.join("profile-home");
        let mut runtime = test_runtime(&profile_home);
        runtime
            .profile_env
            .insert("PROFILE_MARKER".to_string(), "prof-value".to_string());
        // A profile_env entry must not be able to override the forced home.
        runtime
            .profile_env
            .insert("HERMES_HOME".to_string(), "/should/be/ignored".to_string());

        let mut reg = HookRegistry::new().with_runtime(runtime);
        reg.discover_and_load_from(&root);
        let results = reg.emit_collect("agent:start", None).await;

        assert_eq!(results.len(), 1);
        let out = &results[0];
        assert_eq!(
            out.get("home").and_then(Value::as_str),
            Some(profile_home.to_string_lossy().as_ref()),
            "HERMES_HOME must be forced to the selected profile home"
        );
        assert_eq!(
            out.get("marker").and_then(Value::as_str),
            Some("prof-value"),
            "prepared profile env must be installed"
        );
        assert_eq!(
            out.get("leak").and_then(Value::as_str),
            Some("MISSING"),
            "foreign profile secret must not leak into the handler"
        );

        std::env::remove_var("ZZZ_FOREIGN_SECRET");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn nonzero_exit_isolates_and_subsequent_handlers_run() {
        // Two handlers on the same event: the first prints then exits nonzero,
        // the second exits cleanly. The failing handler's output is dropped and
        // treated as an error, but the second handler still runs.
        let root = temp_hooks_root();
        // Dir names sort so "a_fail" loads (and fires) before "b_ok".
        write_hook(
            &root,
            "a_fail",
            &["agent:start"],
            "#!/bin/sh\ncat >/dev/null\necho '\"should-be-dropped\"'\nexit 3\n",
        );
        write_hook(
            &root,
            "b_ok",
            &["agent:start"],
            "#!/bin/sh\ncat >/dev/null\necho '\"second-ran\"'\n",
        );

        let mut reg = HookRegistry::new();
        reg.discover_and_load_from(&root);
        let results = reg.emit_collect("agent:start", None).await;

        assert_eq!(
            results,
            vec![Value::from("second-ran")],
            "nonzero-exit output is dropped and the next handler still runs"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn malformed_output_isolates_and_subsequent_handlers_run() {
        let root = temp_hooks_root();
        write_hook(
            &root,
            "a_malformed",
            &["agent:start"],
            "#!/bin/sh\ncat >/dev/null\necho 'not-json'\n",
        );
        write_hook(
            &root,
            "b_ok",
            &["agent:start"],
            "#!/bin/sh\ncat >/dev/null\necho '\"second-ran\"'\n",
        );

        let mut reg = HookRegistry::new();
        reg.discover_and_load_from(&root);
        assert_eq!(
            reg.emit_collect("agent:start", None).await,
            vec![Value::from("second-ran")]
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn timeout_kills_hung_handler() {
        // A handler and its background descendants are killed at the timeout.
        let root = temp_hooks_root();
        let dir = root.join("hung");
        std::fs::create_dir_all(&dir).unwrap();
        let marker = dir.join("did-not-die.marker");
        {
            use std::os::unix::fs::PermissionsExt;
            let handler = dir.join("handler.sh");
            std::fs::write(
                &handler,
                format!(
                    "#!/bin/sh\ncat >/dev/null\n(sleep 1; touch '{}') &\nsleep 5\n",
                    marker.display(),
                ),
            )
            .unwrap();
            std::fs::set_permissions(&handler, std::fs::Permissions::from_mode(0o755)).unwrap();

            let ctx = Value::Object(Default::default());
            let err = run_handler(
                &handler,
                "agent:start",
                &ctx,
                false,
                None,
                Duration::from_millis(300),
            )
            .await
            .expect_err("a hung handler must time out");
            assert_eq!(err.kind(), std::io::ErrorKind::TimedOut);
        }

        // Without process-group cleanup, the detached child reaches this touch.
        tokio::time::sleep(Duration::from_millis(1_300)).await;
        assert!(
            !marker.exists(),
            "handler process should have been killed before its side effect"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn python_handler_routes_through_runner_when_runtime_set() {
        // Deterministic check of the runner invocation shape, independent of
        // whether the runner file exists on disk.
        let runtime = HookRuntime {
            profile_home: PathBuf::from("/tmp/home"),
            profile_env: HashMap::new(),
            python: "python3".to_string(),
            repo_root: PathBuf::from("/tmp/repo"),
        };
        let handler = PathBuf::from("/tmp/hooks/py/handler.py");
        let cmd = handler_command(&handler, Some(&runtime));
        let std_cmd = cmd.as_std();
        assert_eq!(std_cmd.get_program(), std::ffi::OsStr::new("python3"));
        let args: Vec<_> = std_cmd.get_args().collect();
        assert_eq!(
            args,
            vec![
                std::ffi::OsStr::new("-m"),
                std::ffi::OsStr::new("hermes_cli.rust_hook_runner"),
                handler.as_os_str(),
            ]
        );
        assert_eq!(
            std_cmd.get_current_dir(),
            Some(Path::new("/tmp/repo")),
            "runner must run from the repo root so -m resolves"
        );
    }

    /// Repo root (the dir containing `hermes_cli/`) relative to this crate.
    fn repo_root() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .ancestors()
            .find(|p| p.join("hermes_cli").is_dir())
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."))
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn python_function_handler_via_runner_when_available() {
        // Executes a function-only handler.py through the runner, but only when
        // the runner module is actually present. Otherwise this is a no-op so the
        // suite stays green until the primary lane lands the runner.
        let root = repo_root();
        if !root.join("hermes_cli/rust_hook_runner.py").is_file() {
            return;
        }

        let hooks = temp_hooks_root();
        let dir = hooks.join("pyfn");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("HOOK.yaml"),
            "name: pyfn\ndescription: py fn hook\nevents:\n  - \"agent:start\"\n",
        )
        .unwrap();
        // A function-only handler: no __main__ shim, the runner drives it.
        std::fs::write(
            dir.join("handler.py"),
            "def handle(event_type, context):\n    return {\"handled\": event_type}\n",
        )
        .unwrap();

        let runtime = HookRuntime {
            profile_home: hooks.join("home"),
            profile_env: HashMap::new(),
            python: "python3".to_string(),
            repo_root: root,
        };
        let mut reg = HookRegistry::new().with_runtime(runtime);
        reg.discover_and_load_from(&hooks);
        let results = reg.emit_collect("agent:start", None).await;
        assert_eq!(results.len(), 1);
        assert_eq!(
            results[0].get("handled").and_then(Value::as_str),
            Some("agent:start")
        );
        let _ = std::fs::remove_dir_all(&hooks);
    }
}
