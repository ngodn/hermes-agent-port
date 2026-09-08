//! Conversation-scoped subprocess host for legacy Python extensions.
//!
//! The Rust agent owns the model loop. Existing Python plugins remain usable
//! through one persistent JSONL child per cached conversation. The child
//! renders prompt sections only on a fresh build and keeps tool/provider state
//! alive for later calls. Scoped secrets travel only on the private stdin pipe
//! and are never included in logs, command arguments, or protocol errors.

use std::collections::HashMap;
use std::path::Path;
use std::process::Stdio;
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};
use std::time::Duration;

use async_trait::async_trait;
use hermes_core::{Error, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::{mpsc, oneshot};

const MODULE: &str = "hermes_cli.rust_extension_host";
const INITIALIZE_TIMEOUT: Duration = Duration::from_secs(60);
const SNAPSHOT_TIMEOUT: Duration = Duration::from_secs(30);
const TURN_START_TIMEOUT: Duration = Duration::from_secs(30);
const TURN_COMPLETE_TIMEOUT: Duration = Duration::from_secs(5);
const TOOL_TIMEOUT: Duration = Duration::from_secs(310);
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(7);
const MAX_RESPONSE_BYTES: usize = 4 * 1024 * 1024;
const MAX_CONTAMINATED_BYTES: usize = 64 * 1024;
const MAX_CONTAMINATED_LINES: usize = 32;

#[derive(Serialize)]
pub struct InitializeParams {
    pub home: String,
    pub session_id: String,
    pub model: String,
    pub provider: String,
    pub platform: String,
    pub profile_name: String,
    pub cwd: String,
    pub session_title: Option<String>,
    pub user_id: Option<String>,
    pub user_id_alt: Option<String>,
    pub user_name: Option<String>,
    pub chat_id: Option<String>,
    pub chat_name: Option<String>,
    pub chat_type: Option<String>,
    pub thread_id: Option<String>,
    pub gateway_session_key: Option<String>,
    pub native_tool_names: Vec<String>,
    /// Present only under a task-local profile scope. The private stdin pipe
    /// carries this snapshot so no foreign process-global secret is inherited.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub profile_secrets: Option<HashMap<String, String>>,
}

#[derive(Debug, Deserialize)]
pub struct InitializeResult {
    pub active_memory_provider: Option<String>,
    #[serde(default)]
    pub registered_plugin_tools: Vec<Value>,
    #[serde(default)]
    pub plugin_tools: Vec<Value>,
    #[serde(default)]
    pub memory_tools: Vec<Value>,
    #[serde(default)]
    pub memory_exposed: bool,
}

impl InitializeResult {
    fn convert_tools<'a>(
        definitions: impl Iterator<Item = &'a Value>,
        client: &Client,
    ) -> Vec<Arc<dyn crate::native_tools::Tool>> {
        let mut names = std::collections::HashSet::new();
        definitions
            .filter_map(|definition| Tool::from_definition(client.clone(), definition))
            .filter(|tool| names.insert(crate::native_tools::Tool::spec(tool).name))
            .map(|tool| Arc::new(tool) as Arc<dyn crate::native_tools::Tool>)
            .collect()
    }

    pub fn available_tools(&self, client: &Client) -> Vec<Arc<dyn crate::native_tools::Tool>> {
        Self::convert_tools(self.plugin_tools.iter().chain(&self.memory_tools), client)
    }

    pub fn registered_tools(&self, client: &Client) -> Vec<Arc<dyn crate::native_tools::Tool>> {
        Self::convert_tools(
            self.registered_plugin_tools
                .iter()
                .chain(&self.memory_tools),
            client,
        )
    }
}

#[derive(Debug, Default, Deserialize)]
pub struct PromptSnapshot {
    #[serde(default)]
    pub plugin_sections: Vec<crate::plugin_prompt::Section>,
    pub memory_prompt: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
pub struct TurnStartResult {
    pub api_content: Option<String>,
    pub recall_indicator: Option<String>,
}

#[derive(Clone)]
pub struct Client {
    sender: mpsc::Sender<WorkerCommand>,
    next_id: Arc<AtomicU64>,
}

struct WorkerCommand {
    request: Value,
    timeout: Duration,
    response: oneshot::Sender<Result<Value>>,
}

struct Process {
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: BufReader<ChildStdout>,
}

impl Drop for Process {
    fn drop(&mut self) {
        #[cfg(unix)]
        if let Some(pid) = self.child.id() {
            // The child owns this process group, so a cancelled gateway task
            // cannot leave plugin-spawned descendants behind.
            unsafe {
                libc::kill(-(pid as i32), libc::SIGKILL);
            }
        }
    }
}

#[derive(Deserialize)]
struct Response {
    id: u64,
    ok: bool,
    #[serde(default)]
    result: Value,
    error: Option<String>,
}

#[derive(Debug)]
enum ExchangeError {
    Remote(String),
    Transport(String),
}

impl ExchangeError {
    fn into_error(self) -> Error {
        let message = match self {
            Self::Remote(message) | Self::Transport(message) => message,
        };
        Error::Other(message)
    }

    fn is_transport(&self) -> bool {
        matches!(self, Self::Transport(_))
    }
}

impl Client {
    pub async fn spawn(
        python: &str,
        repo: &Path,
        home: &Path,
        params: InitializeParams,
    ) -> Result<(Self, InitializeResult)> {
        let mut command = Command::new(python);
        if params.profile_secrets.is_some() {
            command.env_clear();
            for (name, value) in std::env::vars_os() {
                let Some(name_text) = name.to_str() else {
                    continue;
                };
                if inherited_child_env_is_global(name_text) {
                    command.env(&name, value);
                }
            }
        }
        command
            .arg("-m")
            .arg(MODULE)
            .current_dir(repo)
            .env("HERMES_HOME", home)
            .env("PYTHONUNBUFFERED", "1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true);
        #[cfg(unix)]
        command.process_group(0);
        #[cfg(windows)]
        command.creation_flags(0x08000000);
        let mut child = command
            .spawn()
            .map_err(|error| Error::Other(format!("extension host spawn failed: {error}")))?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| Error::Other("extension host stdin was not captured".into()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| Error::Other("extension host stdout was not captured".into()))?;
        let process = Process {
            child,
            stdin: Some(stdin),
            stdout: BufReader::new(stdout),
        };
        let (sender, receiver) = mpsc::channel(16);
        tokio::spawn(run_worker(process, receiver));
        let client = Self {
            sender,
            next_id: Arc::new(AtomicU64::new(1)),
        };
        let value = client
            .request(
                "initialize",
                serde_json::to_value(params).unwrap(),
                INITIALIZE_TIMEOUT,
            )
            .await?;
        let initialized = serde_json::from_value(value)
            .map_err(|error| Error::Other(format!("extension host initialize decode: {error}")))?;
        Ok((client, initialized))
    }

    pub async fn snapshot(&self) -> Result<PromptSnapshot> {
        let value = self
            .request("snapshot", json!({}), SNAPSHOT_TIMEOUT)
            .await?;
        serde_json::from_value(value)
            .map_err(|error| Error::Other(format!("extension host snapshot decode: {error}")))
    }

    pub async fn call_tool(&self, name: &str, args: &Value) -> Result<Value> {
        self.request(
            "call_tool",
            json!({"name": name, "args": args}),
            TOOL_TIMEOUT,
        )
        .await
    }

    pub async fn turn_start(
        &self,
        user_message: &Value,
        turn_number: usize,
    ) -> Result<TurnStartResult> {
        let value = self
            .request(
                "turn_start",
                json!({"user_message": user_message, "turn_number": turn_number}),
                TURN_START_TIMEOUT,
            )
            .await?;
        serde_json::from_value(value)
            .map_err(|error| Error::Other(format!("extension host turn start decode: {error}")))
    }

    pub async fn turn_complete(
        &self,
        user_message: &Value,
        final_response: &str,
        messages: &[Value],
        interrupted: bool,
    ) -> Result<()> {
        self.request(
            "turn_complete",
            json!({
                "user_message": user_message,
                "final_response": final_response,
                "interrupted": interrupted,
                "messages": messages,
            }),
            TURN_COMPLETE_TIMEOUT,
        )
        .await
        .map(|_| ())
    }

    async fn request(&self, method: &str, params: Value, timeout: Duration) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let request = json!({"id": id, "method": method, "params": params});
        let (response, receiver) = oneshot::channel();
        self.sender
            .send(WorkerCommand {
                request,
                timeout,
                response,
            })
            .await
            .map_err(|_| Error::Other("extension host is not running".into()))?;
        receiver
            .await
            .map_err(|_| Error::Other("extension host response channel closed".into()))?
    }
}

/// Keep OS/interpreter settings and values that Hermes already classifies as
/// process-global. Everything else is supplied by the selected profile scope.
fn inherited_child_env_is_global(name: &str) -> bool {
    crate::secret_scope::is_global_env(name)
        || matches!(
            name,
            "LOGNAME"
                | "HOSTNAME"
                | "PYTHONHOME"
                | "PYTHONIOENCODING"
                | "PYTHONUTF8"
                | "SSL_CERT_DIR"
                | "REQUESTS_CA_BUNDLE"
                | "CURL_CA_BUNDLE"
                | "HTTP_PROXY"
                | "HTTPS_PROXY"
                | "ALL_PROXY"
                | "NO_PROXY"
                | "http_proxy"
                | "https_proxy"
                | "all_proxy"
                | "no_proxy"
                | "SYSTEMROOT"
                | "WINDIR"
                | "USERPROFILE"
                | "HOMEDRIVE"
                | "HOMEPATH"
                | "LOCALAPPDATA"
                | "APPDATA"
                | "COMSPEC"
                | "PATHEXT"
                | "TEMP"
                | "TMP"
                | "HERMES_SAFE_MODE"
                | "HERMES_ENABLE_PROJECT_PLUGINS"
                | "HERMES_BUNDLED_PLUGINS"
        )
        || name.starts_with("LC_")
        || name.starts_with("XDG_")
        || name.starts_with("DYLD_")
        || name == "LD_LIBRARY_PATH"
}

async fn run_worker(mut process: Process, mut receiver: mpsc::Receiver<WorkerCommand>) {
    let mut healthy = true;
    while let Some(command) = receiver.recv().await {
        let (result, transport_failed) = if healthy {
            match exchange(&mut process, &command.request, command.timeout).await {
                Ok(value) => (Ok(value), false),
                Err(error) => {
                    let fatal = error.is_transport();
                    (Err(error.into_error()), fatal)
                }
            }
        } else {
            (
                Err(Error::Other("extension host is not running".into())),
                false,
            )
        };
        let _ = command.response.send(result);
        if transport_failed {
            healthy = false;
            kill_process_tree(&mut process.child).await;
            break;
        }
    }

    if healthy {
        let request = json!({"id": 0, "method": "shutdown", "params": {}});
        let _ = exchange(&mut process, &request, SHUTDOWN_TIMEOUT).await;
    }
    process.stdin.take();
    if tokio::time::timeout(SHUTDOWN_TIMEOUT, process.child.wait())
        .await
        .is_err()
    {
        kill_process_tree(&mut process.child).await;
    }
}

async fn exchange(
    process: &mut Process,
    request: &Value,
    timeout: Duration,
) -> std::result::Result<Value, ExchangeError> {
    let expected_id = request["id"]
        .as_u64()
        .ok_or_else(|| ExchangeError::Transport("extension host request id is invalid".into()))?;
    let line = serde_json::to_vec(request).map_err(|error| {
        ExchangeError::Transport(format!("extension host request encode: {error}"))
    })?;
    let operation = async {
        let stdin = process
            .stdin
            .as_mut()
            .ok_or_else(|| ExchangeError::Transport("extension host stdin is closed".into()))?;
        stdin.write_all(&line).await.map_err(|error| {
            ExchangeError::Transport(format!("extension host write failed: {error}"))
        })?;
        stdin.write_all(b"\n").await.map_err(|error| {
            ExchangeError::Transport(format!("extension host write failed: {error}"))
        })?;
        stdin.flush().await.map_err(|error| {
            ExchangeError::Transport(format!("extension host flush failed: {error}"))
        })?;

        let mut contaminated_bytes = 0;
        let mut contaminated_lines = 0;
        loop {
            let mut response_line = Vec::new();
            let read = (&mut process.stdout)
                .take((MAX_RESPONSE_BYTES + 1) as u64)
                .read_until(b'\n', &mut response_line)
                .await
                .map_err(|error| {
                    ExchangeError::Transport(format!("extension host read failed: {error}"))
                })?;
            if read == 0 {
                return Err(ExchangeError::Transport(
                    "extension host exited without a response".into(),
                ));
            }
            if response_line.len() > MAX_RESPONSE_BYTES || !response_line.ends_with(b"\n") {
                return Err(ExchangeError::Transport(
                    "extension host response exceeded its limit".into(),
                ));
            }
            if response_line.iter().all(u8::is_ascii_whitespace) {
                continue;
            }
            let response: Response = match serde_json::from_slice(&response_line) {
                Ok(response) => response,
                Err(_) => {
                    contaminated_bytes += response_line.len();
                    contaminated_lines += 1;
                    tracing::warn!(
                        contaminated_lines,
                        "Discarding non-protocol extension-host stdout"
                    );
                    if contaminated_bytes > MAX_CONTAMINATED_BYTES
                        || contaminated_lines > MAX_CONTAMINATED_LINES
                    {
                        return Err(ExchangeError::Transport(
                            "extension host stdout contamination exceeded its limit".into(),
                        ));
                    }
                    continue;
                }
            };
            if response.id != expected_id {
                return Err(ExchangeError::Transport(
                    "extension host response id mismatch".into(),
                ));
            }
            if !response.ok {
                return Err(ExchangeError::Remote(
                    response
                        .error
                        .unwrap_or_else(|| "extension host request failed".into()),
                ));
            }
            return Ok(response.result);
        }
    };
    match tokio::time::timeout(timeout, operation).await {
        Ok(result) => result,
        Err(_) => {
            kill_process_tree(&mut process.child).await;
            Err(ExchangeError::Transport(
                "extension host request timed out".into(),
            ))
        }
    }
}

async fn kill_process_tree(child: &mut Child) {
    let pid = child.id();
    #[cfg(unix)]
    if let Some(pid) = pid {
        unsafe {
            libc::kill(-(pid as i32), libc::SIGKILL);
        }
    }
    #[cfg(windows)]
    if let Some(pid) = pid {
        let mut taskkill = Command::new("taskkill");
        taskkill
            .args(["/T", "/F", "/PID", &pid.to_string()])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .creation_flags(0x08000000);
        let _ = tokio::time::timeout(Duration::from_secs(2), taskkill.status()).await;
    }
    let _ = child.start_kill();
    let _ = child.wait().await;
}

struct Tool {
    client: Client,
    function: Map<String, Value>,
}

impl Tool {
    fn from_definition(client: Client, definition: &Value) -> Option<Self> {
        if definition.get("type").and_then(Value::as_str) != Some("function") {
            return None;
        }
        let mut function = definition.get("function")?.as_object()?.clone();
        let name = function.get("name")?.as_str()?;
        if name.is_empty()
            || name.len() > 64
            || !name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
        {
            return None;
        }
        if !function.get("parameters").is_some_and(Value::is_object) {
            function.insert(
                "parameters".into(),
                json!({"type": "object", "properties": {}}),
            );
        } else if function["parameters"]["type"] != "object" {
            function["parameters"]["type"] = json!("object");
        }
        Some(Self { client, function })
    }
}

#[async_trait]
impl crate::native_tools::Tool for Tool {
    fn spec(&self) -> crate::native_tools::ToolSpec {
        let mut extra = self.function.clone();
        let name = extra
            .shift_remove("name")
            .and_then(|value| value.as_str().map(str::to_owned))
            .unwrap_or_default();
        let description = extra
            .shift_remove("description")
            .and_then(|value| value.as_str().map(str::to_owned))
            .unwrap_or_default();
        let parameters = extra
            .shift_remove("parameters")
            .unwrap_or_else(|| json!({"type": "object", "properties": {}}));
        crate::native_tools::ToolSpec {
            name,
            description,
            parameters,
            extra,
        }
    }

    async fn call(&self, args: &Value) -> Result<Value> {
        let name = self
            .function
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or_default();
        self.client.call_tool(name, args).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::native_tools::Tool as _;

    struct TempHome(std::path::PathBuf);

    impl TempHome {
        fn new(label: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "hermes-extension-host-{label}-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TempHome {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn extension_tool_preserves_extra_schema_fields() {
        let (sender, _receiver) = mpsc::channel(1);
        let client = Client {
            sender,
            next_id: Arc::new(AtomicU64::new(1)),
        };
        let tool = Tool::from_definition(
            client,
            &json!({"type":"function","function":{
                "name":"fixture", "description":"test", "parameters":{"type":"object"},
                "strict":true, "x-provider":"kept"
            }}),
        )
        .unwrap();
        let spec = tool.spec();
        assert_eq!(spec.name, "fixture");
        assert_eq!(spec.extra["strict"], true);
        assert_eq!(spec.extra["x-provider"], "kept");
        assert_eq!(
            crate::native_tools::tool_spec_json(&spec)["function"]["strict"],
            true
        );

        let (sender, _receiver) = mpsc::channel(1);
        let client = Client {
            sender,
            next_id: Arc::new(AtomicU64::new(1)),
        };
        assert!(Tool::from_definition(
            client.clone(),
            &json!({"type":"function","function":{"name":"bad name"}}),
        )
        .is_none());
        let normalized = Tool::from_definition(
            client.clone(),
            &json!({"type":"function","function":{"name":"valid_name","parameters":null}}),
        )
        .unwrap();
        assert_eq!(
            normalized.spec().parameters,
            json!({"type":"object","properties":{}})
        );
        let normalized = Tool::from_definition(
            client,
            &json!({"type":"function","function":{"name":"valid_name","parameters":{}}}),
        )
        .unwrap();
        assert_eq!(normalized.spec().parameters, json!({"type":"object"}));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn protocol_reader_skips_bounded_stdout_contamination() {
        let repo = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../..")
            .canonicalize()
            .unwrap();
        let python = repo.join(".venv/bin/python");
        let script = r#"import json
import sys
request = json.loads(sys.stdin.readline())
sys.stdout.write("\nplugin diagnostic\n")
sys.stdout.write(json.dumps({"id": request["id"], "ok": True,
                             "result": {"accepted": True}}) + "\n")
sys.stdout.flush()
sys.stdin.readline()
"#;
        let mut child = Command::new(python)
            .args(["-c", script])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let mut process = Process {
            child,
            stdin: Some(stdin),
            stdout: BufReader::new(stdout),
        };
        let value = exchange(
            &mut process,
            &json!({"id":41,"method":"fixture","params":{}}),
            Duration::from_secs(2),
        )
        .await
        .unwrap();
        assert_eq!(value, json!({"accepted":true}));
        kill_process_tree(&mut process.child).await;
    }

    #[test]
    fn real_python_host_renders_and_executes_then_shuts_down() {
        let _environment_lock = crate::secret_scope::GLOBAL_TEST_LOCK.lock().unwrap();
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
        struct RestoreEnv {
            name: &'static str,
            value: Option<std::ffi::OsString>,
        }
        impl Drop for RestoreEnv {
            fn drop(&mut self) {
                match self.value.take() {
                    Some(value) => std::env::set_var(self.name, value),
                    None => std::env::remove_var(self.name),
                }
            }
        }
        let _restore = RestoreEnv {
            name: "FOREIGN_EXTENSION_SECRET",
            value: std::env::var_os("FOREIGN_EXTENSION_SECRET"),
        };
        let _restore_setting = RestoreEnv {
            name: "FOREIGN_EXTENSION_SETTING",
            value: std::env::var_os("FOREIGN_EXTENSION_SETTING"),
        };
        std::env::set_var("FOREIGN_EXTENSION_SECRET", "must-not-cross-profile");
        std::env::set_var("FOREIGN_EXTENSION_SETTING", "must-not-cross-profile");
        let home = TempHome::new("real");
        let plugin = home.0.join("plugins/fixture-extension");
        std::fs::create_dir_all(&plugin).unwrap();
        std::fs::write(
            home.0.join("config.yaml"),
            "plugins:\n  enabled: [fixture-extension]\nmemory:\n  provider: fixture-extension\nplatform_toolsets:\n  cli: [fixture, memory]\n",
        )
        .unwrap();
        std::fs::write(home.0.join(".env"), "FIXTURE_PROFILE_SECRET=wrong-dotenv\n").unwrap();
        std::fs::write(
            plugin.join("plugin.yaml"),
            "name: fixture-extension\nversion: 1.0.0\nkind: exclusive\n",
        )
        .unwrap();
        std::fs::write(
            plugin.join("__init__.py"),
r##"import json
import os
import time
from pathlib import Path
from agent.memory_provider import MemoryProvider

class FixtureProvider(MemoryProvider):
    name = "fixture-extension"
    def _record(self, event, **fields):
        with Path(self.home, "extension-lifecycle-" + self.session_id).open("a") as stream:
            stream.write(json.dumps({"event": event, **fields}, sort_keys=True) + "\n")
    def is_available(self): return True
    def initialize(self, session_id, **kwargs):
        import os
        self.session_id = session_id
        self.home = kwargs["hermes_home"]
        self.cwd = os.getcwd()
        self.gateway_session_key = kwargs.get("gateway_session_key", "")
        self.profile_secret_ok = os.environ.get("FIXTURE_PROFILE_SECRET") == "right-profile"
        self.foreign_secret_absent = (
            "FOREIGN_EXTENSION_SECRET" not in os.environ and
            "FOREIGN_EXTENSION_SETTING" not in os.environ
        )
    def on_turn_start(self, turn_number, message, **kwargs):
        self._record("turn_start", turn_number=turn_number, message=message)
    def prefetch(self, query, **kwargs):
        self._record("prefetch", query=query, session_id=kwargs.get("session_id", ""))
        return "remembered fact for " + query
    def sync_turn(self, user_content, assistant_content, **kwargs):
        self._record("sync", user=user_content, assistant=assistant_content,
                     session_id=kwargs.get("session_id", ""),
                     messages=kwargs.get("messages"))
    def queue_prefetch(self, query, **kwargs):
        self._record("queue_prefetch", query=query,
                     session_id=kwargs.get("session_id", ""))
    def on_session_end(self, messages, **kwargs):
        self._record("session_end", messages=messages)
    def system_prompt_block(self):
        return "# Fixture Memory\nSession " + self.session_id
    def get_tool_schemas(self):
        return [
            {"name":"fixture_memory_tool","description":"memory fixture","parameters":{"type":"object"},"strict":True},
            {"name":"fixture_plugin_tool","description":"memory collision","parameters":{"type":"object"}},
        ]
    def handle_tool_call(self, tool_name, args, **kwargs):
        return json.dumps({"session": self.session_id, "cwd": self.cwd, "args": args,
                           "gateway_session_key": self.gateway_session_key,
                           "profile_secret_ok": self.profile_secret_ok,
                           "foreign_secret_absent": self.foreign_secret_absent}, sort_keys=True)
    def shutdown(self):
        Path(self.home, "extension-shutdown-" + self.session_id).write_text(self.session_id)

def plugin_tool(args, **kwargs):
    if args.get("sleep"):
        time.sleep(2)
    return {"_multimodal": True, "content": [{"type":"text", "text":
            kwargs.get("session_id", "") + ":" + args.get("text", "")} ]}

def register(ctx):
    print("plugin stdout must not enter the protocol")
    os.write(1, b"native fd stdout must not enter the protocol\n")
    ctx.register_memory_provider(FixtureProvider())
    ctx.register_system_prompt_section(
        "fixture.rules", lambda info: "Plugin session " + info["session_id"]
    )
    ctx.register_tool(
        name="fixture_plugin_tool", toolset="fixture",
        schema={"description":"plugin fixture","parameters":{"type":"object"},"strict":True},
        handler=plugin_tool,
    )
    ctx.register_tool(
        name="current_time", toolset="fixture",
        schema={"description":"unapproved native collision","parameters":{"type":"object"}},
        handler=plugin_tool,
    )
"##,
        )
        .unwrap();

        let repo = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../..")
            .canonicalize()
            .unwrap();
        let python = repo.join(".venv/bin/python");
        let params = InitializeParams {
            home: home.0.to_string_lossy().into_owned(),
            session_id: "session-one".into(),
            model: "fixture-model".into(),
            provider: "fixture-provider".into(),
            platform: "cli".into(),
            profile_name: "default".into(),
            cwd: home.0.to_string_lossy().into_owned(),
            session_title: Some("Fixture".into()),
            user_id: Some("user-one".into()),
            user_id_alt: None,
            user_name: None,
            chat_id: Some("chat-one".into()),
            chat_name: None,
            chat_type: Some("private".into()),
            thread_id: None,
            gateway_session_key: Some("cli:chat-one".into()),
            native_tool_names: vec!["current_time".into()],
            profile_secrets: Some(HashMap::from([(
                "FIXTURE_PROFILE_SECRET".into(),
                "right-profile".into(),
            )])),
        };
        let (client, initialized) = Client::spawn(python.to_str().unwrap(), &repo, &home.0, params)
            .await
            .unwrap();
        assert_eq!(
            initialized.active_memory_provider.as_deref(),
            Some("fixture-extension")
        );
        assert!(initialized.memory_exposed);
        let tools = initialized.available_tools(&client);
        assert_eq!(
            crate::native_tools::tool_names(&tools),
            ["fixture_plugin_tool", "fixture_memory_tool"]
        );
        assert_eq!(
            crate::native_tools::tool_spec_json(&tools[0].spec())["function"]["strict"],
            true
        );
        assert_eq!(tools[0].spec().description, "plugin fixture");

        let snapshot = client.snapshot().await.unwrap();
        assert_eq!(
            snapshot.plugin_sections,
            [crate::plugin_prompt::Section {
                id: "fixture.rules".into(),
                content: "Plugin session session-one".into(),
            }]
        );
        assert_eq!(
            snapshot.memory_prompt.as_deref(),
            Some("# Fixture Memory\nSession session-one")
        );
        let prepared = client
            .turn_start(&json!("Where were we?"), 3)
            .await
            .unwrap();
        let api_content = prepared.api_content.unwrap();
        assert!(api_content.starts_with("Where were we?\n\n<memory-context>\n"));
        assert!(api_content.contains("remembered fact for Where were we?"));
        assert!(api_content.ends_with("\n</memory-context>"));
        assert!(client
            .turn_start(&json!("thanks"), 4)
            .await
            .unwrap()
            .api_content
            .is_none());
        assert_eq!(
            tools[0].call(&json!({"text":"image text"})).await.unwrap(),
            json!({"_multimodal":true,"content":[{"type":"text","text":"session-one:image text"}]})
        );
        let memory_result = tools[1].call(&json!({"query":"remember"})).await.unwrap();
        let memory_result: Value =
            serde_json::from_str(memory_result.as_str().unwrap()).unwrap();
        assert_eq!(memory_result["session"], "session-one");
        assert_eq!(memory_result["cwd"], home.0.to_string_lossy().as_ref());
        assert_eq!(memory_result["gateway_session_key"], "cli:chat-one");
        assert_eq!(memory_result["profile_secret_ok"], true);
        assert_eq!(memory_result["foreign_secret_absent"], true);
        assert_eq!(memory_result["args"], json!({"query":"remember"}));

        client
            .turn_complete(&json!("discarded"), "partial", &[], true)
            .await
            .unwrap();
        client
            .request(
                "turn_complete",
                json!({"user_message":"discarded", "final_response":"", "interrupted":false}),
                Duration::from_secs(1),
            )
            .await
            .unwrap();
        client
            .turn_complete(
                &json!([{"type":"text", "text":"User text"}, {"type":"image_url", "image_url":{"url":"data:image/png;base64,AA=="}}]),
                "Assistant text",
                &[json!({"role":"user", "content":"User text"}), json!({"role":"assistant", "content":"Assistant text"})],
                false,
            )
            .await
            .unwrap();
        assert_eq!(
            client
                .request(
                    "flush_pending",
                    json!({"timeout": 2.0}),
                    Duration::from_secs(3),
                )
                .await
                .unwrap(),
            Value::Bool(true)
        );
        let ended = [json!({"role":"user", "content":"User text"}), json!({"role":"assistant", "content":"Assistant text"})];
        client
            .request(
                "session_end",
                json!({"messages": ended}),
                Duration::from_secs(15),
            )
            .await
            .unwrap();
        let lifecycle: Vec<Value> = std::fs::read_to_string(
            home.0.join("extension-lifecycle-session-one"),
        )
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
        assert_eq!(lifecycle[0], json!({"event":"turn_start", "message":"Where were we?", "turn_number":3}));
        assert_eq!(lifecycle[1]["event"], "prefetch");
        assert_eq!(lifecycle[2], json!({"event":"turn_start", "message":"thanks", "turn_number":4}));
        assert_eq!(lifecycle[3]["event"], "sync");
        assert_eq!(lifecycle[3]["user"], "[1 image] User text");
        assert_eq!(lifecycle[4]["event"], "queue_prefetch");
        assert_eq!(lifecycle[5]["event"], "session_end");

        drop(tools);
        drop(client);
        for _ in 0..100 {
            if home.0.join("extension-shutdown-session-one").is_file() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        assert_eq!(
            std::fs::read_to_string(home.0.join("extension-shutdown-session-one")).unwrap(),
            "session-one"
        );

        std::fs::write(
            home.0.join("config.yaml"),
            "plugins:\n  enabled: [fixture-extension]\nmemory:\n  provider: fixture-extension\nplatform_toolsets:\n  cli: [fixture, memory]\nagent:\n  disabled_toolsets: [memory]\n",
        )
        .unwrap();
        let gated_params = InitializeParams {
            home: home.0.to_string_lossy().into_owned(),
            session_id: "session-gated".into(),
            model: "fixture-model".into(),
            provider: "fixture-provider".into(),
            platform: "cli".into(),
            profile_name: "default".into(),
            cwd: home.0.to_string_lossy().into_owned(),
            session_title: None,
            user_id: None,
            user_id_alt: None,
            user_name: None,
            chat_id: None,
            chat_name: None,
            chat_type: None,
            thread_id: None,
            gateway_session_key: None,
            native_tool_names: vec!["current_time".into()],
            profile_secrets: Some(HashMap::new()),
        };
        let (gated_client, gated) =
            Client::spawn(python.to_str().unwrap(), &repo, &home.0, gated_params)
                .await
                .unwrap();
        assert_eq!(
            gated.active_memory_provider.as_deref(),
            Some("fixture-extension")
        );
        assert!(!gated.memory_exposed);
        assert_eq!(
            crate::native_tools::tool_names(&gated.available_tools(&gated_client)),
            ["fixture_plugin_tool"]
        );
        let gated_snapshot = gated_client.snapshot().await.unwrap();
        assert!(gated_snapshot.memory_prompt.is_none());
        assert_eq!(gated_snapshot.plugin_sections.len(), 1);
        let remote_error = gated_client
            .request(
                "call_tool",
                json!({"name":"not_registered","args":{}}),
                Duration::from_secs(1),
            )
            .await
            .unwrap_err();
        assert!(remote_error.to_string().contains("request failed"));
        assert_eq!(
            gated_client.snapshot().await.unwrap().plugin_sections.len(),
            1
        );
        let timeout = gated_client
            .request(
                "call_tool",
                json!({"name":"fixture_plugin_tool","args":{"sleep":true}}),
                Duration::from_millis(25),
            )
            .await
            .unwrap_err();
        assert!(timeout.to_string().contains("timed out"));
        let unavailable = gated_client
            .call_tool("fixture_plugin_tool", &json!({}))
            .await
            .unwrap_err();
        assert!(unavailable.to_string().contains("not running"));
                drop(gated_client);
            });
    }
}
