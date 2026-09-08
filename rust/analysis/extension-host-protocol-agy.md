# Hermes Extension Host Protocol Specification: Plugin Prompts and External Memory

## 1. Executive Architecture and Seam Boundaries

The Hermes Rust gateway rewrite reaches a critical capability seam: running native LLM chat turns while retaining full compatibility with existing Python plugin prompt sections and configured Python external memory providers (such as Honcho, Hindsight, Mem0, Holographic, Supermemory, ByteRover, RetainDB, and OpenViking).

### 1.1 The Persistent Subprocess Architecture

In previous analysis checkpoints (recorded in PORT.md and rust/analysis/native-plugin-memory-manager-map-*.md), two alternatives were rejected:
1. Manifest-only provider descriptors: A static manifest declaring provider prompt text and tool schemas without execution capabilities violates system honesty. It advertises tools to the model that have no native implementation of handle_tool_call, directly contradicting the bug fix in issue #81014.
2. Full native Rust rewrites of third-party memory providers: Bundled providers depend on proprietary Python SDKs, local Python SQLite bindings, daemon architectures, or complex authentication layers. Reimplementing eight external memory providers natively in Rust is outside current gateway scope.

The solution is an out-of-process Python extension host communicating over a line-delimited JSON protocol across standard input and output pipes. The extension host is persistent rather than spawned per-turn:
- Cold startup overhead of a Python environment importing third-party libraries (httpx, chromadb, pydantic, sqlite3) ranges from 400ms to 2500ms.
- A persistent subprocess avoids startup latency on every conversation turn. After initialization, line-delimited JSON round trips over anonymous OS pipes complete in 1ms to 3ms.

### 1.2 Byte-Neutrality and Zero-Overhead Default

When neither plugin prompt sections nor external memory providers are configured (the default out-of-the-box installation):
- No subprocess is spawned.
- The fresh system prompt builder in conversation_prompt.rs produces bytes identical to current native output.
- Memory snapshotting and tool availability remain strictly local.
- The existing verbatim prompt reuse golden tests (e.g. main.rs:1041) pass unmodified.

---

## 2. Line-Delimited JSON Protocol Specification

The protocol uses UTF-8 line-delimited JSON (JSONL). Every request from Rust to Python and every response from Python to Rust consists of a single line terminated by an ASCII newline (\n, 0x0A).

### 2.1 Envelope Structure

All messages conform to a predictable RPC envelope:

#### Request Envelope
```json
{
  "id": 1,
  "method": "initialize",
  "params": {}
}
```

#### Success Response Envelope
```json
{
  "id": 1,
  "result": {}
}
```

#### Error Response Envelope
```json
{
  "id": 1,
  "error": {
    "code": 4001,
    "message": "Human-readable failure description",
    "data": null
  }
}
```

### 2.2 Standard Error Codes

| Code | Label | Meaning |
| :--- | :--- | :--- |
| -32700 | ParseError | Invalid JSON was received by the extension host |
| -32600 | InvalidRequest | The request object did not match the expected schema |
| -32601 | MethodNotFound | The method does not exist or is not supported |
| -32602 | InvalidParams | Invalid parameters for the requested method |
| 4001 | InitializationFailed | Plugin discovery or memory provider initialization raised |
| 4002 | ProviderUnavailable | Configured memory provider is not installed or configured |
| 4003 | ToolExecutionFailed | Provider handle_tool_call threw an unhandled exception |
| 4004 | ToolNotFound | The requested tool name is not registered on the provider |
| 4005 | ShutdownError | Errors occurred during provider cleanup or queue flushing |

---

## 3. Exact Request and Response Shapes

The protocol defines exactly three operational methods: initialize, call_tool, and shutdown.

### 3.1 Initialize (`method: "initialize"`)

Sent once by Rust when building or acquiring a conversation client for a given session. It captures the complete environment, resolves the selected memory provider, runs plugin prompt section callbacks with session metadata, and initializes the memory provider instance.

#### Request Shape
```json
{
  "id": 1,
  "method": "initialize",
  "params": {
    "protocol_version": 1,
    "session_id": "c7a2b9e4-8f1d-4e92-b36a-2d93e1150f88",
    "profile_name": "default",
    "hermes_home": "/home/user/.hermes",
    "cwd": "/home/user/project",
    "model": "anthropic/claude-3-5-sonnet",
    "provider": "openrouter",
    "platform": "cli",
    "memory_provider": "honcho",
    "memory_config": {
      "provider": "honcho",
      "memory_enabled": true,
      "user_profile_enabled": true
    },
    "credentials": {
      "HONCHO_API_KEY": "secret_key_value",
      "HONCHO_APP_ID": "app_id_value"
    },
    "enabled_toolsets": ["memory", "standard"],
    "disabled_toolsets": [],
    "session_title": "Project Setup",
    "user_id": "user_12345",
    "user_id_alt": null,
    "chat_id": null
  }
}
```

#### Response Shape (Success with Active Provider and Plugin Sections)
```json
{
  "id": 1,
  "result": {
    "status": "ok",
    "plugin_sections": [
      {
        "id": "project_rules",
        "content": "Rules: execute turns deterministically and avoid hallucination."
      }
    ],
    "memory": {
      "provider_name": "honcho",
      "is_available": true,
      "system_prompt_block": "External Memory (Honcho): User prefers Python 3.12 and Rust 2021.",
      "tools": [
        {
          "name": "honcho_context",
          "description": "Query Honcho external memory for past dialogue context.",
          "parameters": {
            "type": "object",
            "properties": {
              "query": {
                "type": "string",
                "description": "Search query for memory recall"
              }
            },
            "required": ["query"]
          }
        }
      ]
    }
  }
}
```

#### Response Shape (No External Memory Configured, Empty Plugins)
```json
{
  "id": 1,
  "result": {
    "status": "ok",
    "plugin_sections": [],
    "memory": null
  }
}
```

#### Response Shape (Gated Memory Toolset)
When disabled_toolsets contains "memory" or enabled_toolsets omits it:
```json
{
  "id": 1,
  "result": {
    "status": "ok",
    "plugin_sections": [
      {
        "id": "project_rules",
        "content": "Rules: execute turns deterministically."
      }
    ],
    "memory": null
  }
}
```

### 3.2 Memory Tool Call (`method: "call_tool"`)

Invoked synchronously within the native agent's tool execution loop whenever the LLM selects a tool advertised by the external memory provider.

#### Request Shape
```json
{
  "id": 2,
  "method": "call_tool",
  "params": {
    "tool_name": "honcho_context",
    "arguments": {
      "query": "user preferred programming languages"
    },
    "session_id": "c7a2b9e4-8f1d-4e92-b36a-2d93e1150f88"
  }
}
```

#### Response Shape (Success)
```json
{
  "id": 2,
  "result": {
    "output": "{\"success\": true, \"matches\": [\"User stated preference for Rust and Python.\"]}"
  }
}
```

#### Response Shape (Provider Failure)
```json
{
  "id": 2,
  "error": {
    "code": 4003,
    "message": "Honcho API request failed: HTTP 401 Unauthorized",
    "data": {
      "tool_name": "honcho_context"
    }
  }
}
```

### 3.3 Shutdown (`method: "shutdown"`)

Initiates clean, graceful termination of the subprocess. Instructs the Python memory provider to flush queues, close database connections, and signal daemon threads to stop.

#### Request Shape
```json
{
  "id": 3,
  "method": "shutdown",
  "params": {
    "graceful_timeout_s": 2.0
  }
}
```

#### Response Shape
```json
{
  "id": 3,
  "result": {
    "status": "ok",
    "drained": true
  }
}
```

---

## 4. Stdout Contamination Defense

A fundamental vulnerability of line-delimited JSON subprocess protocols over stdout is stdout contamination. Python dependencies (e.g. httpx, requests, chromadb, urllib3, SQLite drivers), user plugins, or legacy libraries frequently write diagnostic banners, deprecation notices, or stray print() calls directly to standard output. A single stray character on stdout corrupts JSONL deserialization.

To guarantee production resilience, a two-layer defense is required:

### 4.1 Layer 1: Python-Side File Descriptor Redirection

Before any external import occurs in the Python extension host entry point, the real standard output file descriptor (fd 1) is duplicated to a dedicated protocol descriptor, and fd 1 is redirected to standard error (fd 2):

```python
import os
import sys

# 1. Duplicate original stdout (fd 1) to a dedicated protocol writer
_proto_fd = os.dup(1)
_proto_writer = os.fdopen(_proto_fd, "w", buffering=1, encoding="utf-8")

# 2. Redirect OS fd 1 to fd 2 (stderr)
os.dup2(2, 1)

# 3. Re-point sys.stdout to sys.stderr
sys.stdout = sys.stderr

def send_response(payload: dict) -> None:
    line = json.dumps(payload, separators=(",", ":"), ensure_ascii=False)
    _proto_writer.write(line + "\n")
    _proto_writer.flush()
```

Impact:
- Any third-party library calling C-level printf() to fd 1 outputs directly to stderr.
- Any Python code calling print() without file=sys.stderr outputs to stderr.
- Only explicit calls to send_response write to the original pipe captured by Rust.

### 4.2 Layer 2: Rust-Side Line Filtering and Framing Validation

On the Rust gateway side:
1. Asynchronous Stderr Drain: The child stderr is piped and consumed by a background Tokio task that forwards lines to tracing::debug!(target: "extension_host::stderr", ...).
2. Framing Validation: The stdout reader checks each line:
   - Discards empty or whitespace-only lines.
   - If a line does not begin with `{` or fails serde_json parsing, Rust logs a tracing::warn!(target: "extension_host", "Discarding contaminated stdout line: {}", line) and continues scanning for the valid JSON response rather than failing immediately.
   - Bounded Line Length: BufReader lines are bounded to 16MB to prevent unbounded memory growth from a runaway unbuffered binary dump.

---

## 5. Timeout, Crash, and Error Behavior

The protocol must never block the gateway event loop, stall HTTP handlers, or cause orphaned child processes.

### 5.1 Timeout Matrix

| Operation | Timeout | Action on Expiration |
| :--- | :--- | :--- |
| initialize | 15 seconds | Cancel task, send SIGKILL to child process group, log warning, fall back to native agent without external memory or dynamic plugin prompt sections. |
| call_tool | 60 seconds | Mark tool call as timed out, return ToolResult containing an error message to the LLM step, kill and mark subprocess as dead. |
| shutdown | 2.5 seconds | Wait for child to exit voluntarily. If still alive after 2.5s, send SIGKILL. |

### 5.2 Crash and Broken Pipe Behavior

If the child process crashes, exits unexpectedly, or closes its pipe:
1. During initialize:
   - Rust detects EOF or BrokenPipe.
   - Rust logs the exit status and any captured stderr output.
   - Rust treats the memory provider as unavailable.
   - System prompt assembly proceeds cleanly without external memory and without dynamic plugin sections. The conversation turn succeeds natively.
2. During call_tool:
   - Rust catches the broken pipe.
   - Returns Ok(ToolResult::Error("Memory provider crashed during tool execution")) to the native agent tool loop.
   - The native agent reports the error back to the model as a regular tool message (role: "tool"), allowing the LLM to complete its turn gracefully without aborting the conversation.
   - The host state is transitioned to ProcessState::Dead. Future tool calls fail fast.

### 5.3 Process State Machine

The Rust client wraps the child process in a state machine protected by an asynchronous mutex:

```
[Uninitialized]
       │  (spawn + initialize)
       ▼
    [Ready] ◄────────┐
       │             │ (tool complete)
       ├─────────────┘
       │  (call_tool)
       ▼
    [InCall]
       │
       ├──────────────────────────┐
       │ (timeout / crash / EOF)  │ (shutdown)
       ▼                          ▼
    [Dead]                    [Closed]
```

Requests are strictly serialized per conversation client: only one request is in-flight on the JSONL pipe at any given moment.

---

## 6. Per-Profile and Per-Conversation Ownership

Hermes Gateway handles multi-tenant, multi-platform, and multi-profile messaging. The ownership boundaries between profiles and conversations must remain strictly isolated.

### 6.1 Per-Profile Ownership Boundary

A profile represents an isolated identity on disk (e.g. ~/.hermes/ or ~/.hermes/profiles/work):
- Secrets and Configuration: Each profile owns its .env file, config.yaml, and custom plugins directory ($HERMES_HOME/plugins/).
- Isolation Invariant: Two different profiles must never share a Python extension host process.
- Spawn Parameters: When Rust spawns the extension host for a profile, HERMES_HOME is set to that profile's root directory, and the process working directory is anchored to the profile's resolved workspace.

### 6.2 Per-Conversation Ownership Boundary

Within a profile, multiple conversation sessions exist concurrently. In Python, MemoryProvider implementations are inherently single-session or stateful:
- Implementations store self._session_id = session_id, per-session turn counters, and in-memory document buffers.
- If multiple concurrent turns across different session IDs shared a single Python provider instance, internal session state would race and corrupt memory attribution.

Therefore, the persistent extension host is owned directly by the conversation client:
1. ConversationAgent (conversation_agent.rs) keys clients by (PathBuf, String) representing (profile_home, session_id).
2. When build_conversation_client builds a NativeAgentClient, it instantiates a dedicated ExtensionHostClient for that conversation.
3. The child process lives for the lifetime of that conversation client in memory.
4. When ConversationAgent evicts an idle conversation client via LRU/TTL policy or session reset, dropping NativeAgentClient drops ExtensionHostClient, which triggers clean subprocess termination.

---

## 7. Child Cleanup and Secret Handling

### 7.1 Child Process Cleanup Guarantees

Orphaned Python processes must not accumulate on the host system. Cleanup is guaranteed through four layers:

1. tokio::process::Command::kill_on_drop(true):
   - Configured immediately upon building the Tokio Command. If the Rust future is cancelled, if a panic occurs, or if the client is dropped, Tokio automatically issues SIGKILL to the child.
2. Process Group Isolation (process_group(0)):
   - On Unix platforms, the child is spawned in its own process group.
   - On termination or kill, signals are sent to the negative PID (-pid), terminating any subprocesses or daemons spawned by Python plugins (e.g. vector databases or CLI helpers).
3. Explicit Shutdown RPC:
   - Clean drops execute the asynchronous shutdown RPC method first, giving providers up to 2.0 seconds to flush pending SQLite or remote writes before SIGKILL.
4. Gateway Cleanup Hooks:
   - Gateway exit handlers (cgroup_cleanup.rs and shutdown_flush.rs) track active child PIDs and sweep lingering unit cgroups.

### 7.2 Secret Handling

Memory providers require API keys (e.g. HONCHO_API_KEY, HINDSIGHT_API_KEY, MEM0_API_KEY).

Security rules:
1. No Secrets in argv: Command-line arguments are visible to all users on Linux via /proc/<pid>/cmdline and ps. The extension host command line contains only python3 -m hermes_cli.extension_host.
2. Clean Subprocess Environment: The subprocess inherits standard system environment variables (PATH, PYTHONPATH, SYSTEMROOT) and profile-specific variables loaded from load_dotenv(&home.join(".env")).
3. Stdin Payload Injection: Dynamic secrets resolved by Rust's secret_scope are passed securely inside the JSON initialize request over the private stdin pipe.
4. Stdin Redaction: The extension host injects these into os.environ upon receipt and scrubs them from any subsequent error or diagnostic responses.
5. Log Scrubbing: Request envelopes logged at debug levels in Rust must redact any values within the credentials object.

---

## 8. Tool-Schema Validation and Toolset Gating

### 8.1 Tool-Schema Validation

Tools returned by the extension host in the initialize response must be strictly validated before they are exposed to the LLM:

1. Identification Validation:
   - Tool name must match regex `^[a-zA-Z0-9_-]{1,64}$`.
   - Tool name must not collide with reserved core tools (clarify, delegate_task, current_time, memory). If a collision occurs, the provider tool is discarded with a warning, preserving core tool precedence.
2. JSON Schema Conformance:
   - parameters must be a valid JSON object of type "object".
   - If parameters is missing, null, or not an object, Rust synthesizes an empty object schema {"type": "object", "properties": {}}.
3. Execution Binding:
   - For every validated schema, Rust creates a NativeExtensionTool implementing the crate::native_tools::Tool trait:
     ```rust
     pub struct NativeExtensionTool {
         spec: ToolSpec,
         client: Arc<ExtensionHostClient>,
         session_id: String,
     }
     ```
   - When the agent calls tool.call(args), NativeExtensionTool dispatches call_tool across the persistent JSONL pipe to the extension host.

### 8.2 Toolset Gating Policy (#81014 Parity)

The gate in toolset_resolution.rs (memory_provider_enabled) enforces exact Python parity:
1. If "memory" is present in disabled_toolsets, external memory is DISABLED.
2. Else if a native memory tool is present in the registered tool surface, external memory is ENABLED.
3. Else if enabled_toolsets is None, external memory is ENABLED.
4. Else if enabled_toolsets is empty, external memory is DISABLED.
5. Else if "memory" is in enabled_toolsets or any enabled toolset resolves to include "memory", external memory is ENABLED.
6. Else external memory is DISABLED.

Enforcement invariant:
When external memory is disabled by the toolset gate:
- The extension host returns memory: null.
- sections.external_memory remains None.
- Provider tools are omitted from the native agent tool surface.
- The prompt never advertises tools that the model cannot execute.

---

## 9. Prompt Persistence Ordering

To prevent context corruption across restarts and maintain ACID boundaries in SQLite (session_db.rs), prompt assembly, extension host queries, and database persistence follow a strict ordering:

```
[1. Resolve Session State from SQLite]
        │
        ▼
[2. Stored Prompt Decision: conversation_prompt::restore_or_build]
        │
        ├──────────────────────────────────────────┐
        ▼ (Prior history & runtime match)          ▼ (First turn or runtime mismatch)
[3a. VERBATIM PROMPT REUSE]                [3b. ASSEMBLE FRESH PROMPT]
  - Read prompt from SQLite                   - Spawn / Connect Extension Host
  - plugin_prompt::restore() recovers         - Send initialize RPC
    frozen plugin sections                     - Receive rendered sections & memory block
  - NO extension host call                    - Assemble Tier 1 (Identity), Tier 2
  - NO dynamic plugin execution                 (Guidance), Tier 3 (Volatile sections:
        │                                       skills -> builtin memory -> user profile ->
        │                                       external memory -> plugin sections -> footer)
        │                                          │
        ▼                                          ▼
[4. PERSIST TO SQLITE BEFORE MODEL I/O] ◄──────────┘
  - database.update_session_prompt(&session_id, prompt)
  - database.update_session_tool_names(&session_id, tool_names)
        │
        ▼
[5. EXECUTE MODEL I/O & TOOL LOOP]
  - Outgoing LLM request with assembled prompt
  - If model invokes memory tool: dispatch call_tool to Extension Host
```

Key Rules:
1. No Extension Host Calls on Resume: When a conversation is resumed with an intact stored prompt, the extension host is not invoked for prompt rendering. The prompt bytes from SQLite are attached verbatim, and plugin_prompt.rs recovers the frozen plugin snapshot without running Python code.
2. Short SQLite Transactions: Persistence calls to SessionDb are single-statement operations. No network I/O, LLM requests, or extension host IPC calls occur inside an active SQLite transaction.
3. Persistence Precedes Model I/O: The prompt and tool names are committed to SQLite before any request is sent to the LLM provider.

---

## 10. Deferred Lifecycle Methods

To keep the protocol minimal, reliable, and production-capable, advanced lifecycle methods from Python's agent/memory_provider.py and agent/memory_manager.py are explicitly deferred from this checkpoint:

| Deferred Method | Python Source Location | Reason for Deferral |
| :--- | :--- | :--- |
| prefetch() / queue_prefetch() | memory_manager.py:616-728 | Prefetch requires complex background threading with 8s timeouts and query stripping. In this minimal protocol, memory recall is handled reactively by model tool calls. |
| sync_turn() | memory_manager.py:744-800 | Asynchronous post-turn ingestion involves single-worker daemon executor queues and background network I/O. Deferred to a subsequent turn-lifecycle checkpoint. |
| on_turn_start() | memory_provider.py:254-261 | Turn counter tracking and remaining token calculations are non-essential for initial prompt rendering and tool dispatch. |
| on_session_end() | memory_provider.py:263-271 | End-of-session background LLM summarization and fact extraction are non-critical background jobs. |
| on_session_switch() | memory_provider.py:273-315 | Mid-process session rebinding for /resume or /branch is superseded by 1:1 conversation client ownership. |
| on_pre_compress() | memory_provider.py:317-327 | Checkpoint API version 2 evidence extraction prior to context compression is deferred until native context compression is ported. |
| on_memory_write() | memory_manager.py:1220-1324 | Mirroring built-in memory write tool actions (add/replace/remove) to external providers requires full core memory tool parity. |
| on_delegation() | memory_provider.py:329-340 | Passing subagent results to parent memory providers is deferred until multi-agent subagent delegation is wired natively. |
| CLI setup wizards | plugins/memory/config_schema.py | Interactive terminal wizards belong exclusively to the Python CLI. |
| Streaming context scrubbing | memory_manager.py:232-248 | Real-time stripping of memory-context tags from token deltas is deferred to streaming response filter wiring. |

---

## 11. Ordered Implementation and Test Plan

The implementation plan is structured into six focused steps across exact repository files, followed by targeted verification.

### 11.1 Step 1: Python Extension Host Entry Point

- File: agent/extension_host.py (new file)
- Responsibilities:
  1. Implement Layer 1 stdout contamination defense (os.dup, os.dup2(2, 1), redirect sys.stdout = sys.stderr).
  2. Implement standard JSONL reading loop over sys.stdin.
  3. Handle initialize:
     - Set os.environ from params["credentials"].
     - Import and invoke hermes_cli.plugins.get_plugin_manager().
     - Collect and render after_memory plugin prompt sections using session_info.
     - If params["memory_provider"] is provided and memory toolset is enabled, call plugins.memory.load_memory_provider(name).
     - Call provider.initialize(...).
     - Extract system_prompt_block() and get_tool_schemas().
     - Respond with validated sections and executable tool specs.
  4. Handle call_tool:
     - Dispatch to provider.handle_tool_call(tool_name, args).
     - Respond with output string.
  5. Handle shutdown:
     - Call provider.shutdown().
     - Exit cleanly with status 0.

### 11.2 Step 2: Rust Extension Host Subprocess Client

- File: rust/crates/hermes-gateway/src/extension_host.rs (new file)
- Responsibilities:
  1. Define request and response structs conforming to the protocol specification.
  2. Implement ExtensionHostClient:
     - Spawns python3 -m agent.extension_host with kill_on_drop(true) and process_group(0).
     - Spawns asynchronous task draining child stderr into tracing.
     - Implements Layer 2 stdout parsing with line filtering and buffer limits.
     - Implements initialize(...), call_tool(...), and shutdown().
     - Enforces timeout bounds (15s for init, 60s for tools, 2.5s for shutdown).

### 11.3 Step 3: Native Tool Adapter and Memory Manager

- File: rust/crates/hermes-gateway/src/external_memory.rs (new file)
- Responsibilities:
  1. Implement NativeExtensionTool implementing crate::native_tools::Tool.
     - In call(&self, args: &Value), invokes ExtensionHostClient::call_tool.
  2. Implement ExternalMemoryManager:
     - Stores optional Arc<ExtensionHostClient>.
     - Provides system_prompt_block() -> Option<String>.
     - Exposes tools() -> Vec<Arc<dyn Tool>>.

### 11.4 Step 4: Wire Fresh Prompt Construction

- File: rust/crates/hermes-gateway/src/conversation_prompt.rs
- Responsibilities:
  1. In FreshPromptInputs, accept optional extension_host: Option<&ExtensionHostClient>.
  2. In Initializer::build_fresh:
     - If extension host is active, insert returned plugin prompt sections into sections.plugin_sections.
     - If memory provider returned a prompt block and toolset gate passes, assign sections.external_memory = Some(block).
     - Keep output byte-identical to current baseline when extension host is absent or returns empty blocks.

### 11.5 Step 5: Wire Client Construction in Gateway Main

- File: rust/crates/hermes-gateway/src/main.rs
- Responsibilities:
  1. Register mod extension_host; and mod external_memory;.
  2. In build_conversation_client:
     - Check if config["memory"]["provider"] is configured or plugins are present.
     - If configured, spawn ExtensionHostClient and execute initialize.
     - Pass extension host into build_fresh.
     - Inject NativeExtensionTool instances into fresh_tools.
     - On client drop / shutdown, trigger clean extension host termination.

### 11.6 Step 6: Test Suite and Verification

- Test Files:
  1. rust/crates/hermes-gateway/tests/extension_host_protocol_test.rs (new integration test):
     - Mock Python Subprocess: Test against a Python test script simulating initialize, tool execution, and shutdown.
     - Stdout Contamination Test: Prove that stray print statements and stderr messages do not break JSONL communication.
     - Timeout and Crash Test: Prove that a killed or hanging subprocess times out cleanly and degrades without crashing the gateway.
     - Tool Gating Test: Verify that disabling "memory" toolset suppresses both tools and prompt block.
     - Verbatim Reuse Test: Verify that session resume restores prompt from SQLite without invoking the extension host.
  2. In-tree validation:
     - Run cargo clippy -- -D warnings.
     - Run cargo test -p hermes-gateway.
     - Verify zero regressions against the 1,492 existing passing workspace tests.
