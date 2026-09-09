//! Session-bound native local terminal tool.
//!
//! The provider schema is immutable for the conversation. Runtime state behind
//! it serializes calls, persists foreground cwd and exported variables, starts
//! managed non-PTY background work, bounds output while streaming, and never
//! weakens the unconditional command-security floor.

use std::collections::{BTreeMap, HashMap};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock};
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
    process_registry: Arc<crate::background_process::Registry>,
    process_owner: crate::background_process::Owner,
    approval_policy: Mutex<ApprovalPolicy>,
    tool_approvals: Arc<crate::tool_approval::ApprovalBroker>,
    approval_prompt_capable: bool,
}

pub struct TerminalConfig<'a> {
    pub cwd: PathBuf,
    pub profile_env: &'a HashMap<String, String>,
    pub profile_home: &'a Path,
    pub session_identity: &'a str,
    pub default_timeout: u64,
    pub database: Option<Arc<crate::session_db::SessionDb>>,
    pub route: Option<(String, String)>,
    pub process_registry: Arc<crate::background_process::Registry>,
    pub approval_config: Value,
    pub tool_approvals: Arc<crate::tool_approval::ApprovalBroker>,
    pub approval_prompt_capable: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ApprovalMode {
    Off,
    Manual,
    Smart,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ApprovalPolicy {
    mode: ApprovalMode,
    deny: Vec<String>,
    permanent: Vec<String>,
    timeout: Duration,
    tirith_enabled: bool,
}

impl ApprovalPolicy {
    fn from_config(value: &Value) -> Self {
        let approvals = &value["approvals"];
        let deny_is_valid = matches!(
            approvals.get("deny"),
            None | Some(Value::Null | Value::Array(_))
        );
        let mode = if !deny_is_valid {
            ApprovalMode::Smart
        } else {
            match approvals.get("mode") {
                None => ApprovalMode::Smart,
                Some(Value::Bool(false)) => ApprovalMode::Off,
                Some(Value::String(mode)) if mode.trim().eq_ignore_ascii_case("off") => {
                    ApprovalMode::Off
                }
                Some(Value::String(mode)) if mode.trim().eq_ignore_ascii_case("smart") => {
                    ApprovalMode::Smart
                }
                _ => ApprovalMode::Manual,
            }
        };
        let deny = approvals
            .get("deny")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(str::trim)
            .filter(|pattern| !pattern.is_empty())
            .map(str::to_owned)
            .collect();
        let permanent = value["command_allowlist"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(str::trim)
            .filter(|pattern| !pattern.is_empty())
            .map(str::to_owned)
            .collect();
        let timeout = approval_timeout(&approvals["timeout"]);
        Self {
            mode,
            deny,
            permanent,
            timeout,
            tirith_enabled: value["security"]["tirith_enabled"].as_bool() != Some(false),
        }
    }
}

fn approval_timeout(value: &Value) -> Duration {
    const DEFAULT_SECONDS: i128 = 300;
    const MAX_SAFE_SECONDS: i128 = 365 * 24 * 60 * 60;

    let parsed = match value {
        Value::Bool(value) => Some(i128::from(*value)),
        Value::Number(value) => value
            .as_i64()
            .map(i128::from)
            .or_else(|| value.as_u64().map(i128::from))
            .or_else(|| {
                value
                    .as_f64()
                    .filter(|value| value.is_finite())
                    .map(|value| value.trunc() as i128)
            }),
        Value::String(value) => value.trim().parse::<i128>().ok(),
        _ => None,
    }
    .unwrap_or(DEFAULT_SECONDS)
    .clamp(0, MAX_SAFE_SECONDS);
    Duration::from_secs(parsed as u64)
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
            process_registry: config.process_registry,
            process_owner: crate::background_process::Owner::new(
                config.profile_home,
                config.session_identity,
            ),
            approval_policy: Mutex::new(ApprovalPolicy::from_config(&config.approval_config)),
            tool_approvals: config.tool_approvals,
            approval_prompt_capable: config.approval_prompt_capable,
        }
    }

    async fn current_approval_policy(&self) -> ApprovalPolicy {
        let path = self.profile_home.join("config.yaml");
        let mut cached = self.approval_policy.lock().await;
        match tokio::fs::read_to_string(&path).await {
            Ok(text) => match serde_yaml_ng::from_str::<Value>(&text) {
                Ok(config) if config.is_object() => {
                    let policy = ApprovalPolicy::from_config(&config);
                    *cached = policy.clone();
                    policy
                }
                Ok(_) => {
                    tracing::warn!(path = %path.display(), "native terminal ignored non-mapping config reload");
                    cached.clone()
                }
                Err(error) => {
                    tracing::warn!(%error, path = %path.display(), "native terminal retained last-known-good approval policy");
                    cached.clone()
                }
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let policy = ApprovalPolicy {
                    mode: ApprovalMode::Smart,
                    deny: Vec::new(),
                    permanent: Vec::new(),
                    timeout: Duration::from_secs(300),
                    tirith_enabled: true,
                };
                *cached = policy.clone();
                policy
            }
            Err(error) => {
                tracing::warn!(%error, path = %path.display(), "native terminal retained last-known-good approval policy");
                cached.clone()
            }
        }
    }

    #[cfg(test)]
    async fn invoke(&self, args: &Value) -> Value {
        self.invoke_with_context(
            args,
            crate::native_tools::ToolCallContext::detached("direct"),
        )
        .await
    }

    async fn invoke_with_context(
        &self,
        args: &Value,
        context: crate::native_tools::ToolCallContext<'_>,
    ) -> Value {
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
        if background && args.get("pty").and_then(Value::as_bool) == Some(true) {
            return error_only(
                "Native background PTY sessions are not available yet. Omit pty for a non-interactive background process.",
            );
        }
        if background
            && (args.get("notify").is_some_and(crate::python_value::truthy)
                || args
                    .get("notify_on_complete")
                    .is_some_and(crate::python_value::truthy)
                || args
                    .get("watch_patterns")
                    .is_some_and(crate::python_value::truthy))
        {
            return error_only(
                "Native background notifications are not available yet. Omit notify and use process_manage(action='poll' or 'wait').",
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
        if !background && timeout > MAX_FOREGROUND_TIMEOUT {
            return error_only(&format!(
                "Foreground timeout {timeout}s exceeds the maximum of {MAX_FOREGROUND_TIMEOUT}s. Use background=true for long-running commands."
            ));
        }
        if !background {
            if let Some(guidance) = foreground_guidance(command) {
                return execution_error(guidance, "error");
            }
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

        let policy = self.current_approval_policy().await;
        let user_home = self
            .environment
            .get(&OsString::from("HOME"))
            .map(PathBuf::from);
        if let Some(pattern) = crate::terminal_guard::user_deny_match(
            command,
            &policy.deny,
            &self.profile_home,
            user_home.as_deref(),
        ) {
            return execution_error(
                &format!(
                    "BLOCKED: this command matches the user-defined deny rule '{pattern}' (approvals.deny in config.yaml). It cannot be executed via the agent - not even with --yolo, /yolo, or approvals.mode=off. Do NOT retry or rephrase this command; the user has explicitly forbidden it."
                ),
                "blocked",
            );
        }
        let mut approval_note = None;
        match policy.mode {
            ApprovalMode::Off => {}
            ApprovalMode::Smart => {
                return execution_error(
                    "Native terminal execution is unavailable because smart approval requires the auxiliary guardian. Start a new conversation or use the Python agent for smart approvals.",
                    "blocked",
                );
            }
            ApprovalMode::Manual => {
                if policy.tirith_enabled {
                    return execution_error(
                        "Native terminal execution is unavailable because Tirith security findings are not native yet. Start a new conversation or use the Python agent when security.tirith_enabled is true.",
                        "blocked",
                    );
                }
                if !command_matches_permanent_allowlist(command, &policy.permanent) {
                    if let Some(finding) = crate::dangerous_command::detect(
                        command,
                        &self.profile_home,
                        user_home.as_deref(),
                    ) {
                        let approved = policy.permanent.iter().any(|key| {
                            crate::dangerous_command::approval_key_matches(
                                &finding.pattern_key,
                                key,
                            )
                        }) || context.route_key.is_some_and(|route| {
                            self.tool_approvals
                                .is_session_approved(route, &finding.pattern_key)
                        });
                        if !approved {
                            if !self.approval_prompt_capable {
                                return execution_error(
                                    "BLOCKED: Command requires approval, but this surface cannot deliver an interactive approval prompt.",
                                    "blocked",
                                );
                            }
                            let Some(events) = context.events.cloned() else {
                                return execution_error(
                                    "BLOCKED: Command requires approval, but this surface cannot deliver an interactive approval prompt.",
                                    "blocked",
                                );
                            };
                            let Some(principal) = context.principal else {
                                return execution_error(
                                    "BLOCKED: Command requires approval, but the current sender identity is unavailable.",
                                    "blocked",
                                );
                            };
                            let Some(route_key) = context.route_key else {
                                return execution_error(
                                    "BLOCKED: Command requires approval, but its stable gateway route is unavailable.",
                                    "blocked",
                                );
                            };
                            let spec = crate::tool_approval::RequestSpec {
                                route_key: route_key.to_owned(),
                                principal: principal.to_owned(),
                                command: crate::compression_redact::redact(command),
                                description: crate::compression_redact::redact(
                                    &finding.description,
                                ),
                                pattern_keys: vec![finding.pattern_key.clone()],
                                allow_session: true,
                                allow_permanent: true,
                                smart_denied: false,
                                timeout: Some(policy.timeout),
                            };
                            let decision = self
                                .tool_approvals
                                .request_with_notify(spec, move |info| {
                                    let events = events.clone();
                                    async move {
                                        events
                                            .send(hermes_core::StreamEvent::ApprovalRequest {
                                                request_id: info.id,
                                                command: info.command,
                                                description: info.description,
                                                allow_session: info.allow_session,
                                                allow_permanent: info.allow_permanent,
                                                smart_denied: info.smart_denied,
                                            })
                                            .await
                                            .is_ok()
                                    }
                                })
                                .await;
                            match decision {
                                Ok(crate::tool_approval::Outcome::Decided(
                                    crate::tool_approval::Decision::AllowOnce,
                                )) => {}
                                Ok(crate::tool_approval::Outcome::Decided(
                                    crate::tool_approval::Decision::AllowSession,
                                )) => {
                                    self.tool_approvals.approve_for_session(
                                        route_key,
                                        &finding.pattern_key,
                                    );
                                }
                                Ok(crate::tool_approval::Outcome::Decided(
                                    crate::tool_approval::Decision::AllowAlways,
                                )) => {
                                    self.tool_approvals.approve_for_session(
                                        route_key,
                                        &finding.pattern_key,
                                    );
                                    if let Err(error) = persist_permanent_allowlist(
                                        &self.profile_home.join("config.yaml"),
                                        &finding.pattern_key,
                                    )
                                    .await
                                    {
                                        tracing::warn!(%error, "native permanent approval persistence failed");
                                    }
                                }
                                Ok(crate::tool_approval::Outcome::Decided(
                                    crate::tool_approval::Decision::Deny { reason },
                                )) => return approval_denied(reason.as_deref(), false),
                                Ok(crate::tool_approval::Outcome::TimedOut) => {
                                    return approval_denied(None, true)
                                }
                                Ok(crate::tool_approval::Outcome::Cancelled) => {
                                    return execution_error(
                                        "BLOCKED: Approval was cancelled before a decision. The user has NOT consented to this action. Do NOT retry.",
                                        "blocked",
                                    )
                                }
                                Err(crate::tool_approval::SubmitError::Overloaded) => {
                                    return execution_error(
                                        "BLOCKED: Too many approval requests are pending. The user has NOT consented to this action. Do NOT retry.",
                                        "blocked",
                                    )
                                }
                            }
                            approval_note = Some(format!(
                                "Command required approval ({}) and was approved by the user.",
                                finding.description
                            ));
                        }
                    }
                }
            }
        }

        let mut session_cwd = self.cwd.lock().await;
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
        if background {
            let mut environment = self.environment.clone();
            environment.insert("PYTHONUNBUFFERED".into(), "1".into());
            let request = crate::background_process::SpawnSpec::with_default_bounds(
                self.process_owner.clone(),
                command.clone(),
                self.shell.clone(),
                vec![
                    "--noprofile".into(),
                    "--norc".into(),
                    "-c".into(),
                    background_wrapper().into(),
                    "hermes-terminal-background".into(),
                    command.into(),
                    self.snapshot_path.as_os_str().into(),
                    self.profile_home.as_os_str().into(),
                ],
                command_cwd,
                environment,
            );
            let mut result = match self.process_registry.spawn(request) {
                Ok(spawned) => json!({
                    "output":"Background process started",
                    "session_id":spawned.id,
                    "pid":spawned.pid,
                    "exit_code":0,
                    "error":Value::Null,
                    "hint":"This process runs silently. Use process_manage(action='poll' or 'wait') to observe completion.",
                }),
                Err(error) => json!({
                    "output":"",
                    "exit_code":-1,
                    "error":redact_output(&format!("Failed to start background process: {error}")),
                }),
            };
            if let Some(note) = approval_note {
                result["approval"] = json!(note);
            }
            return result;
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

        let mut result = match outcome {
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
        };
        if let Some(note) = approval_note {
            result["approval"] = json!(note);
        }
        result
    }
}

#[async_trait]
impl Tool for TerminalTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "terminal".into(),
            description: "Execute a local shell command in the foreground or start a managed background process. Foreground calls persist completed working-directory and exported-environment changes. Background calls return a session id for process_manage.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "command": {"type":"string", "description":"The shell command to execute"},
                    "background": {"type":"boolean", "default":false, "description":"Run as a managed non-interactive background process and return a session id."},
                    "timeout": {"type":"integer", "minimum":1, "description":"Maximum seconds to wait for foreground execution. Background commands return immediately, so this value is ignored for them."},
                    "workdir": {"type":"string", "description":"Working directory for this command. Defaults to the session working directory."}
                },
                "required": ["command"],
                "additionalProperties": false
            }),
            extra: Default::default(),
        }
    }

    async fn call(
        &self,
        args: &Value,
        context: crate::native_tools::ToolCallContext<'_>,
    ) -> Result<Value> {
        Ok(self.invoke_with_context(args, context).await)
    }
}

fn approval_denied(reason: Option<&str>, timed_out: bool) -> Value {
    let (cause, outcome) = if timed_out {
        ("timed out without user response", "timeout")
    } else {
        ("denied by user", "denied")
    };
    let reason = reason
        .filter(|reason| !reason.is_empty())
        .map(|reason| format!(" Reason given by the user: \"{reason}\"."))
        .unwrap_or_default();
    let silence = if timed_out {
        " Silence is not consent."
    } else {
        ""
    };
    json!({
        "output":"",
        "exit_code":-1,
        "error":format!(
            "BLOCKED: Command {cause}.{reason} The user has NOT consented to this action. Do NOT retry this command, do NOT rephrase it, and do NOT attempt the same outcome via a different command. Stop the current workflow and wait for the user to respond before taking any further destructive or irreversible action.{silence}"
        ),
        "status":"blocked",
        "outcome":outcome,
        "user_consent":false,
    })
}

fn command_matches_permanent_allowlist(command: &str, patterns: &[String]) -> bool {
    let command = command.trim();
    if command.is_empty() || has_allowlist_shell_operator(command) {
        return false;
    }
    patterns.iter().any(|pattern| {
        let pattern = pattern.trim();
        !pattern.is_empty()
            && (command == pattern
                || pattern.contains(['*', '?', '['])
                    && crate::python_fnmatch::matches(command, pattern))
    })
}

fn has_allowlist_shell_operator(command: &str) -> bool {
    static REINTERPRETED_ARGUMENT: LazyLock<fancy_regex::Regex> = LazyLock::new(|| {
        fancy_regex::Regex::new(r"(?:^|[ \t])(?:-[^-\s]*[ce]|--(?:command|eval))(?:[= \t]|$)")
            .expect("static reinterpreted-argument regex")
    });

    let mut quote = None;
    let mut escaped = false;
    let mut has_reinterpretable = false;
    let mut chars = command.chars().peekable();
    while let Some(ch) = chars.next() {
        if escaped {
            if matches!(
                ch,
                '\n' | '\r' | ';' | '&' | '|' | '<' | '>' | '`' | '$' | '(' | ')'
            ) {
                has_reinterpretable = true;
            }
            escaped = false;
            continue;
        }
        if ch == '\\' && quote != Some('\'') {
            escaped = true;
            continue;
        }
        if let Some(active) = quote {
            if ch == active {
                quote = None;
            } else if active == '"' && matches!(ch, '$' | '`') {
                return true;
            } else if matches!(
                ch,
                '\n' | '\r' | ';' | '&' | '|' | '<' | '>' | '`' | '$' | '(' | ')'
            ) {
                has_reinterpretable = true;
            }
            continue;
        }
        if matches!(ch, '\'' | '"') {
            quote = Some(ch);
        } else if matches!(ch, '\n' | '\r' | ';' | '&' | '|' | '<' | '>' | '`')
            || ch == '$' && chars.peek() == Some(&'(')
        {
            return true;
        }
    }
    quote.is_some()
        || has_reinterpretable
            && REINTERPRETED_ARGUMENT
                .is_match(command)
                .expect("static reinterpreted-argument regex evaluates")
}

async fn persist_permanent_allowlist(path: &Path, pattern_key: &str) -> anyhow::Result<()> {
    let path = path.to_owned();
    let pattern_key = pattern_key.to_owned();
    tokio::task::spawn_blocking(move || persist_permanent_allowlist_sync(&path, &pattern_key))
        .await
        .map_err(|error| anyhow::anyhow!("config writer task failed: {error}"))?
}

fn persist_permanent_allowlist_sync(path: &Path, pattern_key: &str) -> anyhow::Result<()> {
    use std::str::FromStr;
    use yaml_edit::path::YamlPath;
    use yaml_edit::{SequenceBuilder, YamlFile};

    let _guard = crate::config_file::config_write_lock();
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(error.into()),
    };
    let mut document_text = text.as_str();
    let mut patterns = Vec::new();
    if !text.trim().is_empty() {
        let parsed: Value = serde_yaml_ng::from_str(&text)
            .map_err(|error| anyhow::anyhow!("invalid config.yaml: {error}"))?;
        anyhow::ensure!(
            parsed.is_object() || parsed.is_null(),
            "config.yaml top level is not a mapping"
        );
        if parsed.is_null() {
            document_text = "";
        } else {
            patterns.extend(
                parsed["command_allowlist"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .map(str::trim)
                    .filter(|pattern| !pattern.is_empty())
                    .map(str::to_owned),
            );
        }
    }
    if !patterns.iter().any(|pattern| pattern == pattern_key) {
        patterns.push(pattern_key.to_owned());
    }
    patterns.sort();
    patterns.dedup();
    let document =
        YamlFile::from_str(document_text).map_err(|error| anyhow::anyhow!(error.to_string()))?;
    let first = document.ensure_document();
    anyhow::ensure!(
        first.as_mapping().is_some(),
        "config.yaml top level is not a mapping"
    );
    let sequence = patterns
        .iter()
        .fold(SequenceBuilder::new(), |builder, pattern| {
            builder.item(pattern.as_str())
        })
        .build_document()
        .as_sequence()
        .ok_or_else(|| anyhow::anyhow!("could not build command allowlist sequence"))?;
    first
        .try_set_path("command_allowlist", sequence)
        .map_err(|error| anyhow::anyhow!(error.to_string()))?;
    let rendered = document.to_string();
    let check: Value = serde_yaml_ng::from_str(&rendered)
        .map_err(|error| anyhow::anyhow!("edited config.yaml is invalid: {error}"))?;
    let saved = check["command_allowlist"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect::<Vec<_>>();
    anyhow::ensure!(
        saved == patterns.iter().map(String::as_str).collect::<Vec<_>>(),
        "edited config.yaml did not contain the approval keys"
    );
    crate::atomic_file::write_private_preserving_symlink(path, rendered.as_bytes())?;
    Ok(())
}

fn background_wrapper() -> &'static str {
    r#"exec 2>&1
snapshot=$2
if [ -f "$snapshot" ]; then . "$snapshot"; fi
export HERMES_HOME=$3
eval "$1"
hermes_status=$?
wait
wait_status=$?
if [ "$hermes_status" -eq 0 ]; then hermes_status=$wait_status; fi
exit "$hermes_status""#
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

pub(crate) fn redact_output(output: &str) -> String {
    crate::compression_redact::redact(&crate::terminal_guard::strip_ansi(output))
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
        std::fs::write(
            home.join("config.yaml"),
            "approvals:\n  mode: off\n  deny: []\n",
        )
        .unwrap();
        let profile = HashMap::from([("PROFILE_SECRET".into(), "only-this-profile".into())]);
        TerminalTool::new(TerminalConfig {
            cwd: cwd.to_path_buf(),
            profile_env: &profile,
            profile_home: home,
            session_identity: "test-session",
            default_timeout: 5,
            database: None,
            route: None,
            process_registry: Arc::new(crate::background_process::Registry::new()),
            approval_config: json!({"approvals":{"mode":"off","deny":[]}}),
            tool_approvals: Arc::new(crate::tool_approval::ApprovalBroker::new()),
            approval_prompt_capable: false,
        })
    }

    #[test]
    fn approval_policy_defaults_and_timeout_coercion_match_python() {
        let defaults = ApprovalPolicy::from_config(&json!({}));
        assert_eq!(defaults.mode, ApprovalMode::Smart);
        assert!(defaults.tirith_enabled);
        assert_eq!(defaults.timeout, Duration::from_secs(300));

        for (value, seconds) in [
            (json!(true), 1),
            (json!(false), 0),
            (json!(12.9), 12),
            (json!("45"), 45),
            (json!("invalid"), 300),
            (json!(-10), 0),
            (json!(99_999_999), 365 * 24 * 60 * 60),
        ] {
            assert_eq!(approval_timeout(&value), Duration::from_secs(seconds));
        }

        let manual = ApprovalPolicy::from_config(&json!({
            "approvals":{"mode":"manual"},
            "security":{"tirith_enabled":false}
        }));
        assert_eq!(manual.mode, ApprovalMode::Manual);
        assert!(!manual.tirith_enabled);
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

    #[cfg(unix)]
    #[tokio::test]
    async fn background_uses_persisted_environment_and_shared_registry() {
        let home = temp_dir("background");
        std::fs::write(
            home.join("config.yaml"),
            "approvals:\n  mode: off\n  deny: []\n",
        )
        .unwrap();
        let registry = Arc::new(crate::background_process::Registry::new());
        let profile = HashMap::from([("PROFILE_SECRET".into(), "only-this-profile".into())]);
        let terminal = TerminalTool::new(TerminalConfig {
            cwd: home.clone(),
            profile_env: &profile,
            profile_home: &home,
            session_identity: "background-route",
            default_timeout: 5,
            database: None,
            route: None,
            process_registry: registry.clone(),
            approval_config: json!({"approvals":{"mode":"off","deny":[]}}),
            tool_approvals: Arc::new(crate::tool_approval::ApprovalBroker::new()),
            approval_prompt_capable: false,
        });
        let exported = terminal
            .invoke(&json!({"command":"export SAVED=background-value"}))
            .await;
        assert_eq!(exported["exit_code"], 0);
        let launched = terminal
            .invoke(&json!({
                "command":"printf '%s:%s' \"$SAVED\" \"$PROFILE_SECRET\"",
                "background":true
            }))
            .await;
        let id = launched["session_id"].as_str().unwrap();
        let owner = crate::background_process::Owner::new(&home, "background-route");
        match registry
            .wait(&owner, id, Duration::from_secs(5))
            .await
            .unwrap()
        {
            crate::background_process::WaitOutcome::Finished { output, .. } => {
                assert_eq!(output, "background-value:only-this-profile")
            }
            crate::background_process::WaitOutcome::TimedOut { .. } => {
                panic!("background command did not finish")
            }
        }
        std::fs::remove_dir_all(home).unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn manual_approval_suspends_then_applies_session_and_deny_scopes() {
        let home = temp_dir("manual-approval");
        std::fs::write(
            home.join("config.yaml"),
            "approvals:\n  mode: manual\n  deny: []\n  timeout: 5\nsecurity:\n  tirith_enabled: false\n",
        )
        .unwrap();
        let broker = Arc::new(crate::tool_approval::ApprovalBroker::new());
        let profile = HashMap::new();
        let terminal = Arc::new(TerminalTool::new(TerminalConfig {
            cwd: home.clone(),
            profile_env: &profile,
            profile_home: &home,
            session_identity: "frozen-physical-session",
            default_timeout: 5,
            database: None,
            route: None,
            process_registry: Arc::new(crate::background_process::Registry::new()),
            approval_config: json!({
                "approvals":{"mode":"manual","deny":[],"timeout":5},
                "security":{"tirith_enabled":false}
            }),
            tool_approvals: broker.clone(),
            approval_prompt_capable: true,
        }));
        let (events, mut receiver) = tokio::sync::mpsc::channel(4);
        let first_terminal = terminal.clone();
        let first = tokio::spawn(async move {
            first_terminal
                .call(
                    &json!({"command":"bash -c 'printf approved'"}),
                    crate::native_tools::ToolCallContext {
                        events: Some(&events),
                        call_id: "call-1",
                        principal: Some("owner"),
                        route_key: Some("route"),
                    },
                )
                .await
                .unwrap()
        });
        let event = receiver.recv().await.unwrap();
        assert!(matches!(
            event,
            hermes_core::StreamEvent::ApprovalRequest { ref command, .. }
                if command == "bash -c 'printf approved'"
        ));
        assert_eq!(
            broker.resolve_text("route", "/approve session", |info| {
                info.principal == "stale-owner"
            }),
            crate::tool_approval::ResolveOutcome::Unauthorized
        );
        assert!(matches!(
            broker.resolve_text("route", "/approve session", |info| {
                info.principal == "owner"
            }),
            crate::tool_approval::ResolveOutcome::Resolved {
                decision: crate::tool_approval::Decision::AllowSession,
                ..
            }
        ));
        let result = first.await.unwrap();
        assert_eq!(result["output"], "approved");
        assert_eq!(
            result["approval"],
            "Command required approval (shell command via -c/-lc flag) and was approved by the user."
        );

        let rebuilt_terminal = Arc::new(TerminalTool::new(TerminalConfig {
            cwd: home.clone(),
            profile_env: &profile,
            profile_home: &home,
            session_identity: "frozen-physical-session",
            default_timeout: 5,
            database: None,
            route: None,
            process_registry: Arc::new(crate::background_process::Registry::new()),
            approval_config: json!({
                "approvals":{"mode":"manual","deny":[],"timeout":5},
                "security":{"tirith_enabled":false}
            }),
            tool_approvals: broker.clone(),
            approval_prompt_capable: true,
        }));
        let second = rebuilt_terminal
            .call(
                &json!({"command":"bash -c 'printf remembered'"}),
                crate::native_tools::ToolCallContext {
                    events: None,
                    call_id: "call-rebuilt",
                    principal: Some("owner"),
                    route_key: Some("route"),
                },
            )
            .await;
        let second = second.unwrap();
        assert_eq!(second["output"], "remembered");
        assert!(second.get("approval").is_none());

        let (events, mut receiver) = tokio::sync::mpsc::channel(4);
        let denied_terminal = rebuilt_terminal.clone();
        let denied = tokio::spawn(async move {
            denied_terminal
                .call(
                    &json!({"command":"git push --force invalid.invalid branch"}),
                    crate::native_tools::ToolCallContext {
                        events: Some(&events),
                        call_id: "call-2",
                        principal: Some("owner"),
                        route_key: Some("route"),
                    },
                )
                .await
                .unwrap()
        });
        assert!(matches!(
            receiver.recv().await.unwrap(),
            hermes_core::StreamEvent::ApprovalRequest { .. }
        ));
        broker.resolve_text("route", "/deny unsafe remote", |_| true);
        let denied = denied.await.unwrap();
        assert_eq!(denied["outcome"], "denied");
        assert!(denied["error"]
            .as_str()
            .unwrap()
            .contains("Reason given by the user: \"unsafe remote\""));
        std::fs::remove_dir_all(home).unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn always_approval_is_persisted_losslessly_and_reloaded() {
        let home = temp_dir("always-approval");
        std::fs::write(
            home.join("config.yaml"),
            "# keep this comment\napprovals:\n  mode: manual\n  deny: []\n  timeout: 5\nsecurity:\n  tirith_enabled: false\n",
        )
        .unwrap();
        let broker = Arc::new(crate::tool_approval::ApprovalBroker::new());
        let profile = HashMap::new();
        let terminal = Arc::new(TerminalTool::new(TerminalConfig {
            cwd: home.clone(),
            profile_env: &profile,
            profile_home: &home,
            session_identity: "frozen-physical-session",
            default_timeout: 5,
            database: None,
            route: None,
            process_registry: Arc::new(crate::background_process::Registry::new()),
            approval_config: crate::config_file::load_config_from(&home.join("config.yaml")),
            tool_approvals: broker.clone(),
            approval_prompt_capable: true,
        }));
        let (events, mut receiver) = tokio::sync::mpsc::channel(4);
        let waiter = terminal.clone();
        let run = tokio::spawn(async move {
            waiter
                .call(
                    &json!({"command":"bash -c 'printf persisted'"}),
                    crate::native_tools::ToolCallContext {
                        events: Some(&events),
                        call_id: "call",
                        principal: Some("owner"),
                        route_key: Some("route"),
                    },
                )
                .await
                .unwrap()
        });
        receiver.recv().await.unwrap();
        broker.resolve_text("route", "/approve always", |_| true);
        assert_eq!(run.await.unwrap()["output"], "persisted");
        let saved = std::fs::read_to_string(home.join("config.yaml")).unwrap();
        assert!(saved.contains("# keep this comment"));
        assert_eq!(
            crate::config_file::load_config_from(&home.join("config.yaml"))["command_allowlist"],
            json!(["shell command via -c/-lc flag"])
        );
        std::fs::remove_dir_all(home).unwrap();
    }

    #[tokio::test]
    async fn background_refuses_unimplemented_notification_and_pty_promises() {
        let home = temp_dir("background-options");
        let terminal = tool(&home, &home);
        let notify = terminal
            .invoke(&json!({"command":"true", "background":true, "notify":true}))
            .await;
        assert!(notify["error"].as_str().unwrap().contains("notifications"));
        let pty = terminal
            .invoke(&json!({"command":"true", "background":true, "pty":true}))
            .await;
        assert!(pty["error"].as_str().unwrap().contains("PTY"));
        std::fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn schema_advertises_managed_background_without_unimplemented_options() {
        let home = temp_dir("background-schema");
        let terminal = tool(&home, &home);
        let properties = terminal.spec().parameters["properties"].clone();
        assert_eq!(properties["background"]["type"], "boolean");
        assert!(properties["timeout"].get("maximum").is_none());
        assert!(properties.get("pty").is_none());
        assert!(properties.get("notify").is_none());
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

    #[cfg(unix)]
    #[tokio::test]
    async fn deny_rules_reload_for_foreground_and_background_and_retain_lkg() {
        let home = temp_dir("deny-reload");
        let marker = home.join("must-not-exist");
        let terminal = tool(&home, &home);
        std::fs::write(
            home.join("config.yaml"),
            "approvals:\n  mode: off\n  deny:\n    - 'touch *'\n    - '*'\n",
        )
        .unwrap();

        let hardline = terminal.invoke(&json!({"command":"rm -rf /"})).await;
        assert!(hardline["error"].as_str().unwrap().contains("hardline"));

        for background in [false, true] {
            let denied = terminal
                .invoke(&json!({
                    "command":format!("touch {}", marker.display()),
                    "background":background,
                }))
                .await;
            assert_eq!(denied["status"], "blocked");
            assert!(denied["error"]
                .as_str()
                .unwrap()
                .contains("deny rule 'touch *'"));
        }
        assert!(!marker.exists());

        std::fs::write(
            home.join("config.yaml"),
            "approvals:\n  mode: off\n  deny:\n    - 'printf forbidden*'\n",
        )
        .unwrap();
        let updated = terminal
            .invoke(&json!({"command":"printf forbidden-value"}))
            .await;
        assert_eq!(updated["status"], "blocked");

        std::fs::write(home.join("config.yaml"), "approvals: [unterminated").unwrap();
        let retained = terminal
            .invoke(&json!({"command":"printf forbidden-after-corruption"}))
            .await;
        assert_eq!(retained["status"], "blocked");
        assert!(retained["error"]
            .as_str()
            .unwrap()
            .contains("printf forbidden*"));

        std::fs::write(
            home.join("config.yaml"),
            "approvals:\n  mode: smart\n  deny: []\n",
        )
        .unwrap();
        let mode_changed = terminal
            .invoke(&json!({"command":format!("touch {}", marker.display())}))
            .await;
        assert_eq!(mode_changed["status"], "blocked");
        assert!(mode_changed["error"]
            .as_str()
            .unwrap()
            .contains("smart approval requires the auxiliary guardian"));
        assert!(!marker.exists());

        std::fs::write(
            home.join("config.yaml"),
            "approvals:\n  mode: manual\n  deny: []\n",
        )
        .unwrap();
        let unsupported_tirith = terminal
            .invoke(&json!({"command":"bash -c 'printf must-not-run'"}))
            .await;
        assert_eq!(unsupported_tirith["status"], "blocked");
        assert!(unsupported_tirith["error"]
            .as_str()
            .unwrap()
            .contains("Tirith security findings are not native"));

        std::fs::write(
            home.join("config.yaml"),
            "approvals:\n  mode: manual\n  deny: []\nsecurity:\n  tirith_enabled: false\n",
        )
        .unwrap();
        let unsupported_manual = terminal
            .invoke(&json!({"command":"bash -c 'printf must-not-run'"}))
            .await;
        assert_eq!(unsupported_manual["status"], "blocked");
        assert!(unsupported_manual["error"]
            .as_str()
            .unwrap()
            .contains("surface cannot deliver"));

        std::fs::write(
            home.join("config.yaml"),
            "approvals:\n  mode: off\n  deny: invalid\n",
        )
        .unwrap();
        let invalid_policy = terminal
            .invoke(&json!({"command":format!("touch {}", marker.display())}))
            .await;
        assert_eq!(invalid_policy["status"], "blocked");
        assert!(!marker.exists());
        std::fs::remove_dir_all(home).unwrap();
    }

    #[tokio::test]
    async fn user_deny_envelope_matches_python_contract() {
        let home = temp_dir("deny-envelope");
        let terminal = tool(&home, &home);
        std::fs::write(
            home.join("config.yaml"),
            "approvals:\n  mode: off\n  deny:\n    - 'git push*'\n",
        )
        .unwrap();
        let approval_corpus: Value = serde_json::from_str(include_str!(
            "../../../tools/approval-deny-contract-goldens.json"
        ))
        .unwrap();
        let case = approval_corpus["terminal_tool_envelopes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|case| case["id"] == "envelope_user_deny_block")
            .unwrap()
            .clone();
        let actual = terminal
            .invoke(&json!({"command":case["command"].clone()}))
            .await;
        assert_eq!(actual, case["raw_envelope"]);
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
            let expected = case["terminal_tool_envelope"]["error"]
                .as_str()
                .unwrap()
                .replace(" with notify_on_complete=true", "");
            assert_eq!(actual["error"], expected, "{}", case["id"]);
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
        assert_eq!(
            crate::terminal_guard::strip_ansi("a\u{1b}[31mred\u{1b}[0m b"),
            "ared b"
        );
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
    fn permanent_allowlist_shell_operator_rules_match_python() {
        for command in [
            "echo $HOME",
            "cargo bench -- '^a(b|c)$'",
            r"printf \|",
            "echo foo(bar)",
        ] {
            assert!(
                command_matches_permanent_allowlist(command, &["*".into()]),
                "simple command should remain allowlist-eligible: {command}"
            );
        }
        for command in [
            "echo $(whoami)",
            "echo \"$HOME\"",
            "sh -c 'echo a|b'",
            "git -c alias.x='!echo a|b' x",
            "echo 'unterminated",
        ] {
            assert!(
                !command_matches_permanent_allowlist(command, &["*".into()]),
                "compound command must not use the allowlist shortcut: {command}"
            );
        }
    }

    #[test]
    fn concurrent_permanent_approvals_merge_under_the_config_lock() {
        let home = temp_dir("concurrent-always-approval");
        let path = home.join("config.yaml");
        std::fs::write(&path, "# preserve\ncommand_allowlist:\n  - existing\n").unwrap();
        let first_path = path.clone();
        let first = std::thread::spawn(move || {
            persist_permanent_allowlist_sync(&first_path, "recursive delete")
        });
        let second_path = path.clone();
        let second = std::thread::spawn(move || {
            persist_permanent_allowlist_sync(&second_path, "force push")
        });
        first.join().unwrap().unwrap();
        second.join().unwrap().unwrap();
        let saved = crate::config_file::load_config_from(&path);
        assert_eq!(
            saved["command_allowlist"],
            json!(["existing", "force push", "recursive delete"])
        );
        assert!(std::fs::read_to_string(&path)
            .unwrap()
            .contains("# preserve"));
        std::fs::remove_dir_all(home).unwrap();
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
