//! Session-bound native local foreground terminal tool.
//!
//! The provider schema is immutable for the conversation. Runtime state behind
//! it serializes calls, persists cwd and exported variables, bounds output while
//! streaming, and never weakens the unconditional command-security floor.

use std::collections::{BTreeMap, HashMap};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use hermes_core::Result;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tokio::sync::Mutex;

use crate::foreground_exec::{ForegroundCommand, Outcome, OutputBounds, SpillConfig};
use crate::native_tools::{Tool, ToolSpec};

const MAX_FOREGROUND_TIMEOUT: u64 = 600;
const OUTPUT_HEAD_BYTES: usize = 40_000;
const OUTPUT_TAIL_BYTES: usize = 60_000;
const SPILL_MAX_BYTES: usize = 16 * 1024 * 1024;

#[derive(Clone)]
struct RouteCwd {
    database: Arc<crate::session_db::SessionDb>,
    scope: String,
    session_key: String,
}

/// One terminal instance belongs to one frozen conversation client.
pub struct TerminalTool {
    cwd: Mutex<PathBuf>,
    environment: BTreeMap<OsString, OsString>,
    profile_home: PathBuf,
    snapshot_path: PathBuf,
    spill_dir: PathBuf,
    shell: OsString,
    default_timeout: u64,
    route_cwd: Option<RouteCwd>,
}

pub struct TerminalConfig<'a> {
    pub cwd: PathBuf,
    pub profile_env: &'a HashMap<String, String>,
    pub profile_home: &'a Path,
    pub session_identity: &'a str,
    pub default_timeout: u64,
    pub database: Option<Arc<crate::session_db::SessionDb>>,
    pub route: Option<(String, String)>,
}

impl TerminalTool {
    pub fn new(config: TerminalConfig<'_>) -> Self {
        let identity = format!(
            "{}\0{}",
            config.profile_home.display(),
            config.session_identity
        );
        let digest = format!("{:x}", Sha256::digest(identity.as_bytes()));
        let state_dir = config.profile_home.join("cache/native-terminal");
        let route_cwd =
            config
                .database
                .zip(config.route)
                .map(|(database, (scope, session_key))| RouteCwd {
                    database,
                    scope,
                    session_key,
                });
        Self {
            cwd: Mutex::new(config.cwd),
            environment: crate::secret_scope::isolated_profile_environment(
                config.profile_env,
                config.profile_home,
            ),
            profile_home: config.profile_home.to_path_buf(),
            snapshot_path: state_dir.join(format!("{digest}.env.sh")),
            spill_dir: state_dir.join("output"),
            shell: if Path::new("/bin/bash").is_file() {
                "/bin/bash".into()
            } else {
                "bash".into()
            },
            default_timeout: config.default_timeout.clamp(1, MAX_FOREGROUND_TIMEOUT),
            route_cwd,
        }
    }

    async fn invoke(&self, args: &Value) -> Value {
        let Some(args) = args.as_object() else {
            return error_only("Invalid terminal arguments: expected an object.");
        };
        if !args.contains_key("command") && args.contains_key("code") {
            return error_only(
                "terminal received a 'code' parameter, but it requires a shell command in 'command'. Use execute_code(code=...) for Python; for shell, retry as terminal(command=...).",
            );
        }
        let command = match args.get("command") {
            Some(Value::String(command)) => command,
            Some(value) => return invalid_command(value),
            None => return invalid_command(&Value::Null),
        };
        let background = args.get("background").and_then(Value::as_bool) == Some(true);
        if !background
            && (args.get("notify").is_some_and(crate::python_value::truthy)
                || args
                    .get("notify_on_complete")
                    .is_some_and(crate::python_value::truthy)
                || args
                    .get("watch_patterns")
                    .is_some_and(crate::python_value::truthy))
        {
            return error_only(
                "notify only applies to background commands (foreground results return directly). Either drop notify, or run as terminal(command=..., background=true, notify=...).",
            );
        }
        if !background && args.get("pty").and_then(Value::as_bool) == Some(true) {
            return error_only(
                "pty requires background=true (a PTY session is interacted with via process(action='write'/'submit'), which needs a tracked background process). Retry as terminal(command=..., background=true, pty=true).",
            );
        }
        if args
            .get("notify")
            .is_some_and(|notify| !matches!(notify, Value::Bool(_) | Value::Array(_) | Value::Null))
        {
            return error_only(
                "notify must be true/false (notify on exit) or a list of strings (notify on output pattern match).",
            );
        }
        if background {
            return execution_error(
                "Native terminal currently supports foreground commands only. Retry without background=true.",
                "error",
            );
        }
        let timeout = match args.get("timeout") {
            None | Some(Value::Null) => self.default_timeout,
            Some(value) => match value.as_i64() {
                Some(value) if value > 0 => value as u64,
                Some(value) => {
                    return error_only(&format!(
                        "timeout must be a positive number of seconds (got {value})."
                    ));
                }
                None => return error_only("timeout must be a positive integer number of seconds."),
            },
        };
        if timeout > MAX_FOREGROUND_TIMEOUT {
            return error_only(&format!(
                "Foreground timeout {timeout}s exceeds the maximum of {MAX_FOREGROUND_TIMEOUT}s. Use background=true with notify_on_complete=true for long-running commands."
            ));
        }
        if let Some(guidance) = foreground_guidance(command) {
            return execution_error(guidance, "error");
        }
        if let Some(block) = crate::terminal_guard::unconditional_block(
            command,
            self.environment
                .contains_key(&OsString::from("SUDO_PASSWORD")),
        ) {
            return execution_error(
                &format!(
                    "BLOCKED (hardline): {}. This command is on the unconditional blocklist and cannot be executed by the agent.",
                    block.description
                ),
                "blocked",
            );
        }

        let mut session_cwd = self.cwd.lock().await;
        let user_home = self
            .environment
            .get(&OsString::from("HOME"))
            .map(PathBuf::from);
        let explicit_workdir = match args.get("workdir") {
            None | Some(Value::Null) => None,
            Some(Value::String(path)) => {
                match resolve_workdir(path, &session_cwd, user_home.as_deref()) {
                    Ok(path) => Some(path),
                    Err(error) => return execution_error(&error, "blocked"),
                }
            }
            Some(_) => {
                return execution_error("workdir must be a filesystem path string.", "blocked")
            }
        };
        let command_cwd = explicit_workdir.as_ref().unwrap_or(&session_cwd).clone();
        if let Err(error) = prepare_private_parent(&self.snapshot_path) {
            return error_only(&format!("Failed to prepare terminal state: {error}"));
        }
        let marker = marker(command);
        let wrapper = terminal_wrapper();
        let outcome = crate::foreground_exec::run(ForegroundCommand {
            program: self.shell.clone(),
            args: vec![
                "--noprofile".into(),
                "--norc".into(),
                "-c".into(),
                wrapper.into(),
                "hermes-terminal".into(),
                command.into(),
                marker.clone().into(),
                self.snapshot_path.as_os_str().into(),
                self.profile_home.as_os_str().into(),
            ],
            cwd: command_cwd.clone(),
            env: self.environment.clone(),
            timeout: Duration::from_secs(timeout),
            bounds: OutputBounds {
                head_bytes: OUTPUT_HEAD_BYTES,
                tail_bytes: OUTPUT_TAIL_BYTES,
            },
            spill: Some(SpillConfig {
                dir: self.spill_dir.clone(),
                max_bytes: SPILL_MAX_BYTES,
            }),
        })
        .await;

        match outcome {
            Outcome::Exited(exit) => {
                let (output, observed_cwd) = split_cwd_marker(&exit.stdout.text, &marker);
                let output = if exit.stderr.text.is_empty() {
                    output.to_owned()
                } else {
                    format!("{output}\n{}", exit.stderr.text)
                };
                discard_spill(exit.stderr.spill_path.as_deref());
                let output = redact_output(output.trim());
                let mut result = json!({
                    "output": output,
                    "exit_code": exit.code.unwrap_or(if exit.success { 0 } else { -1 }),
                    "error": Value::Null,
                });
                if explicit_workdir.is_none() {
                    if let Some(observed) = observed_cwd.and_then(valid_observed_cwd) {
                        if observed != command_cwd {
                            result["cwd"] = json!(observed.to_string_lossy());
                        }
                        *session_cwd = observed.clone();
                        if let Some(route) = &self.route_cwd {
                            if let Err(error) = route.database.update_routed_session_cwd(
                                &route.scope,
                                &route.session_key,
                                &observed.to_string_lossy(),
                            ) {
                                tracing::warn!(%error, "native terminal cwd persistence failed");
                            }
                        }
                    }
                }
                if let Some(path) = exit.stdout.spill_path {
                    if redact_spill(&path, &marker) {
                        result["output_total_bytes"] = json!(exit.stdout.total_bytes);
                        result["full_output_path"] = json!(path.to_string_lossy());
                        result["truncation_note"] = json!(format!(
                            "Output exceeded the capture window (head and tail shown). Full bounded spill saved to {}.",
                            path.display()
                        ));
                    }
                }
                result
            }
            Outcome::TimedOut(timed) => {
                discard_spill(timed.stdout.spill_path.as_deref());
                discard_spill(timed.stderr.spill_path.as_deref());
                let partial = redact_output(timed.stdout.text.trim());
                json!({
                    "output": partial,
                    "exit_code": 124,
                    "error": format!("Command timed out after {timeout} seconds"),
                })
            }
            Outcome::SpawnFailed(error) => json!({
                "output": "",
                "exit_code": -1,
                "error": crate::compression_redact::redact(&format!("Command execution failed: {error}")),
                "status": "error",
            }),
        }
    }
}

#[async_trait]
impl Tool for TerminalTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "terminal".into(),
            description: "Execute a foreground shell command in this conversation's local working directory. The working directory and exported environment variables persist between calls.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "command": {"type":"string", "description":"The shell command to execute"},
                    "timeout": {"type":"integer", "minimum":1, "maximum":MAX_FOREGROUND_TIMEOUT, "description":"Maximum seconds to wait. The command returns immediately when it finishes."},
                    "workdir": {"type":"string", "description":"Working directory for this command. Defaults to the session working directory."}
                },
                "required": ["command"],
                "additionalProperties": false
            }),
            extra: Default::default(),
        }
    }

    async fn call(&self, args: &Value) -> Result<Value> {
        Ok(self.invoke(args).await)
    }
}

fn terminal_wrapper() -> &'static str {
    r#"exec 2>&1
snapshot=$3
if [ -f "$snapshot" ]; then . "$snapshot"; fi
export HERMES_HOME=$4
PWD=$(pwd -P)
export PWD
eval "$1"
hermes_status=$?
hermes_cwd=$PWD
umask 077
temporary="${snapshot}.tmp.$$"
if export -p > "$temporary"; then mv -f -- "$temporary" "$snapshot"; else rm -f -- "$temporary"; fi
printf '\n%s%s\n' "$2" "$hermes_cwd"
exit "$hermes_status""#
}

fn marker(command: &str) -> String {
    static SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let sequence = SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let digest = Sha256::digest(format!("{}\0{sequence}\0{command}", std::process::id()));
    format!("__HERMES_CWD_{digest:x}__=")
}

fn split_cwd_marker<'a>(output: &'a str, marker: &str) -> (&'a str, Option<PathBuf>) {
    let Some(index) = output.rfind(marker) else {
        return (output, None);
    };
    let suffix = &output[index + marker.len()..];
    let cwd = suffix.lines().next().filter(|line| !line.is_empty());
    if suffix.lines().count() != 1 {
        return (output, None);
    }
    (output[..index].trim_end(), cwd.map(PathBuf::from))
}

fn valid_observed_cwd(path: PathBuf) -> Option<PathBuf> {
    path.is_absolute()
        .then_some(path)
        .filter(|path| path.is_dir())
}

fn resolve_workdir(
    raw: &str,
    current: &Path,
    user_home: Option<&Path>,
) -> std::result::Result<PathBuf, String> {
    validate_workdir_syntax(raw)?;
    let path = if raw == "~" {
        user_home
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from(raw))
    } else if let Some(rest) = raw.strip_prefix("~/") {
        user_home
            .map(|home| home.join(rest))
            .unwrap_or_else(|| PathBuf::from(raw))
    } else {
        PathBuf::from(raw)
    };
    let path = if path.is_absolute() {
        path
    } else {
        current.join(path)
    };
    let canonical = std::fs::canonicalize(&path)
        .map_err(|error| format!("Working directory is unavailable: {error}"))?;
    if !canonical.is_dir() {
        return Err(format!(
            "Working directory is not a directory: {}",
            canonical.display()
        ));
    }
    Ok(canonical)
}

fn validate_workdir_syntax(raw: &str) -> std::result::Result<(), String> {
    if let Some(ch) = raw.chars().find(|ch| !safe_workdir_char(*ch)) {
        Err(format!(
            "Blocked: workdir contains disallowed character {ch:?}. Use a simple filesystem path without shell metacharacters."
        ))
    } else {
        Ok(())
    }
}

fn safe_workdir_char(ch: char) -> bool {
    !ch.is_control()
        && (ch.is_alphanumeric()
            || matches!(
                ch,
                '/' | '\\' | ':' | '_' | '-' | '.' | '~' | ' ' | '+' | '@' | '=' | ','
            ))
}

fn foreground_guidance(command: &str) -> Option<&'static str> {
    let normalized = command.to_ascii_lowercase();
    if normalized.contains(" --help")
        || normalized.ends_with(" -h")
        || normalized.contains(" --version")
        || normalized.ends_with(" -v")
    {
        return None;
    }
    let unquoted = strip_quoted(&normalized);
    if ["nohup ", "disown", "setsid "]
        .iter()
        .any(|needle| unquoted.contains(needle))
    {
        return Some(
            "Foreground command uses shell-level background wrappers (nohup/disown/setsid). Use the managed background terminal path instead.",
        );
    }
    if unquoted.trim_end().ends_with('&') || unquoted.contains(" & ") {
        return Some(
            "Foreground command uses '&' backgrounding. Use the managed background terminal path instead.",
        );
    }
    let long_lived = [
        "python -m http.server",
        "python3 -m http.server",
        "npm run dev",
        "npm start",
        "yarn dev",
        " vite",
        "uvicorn",
        "gunicorn",
        "nodemon",
    ];
    long_lived
        .iter()
        .any(|needle| unquoted.contains(needle) || unquoted.trim_start().starts_with(needle.trim()))
        .then_some(
            "This foreground command appears to start a long-lived server/watch process. Use the managed background terminal path instead.",
        )
}

fn strip_quoted(text: &str) -> String {
    let mut output = String::with_capacity(text.len());
    let mut quote = None;
    for ch in text.chars() {
        if matches!(ch, '\'' | '"') {
            quote = if quote == Some(ch) {
                None
            } else if quote.is_none() {
                Some(ch)
            } else {
                quote
            };
            output.push(' ');
        } else if quote.is_some() {
            output.push(' ');
        } else {
            output.push(ch);
        }
    }
    output
}

fn error_only(message: &str) -> Value {
    json!({"error": message})
}

fn invalid_command(value: &Value) -> Value {
    let kind = match value {
        Value::Null => "NoneType",
        Value::Bool(_) => "bool",
        Value::Number(number) if number.is_i64() || number.is_u64() => "int",
        Value::Number(_) => "float",
        Value::String(_) => "str",
        Value::Array(_) => "list",
        Value::Object(_) => "dict",
    };
    json!({
        "output": "",
        "exit_code": -1,
        "error": format!("Invalid command: expected string, got {kind}"),
        "status": "error",
    })
}

fn execution_error(message: &str, status: &str) -> Value {
    json!({"output":"", "exit_code":-1, "error":message, "status":status})
}

fn prepare_private_parent(path: &Path) -> std::io::Result<()> {
    let parent = path.parent().expect("snapshot path has a parent");
    std::fs::create_dir_all(parent)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

fn redact_output(output: &str) -> String {
    crate::compression_redact::redact(&strip_ansi(output))
}

fn redact_spill(path: &Path, marker: &str) -> bool {
    let result = (|| -> std::io::Result<()> {
        let metadata = std::fs::symlink_metadata(path)?;
        if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
            return Err(std::io::Error::other("spill path is not a regular file"));
        }
        let raw = std::fs::read_to_string(path)?;
        let (raw, _) = split_cwd_marker(&raw, marker);
        let redacted = redact_output(raw);
        std::fs::remove_file(path)?;
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        use std::io::Write as _;
        options.open(path)?.write_all(redacted.as_bytes())
    })();
    if let Err(error) = result {
        tracing::warn!(%error, path = %path.display(), "native terminal spill redaction failed");
        discard_spill(Some(path));
        false
    } else {
        true
    }
}

fn discard_spill(path: Option<&Path>) {
    if let Some(path) = path {
        let _ = std::fs::remove_file(path);
    }
}

fn strip_ansi(text: &str) -> String {
    let mut output = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch != '\u{1b}' {
            output.push(ch);
            continue;
        }
        match chars.next() {
            Some('[') => {
                for next in chars.by_ref() {
                    if ('@'..='~').contains(&next) {
                        break;
                    }
                }
            }
            Some(']') => {
                let mut escaped = false;
                for next in chars.by_ref() {
                    if next == '\u{7}' || escaped && next == '\\' {
                        break;
                    }
                    escaped = next == '\u{1b}';
                }
            }
            Some(_) | None => {}
        }
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    fn corpus() -> Value {
        serde_json::from_str(include_str!(
            "../../../tools/terminal-contract-goldens.json"
        ))
        .unwrap()
    }

    fn temp_dir(tag: &str) -> PathBuf {
        static SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let sequence = SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "hermes-native-terminal-{tag}-{}-{sequence}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn tool(home: &Path, cwd: &Path) -> TerminalTool {
        let profile = HashMap::from([("PROFILE_SECRET".into(), "only-this-profile".into())]);
        TerminalTool::new(TerminalConfig {
            cwd: cwd.to_path_buf(),
            profile_env: &profile,
            profile_home: home,
            session_identity: "test-session",
            default_timeout: 5,
            database: None,
            route: None,
        })
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn foreground_calls_persist_cwd_and_exports_without_ambient_secrets() {
        let home = temp_dir("state");
        let child = home.join("child");
        std::fs::create_dir(&child).unwrap();
        let terminal = tool(&home, &home);
        let first = terminal
            .invoke(&json!({"command":"cd child && export SAVED=value && printf '%s' \"$PROFILE_SECRET\""}))
            .await;
        assert_eq!(first["exit_code"], 0);
        assert_eq!(first["output"], "only-this-profile");
        assert_eq!(first["cwd"], child.to_string_lossy().as_ref());
        let second = terminal
            .invoke(&json!({"command":"printf '%s:%s' \"$PWD\" \"$SAVED\""}))
            .await;
        assert_eq!(second["exit_code"], 0);
        assert_eq!(second["output"], format!("{}:value", child.display()));
        std::fs::remove_dir_all(home).unwrap();
    }

    #[tokio::test]
    async fn blocks_hardline_and_sudo_guessing_before_execution() {
        let home = temp_dir("guard");
        let terminal = tool(&home, &home);
        for command in ["rm -rf /", "echo guessed | sudo -S id"] {
            let result = terminal.invoke(&json!({"command":command})).await;
            assert_eq!(result["status"], "blocked");
            assert_eq!(result["exit_code"], -1);
        }
        std::fs::remove_dir_all(home).unwrap();
    }

    #[tokio::test]
    async fn provider_rejections_match_python_contract() {
        let home = temp_dir("provider-contract");
        let terminal = tool(&home, &home);
        for case in corpus()["provider_argument_normalization"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|case| case["status"] == "rejected")
        {
            let actual = terminal.invoke(&case["input_args"]).await;
            assert_eq!(
                actual["error"], case["rejection_envelope"]["error"],
                "{}",
                case["id"]
            );
        }
        std::fs::remove_dir_all(home).unwrap();
    }

    #[tokio::test]
    async fn command_and_timeout_rejections_match_python_contract() {
        let home = temp_dir("core-contract");
        let terminal = tool(&home, &home);
        for case in corpus()["core_parameter_validation"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|case| {
                case["id"]
                    .as_str()
                    .is_some_and(|id| id.starts_with("command_") || id.starts_with("timeout_"))
                    && case["terminal_tool_envelope"]["error"]
                        .as_str()
                        .is_some_and(|error| !error.is_empty())
            })
        {
            let actual = terminal
                .invoke(&json!({
                    "command": case["command"].clone(),
                    "background": case["background"].clone(),
                    "timeout": case["timeout"].clone(),
                }))
                .await;
            assert_eq!(
                actual["error"], case["terminal_tool_envelope"]["error"],
                "{}",
                case["id"]
            );
        }
        std::fs::remove_dir_all(home).unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn timeout_keeps_partial_output_and_kills_the_group() {
        let home = temp_dir("timeout");
        let terminal = tool(&home, &home);
        let result = terminal
            .invoke(&json!({"command":"printf partial; sleep 30", "timeout":1}))
            .await;
        assert_eq!(result["exit_code"], 124);
        assert_eq!(result["output"], "partial");
        std::fs::remove_dir_all(home).unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn overflow_is_bounded_and_private_spill_is_redacted() {
        use std::os::unix::fs::PermissionsExt;

        let home = temp_dir("overflow");
        let terminal = tool(&home, &home);
        let result = terminal
            .invoke(&json!({
                "command":"printf 'OPENAI_API_KEY=sk-secretvalue123456\\n'; head -c 120000 /dev/zero | tr '\\0' x"
            }))
            .await;
        assert_eq!(result["exit_code"], 0);
        assert!(!result["output"].as_str().unwrap().contains("secretvalue"));
        assert!(result["output"].as_str().unwrap().len() < 101_000);
        let path = PathBuf::from(result["full_output_path"].as_str().unwrap());
        let full = std::fs::read_to_string(&path).unwrap();
        assert!(!full.contains("secretvalue"));
        assert!(!full.contains("__HERMES_CWD_"));
        assert_eq!(
            std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        std::fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn strips_terminal_escape_sequences() {
        assert_eq!(strip_ansi("a\u{1b}[31mred\u{1b}[0m b"), "ared b");
    }

    #[test]
    fn workdir_allowlist_matches_python_contract() {
        for case in corpus()["workdir_validation"].as_array().unwrap() {
            let raw = case["workdir"].as_str().unwrap_or("");
            assert_eq!(
                validate_workdir_syntax(raw).is_ok(),
                case["is_safe"].as_bool().unwrap(),
                "{}",
                case["id"]
            );
        }
    }

    #[test]
    fn workdir_resolution_expands_tilde_and_resolves_relative_paths() {
        let home = temp_dir("workdir-resolution");
        let child = home.join("child");
        std::fs::create_dir(&child).unwrap();
        assert_eq!(resolve_workdir("~", &child, Some(&home)).unwrap(), home);
        assert_eq!(
            resolve_workdir("../child", &child, Some(&home)).unwrap(),
            child
        );
        std::fs::remove_dir_all(home).unwrap();
    }
}
