//! Provider-facing management tool for native local background processes.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use hermes_core::Result;
use serde_json::{json, Value};

use crate::background_process::{Owner, Registry, Status, WaitOutcome};
use crate::native_tools::{Tool, ToolSpec};

pub struct ProcessTool {
    registry: Arc<Registry>,
    owner: Owner,
    max_wait_seconds: u64,
}

impl ProcessTool {
    pub fn new(registry: Arc<Registry>, owner: Owner, max_wait_seconds: u64) -> Self {
        Self {
            registry,
            owner,
            max_wait_seconds: max_wait_seconds.max(1),
        }
    }

    async fn invoke(&self, args: &Value) -> Value {
        let Some(args) = args.as_object() else {
            return json!({"error":"Invalid process_manage arguments: expected an object."});
        };
        let action = args.get("action").map(argument_text).unwrap_or_default();
        if action == "list" {
            let processes = self
                .registry
                .list(&self.owner)
                .into_iter()
                .map(|process| {
                    let mut entry = json!({
                        "session_id": process.id,
                        "command": clean(&process.command),
                        "cwd": process.cwd,
                        "pid": process.pid,
                        "started_at": chrono::DateTime::<chrono::Local>::from(process.started_at)
                            .format("%Y-%m-%dT%H:%M:%S")
                            .to_string(),
                        "uptime_seconds": process.uptime.as_secs(),
                        "status": status_name(process.status),
                        "output_preview": clean(&process.output_preview),
                    });
                    if let Some(code) = process.status.exit_code() {
                        entry["exit_code"] = json!(code);
                    }
                    entry
                })
                .collect::<Vec<_>>();
            return json!({"processes": processes});
        }

        let supported = ["poll", "log", "wait", "kill"];
        if !supported.contains(&action.as_str()) {
            return json!({
                "error": format!(
                    "Unknown process action: {action}. Use: list, poll, log, wait, kill"
                )
            });
        }
        let session_id = args
            .get("session_id")
            .filter(|value| !value.is_null())
            .map(argument_text)
            .unwrap_or_default();
        if session_id.is_empty() {
            return json!({"error":format!("session_id is required for {action}")});
        }

        match action.as_str() {
            "poll" => match self.registry.poll(&self.owner, &session_id) {
                Ok(process) => poll_json(process),
                Err(_) => not_found(&session_id),
            },
            "log" => {
                let offset = match optional_nonnegative(args.get("offset"), "offset") {
                    Ok(value) => value,
                    Err(error) => return error,
                };
                let limit = match positive(args.get("limit"), 200, "limit") {
                    Ok(value) => value as usize,
                    Err(error) => return error,
                };
                match self.registry.log(
                    &self.owner,
                    &session_id,
                    offset.map(|value| value as usize),
                    limit,
                ) {
                    Ok(process) => json!({
                        "session_id":process.id,
                        "command":clean(&process.command),
                        "status":status_name(process.status),
                        "output":clean(&process.output),
                        "total_lines":process.total_lines,
                        "showing":format!("{} lines", process.returned_lines),
                    }),
                    Err(_) => not_found(&session_id),
                }
            }
            "wait" => {
                let requested =
                    match positive(args.get("timeout"), self.max_wait_seconds, "timeout") {
                        Ok(value) => value,
                        Err(error) => return error,
                    };
                let seconds = requested.min(self.max_wait_seconds);
                let clamp_note = (requested > self.max_wait_seconds).then(|| {
                    format!(
                        "Requested wait of {requested}s was clamped to configured limit of {}s",
                        self.max_wait_seconds
                    )
                });
                match self
                    .registry
                    .wait(&self.owner, &session_id, Duration::from_secs(seconds))
                    .await
                {
                    Ok(WaitOutcome::Finished {
                        status,
                        command,
                        output,
                    }) => {
                        let mut result = terminal_status_json(status, &command, &output);
                        if let Some(note) = clamp_note {
                            result["timeout_note"] = json!(note);
                        }
                        result
                    }
                    Ok(WaitOutcome::TimedOut {
                        command,
                        output,
                        uptime,
                    }) => {
                        let guidance = format!(
                            "Wait window of {seconds}s elapsed - the process is still running. This is not an error. Uptime: {}s. Poll again later.",
                            uptime.as_secs()
                        );
                        json!({
                            "status":"timeout",
                            "command":clean(&command),
                            "output":clean(&output),
                            "process_running":true,
                            "timeout_note":match clamp_note {
                                Some(note) => format!("{note}. {guidance}"),
                                None => guidance,
                            },
                        })
                    }
                    Err(_) => not_found(&session_id),
                }
            }
            "kill" => match self.registry.kill(&self.owner, &session_id).await {
                Ok(process) if process.status == Status::Running => json!({
                    "status":"error",
                    "error":"Process did not exit after termination escalation.",
                }),
                Ok(process) if process.already_finished => json!({
                    "status":"already_exited",
                    "command":clean(&process.command),
                    "exit_code":process.status.exit_code(),
                    "completion_reason":completion_reason(process.status),
                    "termination_source":termination_source(process.status),
                    "output":clean(&process.output),
                }),
                Ok(process) => json!({
                    "status":"killed",
                    "session_id":process.id,
                    "completion_reason":"killed",
                    "termination_source":"process.kill",
                    "output":clean(&process.output),
                }),
                Err(_) => not_found(&session_id),
            },
            _ => unreachable!(),
        }
    }
}

#[async_trait]
impl Tool for ProcessTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "process_manage".into(),
            description: "List, poll, read logs from, wait on, or kill local background terminal processes. poll returns status plus the current output tail. log returns retained output by line. wait blocks only for the requested bounded window.".into(),
            parameters: json!({
                "type":"object",
                "properties":{
                    "action":{"type":"string", "enum":["list","poll","log","wait","kill"]},
                    "session_id":{"type":"string", "description":"From terminal background output; any unique prefix of at least four characters works. Required except for list."},
                    "timeout":{"type":"integer", "minimum":1, "description":"Maximum seconds for wait."},
                    "offset":{"type":"integer", "minimum":0, "description":"Log line offset. Omit to return the last 200 lines."},
                    "limit":{"type":"integer", "minimum":1, "description":"Maximum log lines."}
                },
                "required":["action"],
                "additionalProperties":false
            }),
            extra: Default::default(),
        }
    }

    async fn call(
        &self,
        args: &Value,
        _context: crate::native_tools::ToolCallContext<'_>,
    ) -> Result<Value> {
        Ok(self.invoke(args).await)
    }
}

fn poll_json(process: crate::background_process::Poll) -> Value {
    let mut result = json!({
        "session_id":process.id,
        "command":clean(&process.command),
        "status":status_name(process.status),
        "pid":process.pid,
        "uptime_seconds":process.uptime.as_secs(),
        "output_preview":clean(&process.output_preview),
    });
    if process.status.is_terminal() {
        result["exit_code"] = json!(process.status.exit_code());
        result["completion_reason"] = json!(completion_reason(process.status));
        result["termination_source"] = json!(termination_source(process.status));
    }
    result
}

fn terminal_status_json(status: Status, command: &str, output: &str) -> Value {
    json!({
        "status":"exited",
        "command":clean(command),
        "exit_code":status.exit_code(),
        "completion_reason":completion_reason(status),
        "termination_source":termination_source(status),
        "output":clean(output),
    })
}

fn status_name(status: Status) -> &'static str {
    if status == Status::Running {
        "running"
    } else {
        "exited"
    }
}

fn completion_reason(status: Status) -> &'static str {
    if status == Status::Killed {
        "killed"
    } else {
        "exited"
    }
}

fn termination_source(status: Status) -> &'static str {
    if status == Status::Killed {
        "process.kill"
    } else {
        ""
    }
}

fn clean(text: &str) -> String {
    crate::native_terminal::redact_output(text)
}

fn not_found(id: &str) -> Value {
    json!({"status":"not_found", "error":format!("No process with ID {id}")})
}

fn argument_text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        other => crate::python_value::python_repr(other),
    }
}

fn positive(value: Option<&Value>, default: u64, name: &str) -> std::result::Result<u64, Value> {
    let Some(value) = value.filter(|value| !value.is_null()) else {
        return Ok(default);
    };
    match value.as_i64() {
        Some(number) if number > 0 => Ok(number as u64),
        Some(number) => Err(
            json!({"status":"error", "error":format!("{name} must be positive (got {number})")}),
        ),
        None => {
            Err(json!({"status":"error", "error":format!("{name} must be a positive integer")}))
        }
    }
}

fn optional_nonnegative(
    value: Option<&Value>,
    name: &str,
) -> std::result::Result<Option<u64>, Value> {
    let Some(value) = value.filter(|value| !value.is_null()) else {
        return Ok(None);
    };
    match value.as_i64() {
        Some(number) if number >= 0 => Ok(Some(number as u64)),
        Some(number) => Err(
            json!({"status":"error", "error":format!("{name} must not be negative (got {number})")}),
        ),
        None => Err(json!({"status":"error", "error":format!("{name} must be an integer")})),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn process_tool(route: &str) -> ProcessTool {
        ProcessTool::new(
            Arc::new(Registry::new()),
            Owner::new(std::path::Path::new("/profile"), route),
            2,
        )
    }

    fn spawn(tool: &ProcessTool, command: &str) -> String {
        let mut env = std::collections::BTreeMap::new();
        if let Some(path) = std::env::var_os("PATH") {
            env.insert("PATH".into(), path);
        }
        tool.registry
            .spawn(crate::background_process::SpawnSpec::with_default_bounds(
                tool.owner.clone(),
                command.into(),
                "/bin/bash".into(),
                vec![
                    "--noprofile".into(),
                    "--norc".into(),
                    "-c".into(),
                    command.into(),
                ],
                std::env::temp_dir(),
                env,
            ))
            .unwrap()
            .id
    }

    #[test]
    fn schema_exposes_only_the_implemented_non_pty_actions() {
        let tool = process_tool("route");
        assert_eq!(
            tool.spec().parameters["properties"]["action"]["enum"],
            json!(["list", "poll", "log", "wait", "kill"])
        );
    }

    #[tokio::test]
    async fn interface_lists_polls_pages_and_waits_with_redaction() {
        let tool = process_tool("route");
        let id = spawn(
            &tool,
            "printf 'first\\nOPENAI_API_KEY=sk-secretvalue123456\\nthird\\n'",
        );
        let waited = tool
            .invoke(&json!({"action":"wait", "session_id":&id[5..9], "timeout":2}))
            .await;
        assert_eq!(waited["status"], "exited");
        assert!(!waited.to_string().contains("secretvalue"), "{waited}");
        let polled = tool
            .invoke(&json!({"action":"poll", "session_id":id}))
            .await;
        assert_eq!(polled["exit_code"], 0);
        assert!(!polled.to_string().contains("secretvalue"));
        let log = tool
            .invoke(&json!({"action":"log", "session_id":id, "offset":0, "limit":1}))
            .await;
        assert_eq!(log["output"], "first");
        assert_eq!(log["total_lines"], 3);
        let listed = tool.invoke(&json!({"action":"list"})).await;
        assert_eq!(listed["processes"].as_array().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn interface_rejects_bad_dispatch_and_foreign_owners() {
        let tool = process_tool("route");
        let id = spawn(&tool, "true");
        assert_eq!(
            tool.invoke(&json!({"action":"poll"})).await["error"],
            "session_id is required for poll"
        );
        assert!(tool
            .invoke(&json!({"action":"write", "session_id":id}))
            .await["error"]
            .as_str()
            .unwrap()
            .contains("Unknown process action"));
        assert_eq!(
            tool.invoke(&json!({"action":"wait", "session_id":id, "timeout":0}))
                .await["status"],
            "error"
        );
        let foreign = ProcessTool::new(
            tool.registry.clone(),
            Owner::new(std::path::Path::new("/profile"), "other"),
            2,
        );
        assert_eq!(
            foreign
                .invoke(&json!({"action":"poll", "session_id":id}))
                .await["status"],
            "not_found"
        );
    }

    #[tokio::test]
    async fn interface_kills_once_then_reports_already_exited() {
        let tool = process_tool("route");
        let id = spawn(&tool, "sleep 30 & wait");
        let killed = tool
            .invoke(&json!({"action":"kill", "session_id":id}))
            .await;
        assert_eq!(killed["status"], "killed");
        assert_eq!(killed["termination_source"], "process.kill");
        let again = tool
            .invoke(&json!({"action":"kill", "session_id":id}))
            .await;
        assert_eq!(again["status"], "already_exited");
        assert_eq!(again["exit_code"], -libc::SIGTERM);
    }
}
