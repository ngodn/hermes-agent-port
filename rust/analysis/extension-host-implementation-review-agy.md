# Hermes Extension Host Implementation Review

## Executive Summary

This review analyzes the uncommitted extension-host implementation for the Hermes Rust gateway rewrite across the Python child (`hermes_cli/rust_extension_host.py`), the Rust client and supervisor (`rust/crates/hermes-gateway/src/extension_host.rs`), and the integration seams (`rust/crates/hermes-gateway/src/{main.rs,conversation_prompt.rs,plugin_prompt.rs,native_tools.rs,native_agent.rs,message.rs}`).

The implementation achieves several key architectural goals: Layer 1 stdout redirection safely routes Python-level and C-level writes to stderr before loading plugins; prompt callbacks execute only on fresh prompt builds and are skipped during cached prompt reuse; external-memory toolset gating properly suppresses both tool schemas and system prompt blocks when disabled; and per-conversation ownership boundaries isolate state across sessions.

However, several critical defects exist in the current revision:
1. Application-level RPC errors permanently mark the supervisor worker as dead, bricking all subsequent tool calls in that conversation.
2. Structured multimodal tool results (`{"_multimodal": True, ...}`) pass through the native tool loop as raw JSON objects instead of being unwrapped into valid content parts, causing HTTP 400 errors from LLM providers.
3. Layer 2 stdout line filtering is completely missing in Rust; any stray newline or pre-redirection banner causes fatal JSON parse failure.
4. Python's `load_hermes_dotenv` with `override=True` clobbers task-local `profile_secrets` sent by the gateway because the child does not activate multiplexed mode.
5. `merge_native_tools` reverses the established built-in precedence contract by replacing native tools with extension tools.
6. A collision routing mismatch exists between schema advertisement (where plugins take precedence) and execution dispatch (where memory providers take precedence).

Findings are ranked below by severity with exact line anchors, followed by confirmed areas with no defect and the exact focused tests still missing. No em dash character is used in this report.

---

## Ranked Findings: Confirmed Bugs

### Finding 1 (Critical): Worker Permanently Marked Dead on Application-Level Error
- File and Line: `rust/crates/hermes-gateway/src/extension_host.rs:237-243`
- Description: In `run_worker`, the loop processes incoming commands and updates its health flag:
  ```rust
  let result = if healthy {
      exchange(&mut process, &command.request, command.timeout).await
  } else {
      Err(Error::Other("extension host is not running".into()))
  };
  healthy &= result.is_ok();
  let _ = command.response.send(result);
  ```
  In `exchange` (lines 304-310), if the Python host returns an application-level error (`"ok": false`), `exchange` converts it into `Err(Error::Other(...))`.
  Because `result.is_ok()` is false, `healthy` becomes `false`.
- Impact: If a single tool call fails at the application level (for instance, an unknown tool name passed by the model, invalid argument types, or an unhandled exception in a plugin handler), the extension host supervisor permanently marks itself dead. Every subsequent tool call during that session immediately fails with `"extension host is not running"`. Furthermore, when the client is dropped, lines 246-249 skip sending a clean `shutdown` RPC because `healthy` is false.
- Remediation: Distinguish transport/framing failures (I/O error, EOF, timeout, corrupt JSON) from application-level response failures (`ok: false`). `healthy` must remain `true` when a valid JSON response envelope with `ok: false` is received.

### Finding 2 (High): Multimodal Tool Results Emit Invalid Object Schema to LLM Provider
- File and Line: `rust/crates/hermes-gateway/src/native_tools.rs:672-687`, `rust/crates/hermes-gateway/src/tool_result.rs:265-270`, `hermes_cli/rust_extension_host.py:269`
- Description: `Tool::call` was updated to return `Result<Value>`. In `rust_extension_host.py:269`, plugin handlers dispatched through `tools.registry.registry` can return a multimodal result envelope: `{"_multimodal": True, "content": [{"type": "text", "text": "..."}]}` (see `tools/registry.py:1145-1150` and the test fixture in `extension_host.rs:542, 618`).
  In Python (`run_agent.py:7943-7980`), `_tool_result_content_for_active_model` unwraps the `_multimodal` dict into an OpenAI-style content array `result["content"]` (or summarizes to string for text-only models) before building the tool result message.
  In Rust, `native_tools.rs:684-694` passes `content` directly to `tool_result::build`. Inside `tool_result.rs:265-270`, `maybe_wrap_untrusted` handles `Value::String` and `Value::Array`, but falls through on `Value::Object`, returning the raw envelope object untouched.
- Impact: The message appended to the LLM conversation history has `"role": "tool", "content": {"_multimodal": true, "content": [...]}}`. OpenAI, Anthropic, OpenRouter, and compatible chat completion endpoints reject tool messages whose `content` is a JSON object with HTTP 400 Bad Request.
- Remediation: Unwrap `_multimodal` objects into their `content` array (or extract a text fallback when images are not supported) prior to inserting into `tool_result::build`, mirroring Python's `_tool_result_content_for_active_model`.

### Finding 3 (High): Missing Layer 2 Stdout Line Filtering and Contamination Defense
- File and Line: `rust/crates/hermes-gateway/src/extension_host.rs:283-301`
- Description: The protocol specification (`rust/analysis/extension-host-protocol-agy.md` section 4.2) requires a two-layer defense against stdout contamination. Layer 2 in Rust requires discarding empty lines, whitespace, and non-JSON lines with a warning, scanning until a valid JSON response is found.
  In `extension_host.rs:283-301`, `exchange` performs a single `read_until(b'\n')`. It immediately submits the buffer to `serde_json::from_slice`.
- Impact: If an external library prints to stdout before Layer 1 redirection runs in Python (for example, Python interpreter warnings, sitecustomize/usercustomize scripts, or third-party C extensions writing during interpreter startup), or if an unbuffered newline slips through, `read_until` reads that single line, fails JSON parsing with `"extension host returned invalid JSON"`, and triggers Finding 1, killing the host.
- Remediation: Implement a line-scanning loop in `exchange` that skips empty lines and lines not starting with `{`, logging a warning and continuing until a valid response envelope or EOF/timeout is encountered.

### Finding 4 (High): Profile Secrets Overwritten by Dotenv File Override in Child Process
- File and Line: `hermes_cli/rust_extension_host.py:92-95`, `hermes_cli/env_loader.py:504-541`
- Description: In `rust_extension_host.py:92-95`:
  ```python
  _install_profile_secrets(params.get("profile_secrets"))
  from hermes_cli.env_loader import load_hermes_dotenv
  load_hermes_dotenv(hermes_home=resolved_home)
  ```
  `_install_profile_secrets` populates `os.environ` with the task-local secrets passed over stdin from the gateway. Immediately afterwards, `load_hermes_dotenv` executes.
  In `env_loader.py:504-523`, `load_hermes_dotenv` only skips process-global dotenv loading if `is_multiplex_active()` is true. In the child process, `is_multiplex_active()` is false because `set_multiplex_active()` was never called.
  Consequently, `load_hermes_dotenv` proceeds to line 536:
  `_load_dotenv_with_fallback(user_env, override=True)`
- Impact: If `<hermes_home>/.env` exists, any key in `.env` unconditionally overwrites the scoped secret provided by the gateway. If a user rotates a credential in memory, or if the gateway injects secrets from an external vault, the disk values clobber the injected values. Additionally, `agent.secret_scope.set_secret_scope()` is never called, leaving the Python secret scope uninitialized.
- Remediation: Call `set_multiplex_active(True)` and `set_secret_scope(params.get("profile_secrets"))` in the child process, or invoke `load_hermes_dotenv` before `_install_profile_secrets` so injected secrets always take precedence over disk values.

### Finding 5 (Medium): Built-in Tool Precedence Inversion in Tool Merging
- File and Line: `rust/crates/hermes-gateway/src/main.rs:234-247`
- Description: The docstring for `merge_native_tools` states:
  "Extension definitions replace an explicitly authorized native override in place, while new names append without perturbing the existing prefix."
  The implementation executes:
  ```rust
  for extension in extensions {
      let name = extension.spec().name;
      if let Some(slot) = base.iter().position(|tool| tool.spec().name == name) {
          base[slot] = extension;
      } else if !name.is_empty() {
          base.push(extension);
      }
  }
  ```
- Impact: If an extension registers a tool with the same name as a built-in core tool (such as `current_time` or future native tools like `memory` or `clarify`), `base[slot] = extension` overwrites the native tool implementation with the extension tool. This directly contradicts the Python contract (`agent/memory_manager.py:506-523`, `toolsets._HERMES_CORE_TOOLS`), where built-in core tools always win and colliding extension tool names are dropped.
- Remediation: Core native tools must have precedence. In `merge_native_tools`, if `base.iter().any(|tool| tool.spec().name == name)`, log a warning and discard the extension tool.

### Finding 6 (Medium): Collision Routing Inversion Between Schema Resolution and Tool Call
- File and Line: `rust/crates/hermes-gateway/src/extension_host.rs:83`, `hermes_cli/rust_extension_host.py:260-269`
- Description: In Rust (`extension_host.rs:83`), `available_tools` constructs:
  `Self::convert_tools(self.plugin_tools.iter().chain(&self.memory_tools), client)`
  Because `plugin_tools` is chained first, `names.insert` preserves the plugin tool schema and discards the memory tool schema when a name collision occurs.
  In Python (`rust_extension_host.py:260-269`), `call_tool` checks:
  ```python
  if name in self._memory_tool_names:
      return self._memory_manager.handle_tool_call(...)
  if name in self._plugin_tool_names:
      return registry.dispatch(...)
  ```
- Impact: If a plugin and an external memory provider register a tool with the identical name, Rust advertises the plugin's description and parameters schema to the model, but when the model invokes the tool, Python routes the call to the memory provider. Schema advertisement and execution target are completely inverted.
- Remediation: Align collision precedence in both Rust and Python so that tool schema advertisement and dispatch ordering use the exact same precedence rule, and reject collisions with an explicit warning.

### Finding 7 (Medium): Broken Pipe and EOF in `exchange` Leaves Child Subprocess Running
- File and Line: `rust/crates/hermes-gateway/src/extension_host.rs:288-293, 313-319`
- Description: In `exchange`:
  ```rust
  match tokio::time::timeout(timeout, operation).await {
      Ok(result) => result,
      Err(_) => {
          kill_process_tree(&mut process.child).await;
          Err(Error::Other("extension host request timed out".into()))
      }
  }
  ```
  `kill_process_tree` is executed only when `timeout` expires (`Err(_)` from the timeout wrapper). If `operation` itself fails (for example, `read == 0` EOF, broken pipe on write, or JSON decode error), `timeout` resolves with `Ok(Err(...))`.
- Impact: The child process is not killed when a broken pipe or framing error occurs. The supervisor loop marks `healthy = false` and ignores future commands, but the Python process and any background threads or daemons it started remain alive until the Rust client is dropped. If the client is cached in conversation state, the defunct process lingers indefinitely.
- Remediation: Call `kill_process_tree(&mut process.child).await` on any I/O failure or unexpected EOF in `exchange`, not solely on timeout.

### Finding 8 (Medium): Windows Descendant Cleanup Missing in `Process::drop`
- File and Line: `rust/crates/hermes-gateway/src/extension_host.rs:121-132`
- Description: `impl Drop for Process` contains only Unix cleanup:
  ```rust
  #[cfg(unix)]
  if let Some(pid) = self.child.id() {
      unsafe {
          libc::kill(-(pid as i32), libc::SIGKILL);
      }
  }
  ```
  On Windows, `Drop` contains no code. Although `kill_process_tree` invokes `taskkill /T /F`, `kill_process_tree` is an asynchronous function that only runs during explicit worker shutdown or timeout.
- Impact: If a Tokio worker task is abruptly dropped or cancelled on Windows, or during gateway panic, only the top-level Python child receives `kill_on_drop`. Any background processes spawned by Python plugins (e.g. database engines or helper utilities) are orphaned on Windows.
- Remediation: On Windows, associate the spawned child with a Windows Job Object configured with `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`, ensuring the kernel automatically terminates all descendant processes when the handle is dropped.

### Finding 9 (Low): Response Deserialization Fails on Null ID in Python Error Responses
- File and Line: `hermes_cli/rust_extension_host.py:304-331`, `rust/crates/hermes-gateway/src/extension_host.rs:134-141`
- Description: In `rust_extension_host.py:304-307`:
  ```python
  request_id = None
  try:
      request = json.loads(raw_line)
      request_id = request.get("id")
      ...
  except Exception:
      response = _response(request_id, error="extension host request failed")
  ```
  If `json.loads` fails or `request` is not a dict, `request_id` remains `None`. Python writes `{"id": null, "ok": false, "error": "extension host request failed"}`.
  In `extension_host.rs:136`, `Response` is defined with `id: u64`.
- Impact: `serde_json::from_slice` cannot deserialize JSON `null` into `u64`. Instead of extracting the error message and reporting an RPC error, Rust returns `"extension host returned invalid JSON"`.
- Remediation: Define `id: Option<u64>` on `Response` in Rust, or validate and return a default ID of 0 in Python.

---

## Ranked Findings: Follow-up Improvements

### Finding 10 (Improvement): Working Directory (`cwd`) Not Changed in Extension Host
- File and Line: `hermes_cli/rust_extension_host.py:107-117`, `rust/crates/hermes-gateway/src/extension_host.rs:154`
- Description: Rust spawns the Python extension host with `current_dir(repo)` (the Hermes repository root). It passes `params["cwd"]` in the `initialize` payload, and Python records `self._session_info["cwd"] = ...`. However, `rust_extension_host.py` never calls `os.chdir(params["cwd"])`.
- Impact: Plugin tools that rely on `os.getcwd()` or relative filesystem operations (such as `file_tools.py` or `local.py`) execute against the Hermes repo root instead of the session workspace directory.
- Remediation: Call `os.chdir(params["cwd"])` in `ExtensionHost.initialize` if `params.get("cwd")` is provided and exists.

### Finding 11 (Improvement): Dead Extension Tools Advertised if `snapshot()` Fails
- File and Line: `rust/crates/hermes-gateway/src/main.rs:570-598`
- Description: In `main.rs:570-573`, `fresh_tools` is assembled from `available_extension_tools`. Subsequently, line 593 calls `client.snapshot().await`. If `snapshot()` times out, `exchange` kills the extension host child process. `main.rs:593` catches this failure via `unwrap_or_else` and falls back to an empty snapshot, but leaves the dead tools inside `fresh_tools`.
- Impact: The LLM prompt is assembled without extension sections, but the model is still offered the extension tools. When the model invokes one, it fails immediately with `"extension host is not running"`.
- Remediation: If `client.snapshot()` fails, strip the extension tools from `fresh_tools` before proceeding with prompt assembly.

### Finding 12 (Improvement): Heuristic Suffix Filtering in `_install_profile_secrets`
- File and Line: `hermes_cli/rust_extension_host.py:43-58`, `rust/crates/hermes-gateway/src/extension_host.rs:150-160`
- Description: `extension_host.rs` does not clear environment variables before spawning Python (`command.env_clear()`), causing the child to inherit the parent gateway environment. In Python, `_install_profile_secrets` attempts to purge stale secrets using a suffix list (`_API_KEY`, `_TOKEN`, `_SECRET`, `_KEY`, `_PASSWORD`, `_CREDENTIAL`, `_CREDENTIALS`).
- Impact: Non-standard credential names (such as `HONCHO_APP_ID`, `APP_ID`, or custom plugin tokens) are not cleared and can leak from a prior gateway process environment into a different profile's child.
- Remediation: Use `agent.secret_scope._is_global_env(name)` to purge all environment variables that are not explicitly recognized as process-global, or pass an explicit minimal environment from Rust via `Command::env_clear`.

### Finding 13 (Improvement): Generic Error Masking in Python Exception Handler
- File and Line: `hermes_cli/rust_extension_host.py:328-330`
- Description: When an exception occurs during request handling, Python logs the traceback to stderr but sends back a static string: `error="extension host request failed"`.
- Impact: The specific cause of failure (for example, missing required arguments, tool lookup failure, or parameter validation errors) is withheld from the Rust caller and the LLM tool result.
- Remediation: Return the exception class and sanitized message in `response["error"]`, e.g. `f"{type(e).__name__}: {e}"`.

### Finding 14 (Improvement): Parameter Schema Missing `"type": "object"` Enforcement
- File and Line: `rust/crates/hermes-gateway/src/extension_host.rs:366-371`
- Description: `Tool::from_definition` checks `function.get("parameters").is_some_and(Value::is_object)`. If a provider returns `{}` or `{"properties": {}}` without `"type": "object"`, Rust does not inject the `"type": "object"` property.
- Impact: Strict model providers (such as OpenAI strict function calling or Anthropic schema validation) return HTTP 400 when the root parameter schema lacks an explicit `"type": "object"`.
- Remediation: Ensure `parameters["type"] = "object"` whenever `parameters` is an object.

### Finding 15 (Improvement): Subprocess Spawned Unconditionally on Resumed Sessions
- File and Line: `rust/crates/hermes-gateway/src/main.rs:515-555`
- Description: In `build_conversation_client`, if `extensions_configured(&selected)` is true, `extension_host::Client::spawn` is called before checking whether the session already has a stored prompt in SQLite. If the session has no extension tools and only dynamic prompt sections, the prompt is restored from SQLite and the spawned Python child sits idle for the life of the client.
- Impact: Unnecessary Python process startup latency and memory overhead on session resumption when no extension tools exist.
- Remediation: Inspect stored session tool names or evaluate whether extension tools are present before spawning, or lazily spawn the child upon first tool execution if only cached prompt reuse is required.

---

## Verified Areas with No Defects

- Timeouts: Explicit timeouts are defined and enforced across all four operations (`INITIALIZE_TIMEOUT = 60s`, `SNAPSHOT_TIMEOUT = 30s`, `TOOL_TIMEOUT = 310s`, `SHUTDOWN_TIMEOUT = 7s`). Tokio timeouts guard every I/O exchange without blocking the async runtime.
- Gating Honesty (Issue #81014 Parity): When the `memory` toolset is disabled or omitted from `config.yaml`, `rust_extension_host.py` accurately evaluates `memory_provider_tools_enabled` to set `memory_exposed = False`. It returns `memory_tools = []` and `memory_prompt = None`. `conversation_prompt.rs:355-360` filters out empty/null external memory, ensuring that neither the external memory prompt block nor uncallable tools are exposed to the model.
- Cached Prompt Reuse: When a session is resumed with valid history and matching runtime parameters, `conversation_prompt::restore_or_build` restores the prompt directly from SQLite. The fresh build closure is bypassed, and `client.snapshot()` is not executed.
- Callback Execution Count: Plugin prompt callbacks execute exclusively inside `client.snapshot()`. Because `snapshot()` is bypassed during stored prompt reuse, callbacks execute exactly once across the session lifecycle, verified by test assertions.
- Layer 1 Stdout Redirection: In `rust_extension_host.py:295-300`, `os.dup(sys.stdout.fileno())` preserves the protocol pipe descriptor, `os.dup2(sys.stderr.fileno(), sys.stdout.fileno())` redirects OS fd 1 to fd 2, and `sys.stdout = sys.stderr` redirects Python-level prints before any third-party plugins or memory providers are loaded.
- Process Group Assignment: On Unix, `command.process_group(0)` sets the child as a new process group leader. On drop and timeout, negative PID signaling (`libc::kill(-(pid as i32), libc::SIGKILL)`) targets the entire process group.

---

## Missing Focused Tests

The following targeted integration and unit tests are currently missing and should be added to prevent regressions:

1. `test_tool_execution_error_does_not_kill_worker`:
   - Objective: Send a `call_tool` request that returns an error (e.g. unknown tool or invalid parameters), verify the caller receives the error, and immediately send a second valid tool call to confirm the worker remains healthy and processes the second call successfully.
   - Location: `rust/crates/hermes-gateway/src/extension_host.rs:tests`

2. `test_multimodal_tool_result_wire_format`:
   - Objective: Execute a plugin tool returning `{"_multimodal": true, "content": [{"type": "text", "text": "sample"}]}`, run it through `run_tool_loop_with_content`, and verify the resulting message in `messages` has a valid content array `[{"type": "text", "text": "sample"}]` rather than the raw dictionary envelope.
   - Location: `rust/crates/hermes-gateway/src/native_tools.rs:tests`

3. `test_stdout_contamination_line_filtering`:
   - Objective: Configure a mock Python subprocess that emits leading blank lines, diagnostic messages, and unformatted text before emitting the JSON response line, proving that Rust scans past the noise and extracts the valid JSON response.
   - Location: `rust/crates/hermes-gateway/src/extension_host.rs:tests`

4. `test_profile_secrets_precedence_over_dotenv`:
   - Objective: Place `FIXTURE_KEY=from_dotenv` in `<home>/.env`, pass `FIXTURE_KEY=from_gateway` in `params.profile_secrets`, and assert that code in the extension host observes `from_gateway` rather than `from_dotenv`.
   - Location: `rust/crates/hermes-gateway/src/extension_host.rs:tests`

5. `test_core_tool_collision_precedence`:
   - Objective: Register an extension tool named `current_time` and verify that `merge_native_tools` preserves the built-in native implementation rather than replacing it with the extension tool.
   - Location: `rust/crates/hermes-gateway/src/main.rs:startup_tests`

6. `test_plugin_and_memory_collision_consistency`:
   - Objective: Register a tool with the same name in both a plugin and a memory provider, and verify that the schema advertised to the model and the handler invoked by `call_tool` belong to the same component.
   - Location: `rust/crates/hermes-gateway/src/extension_host.rs:tests`

7. `test_child_process_group_termination_on_cancellation`:
   - Objective: Spawn a plugin that launches a background child process (e.g. `sleep 60`), drop the Rust `Client`, and assert that both the Python host and the background child process are terminated.
   - Location: `rust/crates/hermes-gateway/src/extension_host.rs:tests`

8. `test_session_cwd_propagation`:
   - Objective: Pass a custom `cwd` in `InitializeParams`, call a plugin tool that returns `os.getcwd()`, and assert that the returned path matches the requested session working directory.
   - Location: `rust/crates/hermes-gateway/src/extension_host.rs:tests`
