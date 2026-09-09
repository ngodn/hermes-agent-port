# Authoritative Python Background-Process Runtime Contract for the Native Rust Port

## 1. Executive Summary & Objective

This document defines the authoritative contract for bounded, local, non-PTY background process execution and process registry lifecycle management within the Hermes agent runtime.

It accompanies two checked-in artifacts:
- [`rust/tools/background-process-contract-oracle.py`](file:///home/eins0fx/development/hermes-agent-port/rust/tools/background-process-contract-oracle.py): a deterministic, source-executed Python driver that exercises the real runtime code against live processes in isolated temporary directories.
- [`rust/tools/background-process-contract-goldens.json`](file:///home/eins0fx/development/hermes-agent-port/rust/tools/background-process-contract-goldens.json): 71 source-executed golden test cases across 9 behavioral sections establishing the exact behavioral and structural contract that the native Rust port ([`rust/crates/hermes-gateway/src/background_process.rs`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/background_process.rs)) must replicate.

Every value in the golden corpus was produced by running real repository code. No business logic is simulated or reimplemented.

---

## 2. Authoritative Python Sources

The contract is defined by line-level audit of three core modules:

1. [`tools/process_registry.py`](file:///home/eins0fx/development/hermes-agent-port/tools/process_registry.py):
   - [`ProcessRegistry`](file:///home/eins0fx/development/hermes-agent-port/tools/process_registry.py#L471): In-memory process registry tracking active and completed background processes.
   - [`ProcessSession`](file:///home/eins0fx/development/hermes-agent-port/tools/process_registry.py#L409): Dataclass encapsulating child PID, execution state, output buffers, exit codes, and task ownership.
   - [`ProcessRegistry.spawn_local`](file:///home/eins0fx/development/hermes-agent-port/tools/process_registry.py#L1073): Spawns a background child process locally using the user's login shell with `set +m` job control.
   - [`ProcessRegistry._reader_loop`](file:///home/eins0fx/development/hermes-agent-port/tools/process_registry.py#L1422): Background reader thread streaming chunks from `stdout.buffer.read1(4096)` through an incremental UTF-8 decoder with ANSI stripping.
   - [`ProcessRegistry.poll`](file:///home/eins0fx/development/hermes-agent-port/tools/process_registry.py#L2220): Queries running/exited status, exit code, uptime, and the trailing 1,000 characters of output (`output_preview`).
   - [`ProcessRegistry.read_log`](file:///home/eins0fx/development/hermes-agent-port/tools/process_registry.py#L2263): Reads the full or paginated output log with explicit `offset` and `limit` semantics.
   - [`ProcessRegistry.wait`](file:///home/eins0fx/development/hermes-agent-port/tools/process_registry.py#L2305): Blocks until a process exits, timeout elapses, or an interrupt occurs; validates positive timeout bounds and clamps to `TERMINAL_TIMEOUT`.
   - [`ProcessRegistry.kill_process`](file:///home/eins0fx/development/hermes-agent-port/tools/process_registry.py#L2424): Terminates a running child process group via `SIGTERM`; the first response omits an exit code, the stored session later records `-15`, and redundant calls return `status: "already_exited"` with that stored code.
   - [`ProcessRegistry.write_stdin`](file:///home/eins0fx/development/hermes-agent-port/tools/process_registry.py#L2551): Sends raw bytes to child stdin without appending a newline.
   - [`ProcessRegistry.submit_stdin`](file:///home/eins0fx/development/hermes-agent-port/tools/process_registry.py#L2584): Sends text + `\n` (or `\r\n` on Windows PTY) to child stdin.
   - [`ProcessRegistry.close_stdin`](file:///home/eins0fx/development/hermes-agent-port/tools/process_registry.py#L2637): Closes the stdin pipe to signal EOF without killing the process.
   - [`ProcessRegistry.list_sessions`](file:///home/eins0fx/development/hermes-agent-port/tools/process_registry.py#L2673): Queries registered processes filtered by `task_id` and/or `session_key`, marking cross-task entries with `"session_scoped": true`.
   - [`ProcessRegistry._resolve_prefix`](file:///home/eins0fx/development/hermes-agent-port/tools/process_registry.py#L2116): Resolves unique session ID prefixes (with or without `proc_` lead, minimum 4 characters) to a unique matching session.
   - [`_handle_process`](file:///home/eins0fx/development/hermes-agent-port/tools/process_registry.py#L3536): Primary tool handler parsing tool parameters, coercing integer IDs, validating actions, formatting JSON responses, and redacting sensitive text.

2. [`tools/terminal_tool.py`](file:///home/eins0fx/development/hermes-agent-port/tools/terminal_tool.py):
   - [`terminal_tool`](file:///home/eins0fx/development/hermes-agent-port/tools/terminal_tool.py#L2867): Spawns background processes when `background=True`, constructs the return envelope with `exit_code: 0`, and injects background guidance hints.
   - [`_handle_terminal`](file:///home/eins0fx/development/hermes-agent-port/tools/terminal_tool.py#L4220): Recovers misplaced `code` parameters, rejects `notify` and `pty` on foreground execution, and routes to `terminal_tool`.
   - [`_validate_workdir`](file:///home/eins0fx/development/hermes-agent-port/tools/terminal_tool.py#L1003): Rejects directory paths containing shell metacharacters (`;`, `&`, `|`, `>`, `<`, `$`, etc.).

3. [`agent/redact.py`](file:///home/eins0fx/development/hermes-agent-port/agent/redact.py):
   - [`redact_terminal_output`](file:///home/eins0fx/development/hermes-agent-port/agent/redact.py#L1166): Redacts secrets from terminal output, switching `code_file` mode based on whether the command is an env dump (`printenv`/`env`) or general shell command.
   - [`redact_sensitive_text`](file:///home/eins0fx/development/hermes-agent-port/agent/redact.py#L1052): Core secret pattern detector redacting API keys (`sk-ant-...`, `sk-proj-...`, `ghp_...`, Bearer tokens).

---

## 3. Scope Boundaries & Explicit Exclusions

To ensure safe, robust, and incrementally verifiable Rust porting, the first native slice is strictly bounded. The following mechanisms are deliberately excluded from this initial contract corpus:

| Feature / Mechanism | Reason for Exclusion from Initial Slice | Rust Gateway Equivalent / Next Phase |
| :--- | :--- | :--- |
| **PTY Allocation (`use_pty=True`)** | Requires pseudo-terminal master/slave pairs (`ptyprocess` / `openpty`), escape sequences, and terminal dimension handling. | Dedicated `hermes-terminal-pty` module. |
| **Remote Sandboxes** | Involves Docker, Singularity, Modal, Daytona, and SSH remote exec wrappers (`spawn_via_env`). | Handled by separate container/SSH provider interfaces. |
| **Notifications & Watchers** | Involves `notify=True` completion queue polling, `watch_patterns` regex scans, strike counters, and gateway cron tasks. | Gateway async completion delivery pipeline. |
| **Systemd cgroup Isolation** | Requires Linux D-Bus communication with systemd (`systemd-run --user --scope --unit=...`). | Host-level supervisor isolation. |
| **Delegation Attribution** | Maps async subagent task ownership trees (`owner_task_id` vs `task_id`). | Gateway multi-agent session coordinator. |
| **Restart Adoption** | Re-attaching orphaned processes from `processes.json` after gateway crash. | Gateway state persistence layer. |
| **Windows Platform Specifics** | Involves ConPTY CRLF translations, `taskkill /T /F`, and ASLR Cygwin heap workarounds. | Windows platform abstraction crate. |

---

## 4. Contract Specification & Observable Behaviors

### 4.1 Local Non-PTY Spawn & Status Envelopes

When a background command is launched via `terminal_tool(command=..., background=True)`:
- Returns an immediate JSON envelope:
  ```json
  {
    "output": "Background process started",
    "session_id": "proc_xxxxxxxxxxxx",
    "pid": 12345,
    "exit_code": 0,
    "error": null,
    "hint": "background=true without notify_on_complete=true means this process runs SILENTLY - you will not be told when it exits..."
  }
  ```
- The child process runs detached via `subprocess.Popen` with `stdout=PIPE`, `stderr=STDOUT`, and `stdin=DEVNULL` (in default non-PTY mode).
- Invalid command types (non-strings such as integers, lists, or `None`) are rejected with `{"output": "", "exit_code": -1, "error": "Invalid command: expected string, got <type>", "status": "error"}`.
- Workdir containing shell metacharacters (e.g. `;`, `&`, `|`) is rejected with `{"output": "", "exit_code": -1, "error": "Blocked: workdir contains disallowed character ';'. Use a simple filesystem path without shell metacharacters.", "status": "blocked"}`.
- If `_handle_terminal` receives a `"code"` argument instead of `"command"`, it rejects with `"terminal received a 'code' parameter, but it requires a shell command in 'command'..."`.
- Foreground execution (`background=False`) with `notify=True` or `pty=True` is rejected at the dispatch boundary.

### 4.2 Unique-Prefix Lookup Algorithm

Process lookups accept full IDs or unique prefixes (`tools/process_registry.py:2116`):
1. Normalization: bare hex suffixes are automatically prefixed with `proc_` (e.g. `"4dae"` becomes `"proc_4dae"`).
2. Minimum Length Guard: the suffix length must be $\ge 4$ characters (`_MIN_PREFIX_CHARS = 4`). Shorter queries (e.g. `"abc"`, `"proc_ab"`) return `None`.
3. Uniqueness Check: scans both `_running` and `_finished` tables. If exactly one session matches `id.startswith(query)`, that session is returned.
4. Ambiguity Guard: if more than one session matches (e.g. `"proc_aaaa"` matches `"proc_aaaa11112222"` and `"proc_aaaa33334444"`), returns `None` (`{"status": "not_found", "error": "No process with ID ..."}`).
5. Disambiguation: adding characters until the prefix is unique (e.g. `"aaaa1"`) resolves cleanly.

### 4.3 Session and Task Ownership Filtering

The registry tracks two scoping keys for every process: `task_id` (subagent or task container) and `session_key` (gateway conversation boundary):
- `list_sessions()` without arguments returns all running and finished processes.
- `list_sessions(task_id=...)` returns only processes matching that `task_id`.
- `list_sessions(task_id=T, session_key=S)` returns:
  - Processes matching `s.task_id == T`.
  - Processes where `s.task_id != T` but `s.session_key == S`. These cross-task entries are flagged with `"session_scoped": true` so an agent can discover forgotten servers started by prior turns.
- `has_active_processes(task_id)`: returns `True` if any non-exited session matches `task_id`.
- `has_active_for_session(session_key)`: returns `True` if any active session matches `session_key`.
- `kill_all(task_id)`: bulk-kills all running processes owned by `task_id`, returning the count of processes killed.

### 4.4 Polling and Log Pagination

- `poll(session_id)`:
  - While running: `{"status": "running", "output_preview": "...", "pid": ..., "uptime_seconds": ..., "command": ...}`.
  - When finished: `{"status": "exited", "exit_code": 0, "completion_reason": "exited", "termination_source": "", ...}`.
  - ANSI escape sequences are stripped from `output_preview`.
  - `output_preview` contains up to the last 1,000 characters of buffered output.
- `read_log(session_id, offset=None, limit=200)`:
  - When `offset=None`: returns the tail of the log up to `limit` lines.
  - When `offset` is specified (e.g. `offset=0`): returns lines in range `[offset, offset + limit]`.
  - Returns `total_lines` (integer count of all lines produced) and `showing: "N lines"`.
  - Out-of-bounds offset returns `output: ""`, `showing: "0 lines"`, and preserves `total_lines`.

### 4.5 Bounded Wait Lifecycle

`wait(session_id, timeout=...)`:
- Rejects non-positive timeouts (`timeout <= 0`) with `{"status": "error", "error": "timeout must be positive (got ...)"}`.
- If requested timeout exceeds `TERMINAL_TIMEOUT` (default 180s), clamps the timeout and appends `timeout_note: "Requested wait of Xs was clamped to configured limit of Ys"`.
- If child exits before timeout: returns immediately with `status: "exited"`, `exit_code`, and output tail.
- If timeout window expires while child is running: returns `status: "timeout"`, `process_running: true`, and a `timeout_note` stating that the window elapsed and the process is still running (non-error guidance).

### 4.6 Exit Semantics & Termination Sources

- Natural exit with return code 0: `status: "exited"`, `exit_code: 0`, `completion_reason: "exited"`, `termination_source: ""`.
- Natural exit with non-zero code: `status: "exited"`, `exit_code: 42`, `completion_reason: "exited"`, `termination_source: ""`.
- Explicit kill via `kill_process(session_id)`:
  - Sends `SIGTERM` to the child process group.
  - Returns `status: "killed"`, `completion_reason: "killed"`, and `termination_source: "process.kill"` without an `exit_code` field.
  - Records `exit_code: -15` on the session, which is visible to later poll or redundant-kill responses.
- Redundant kill on an already exited process:
  - Does not error; returns `status: "already_exited"` along with the recorded exit code and output snapshot.
- External signal termination (e.g. `SIGKILL`):
  - When child is killed externally, reader loop detects termination and reconciles `exit_code: -9` (or `-SIG`), `completion_reason: "exited"`.

### 4.7 Stdin Lifecycle (Non-PTY vs Pipe Mode)

The registry supports two distinct stdin conditions:
1. Default non-PTY spawn (`spawn_local(use_pty=False)`):
   - `session.process.stdin` is detached (`subprocess.DEVNULL`).
   - `write_stdin`, `submit_stdin`, and `close_stdin` return `{"status": "error", "error": "Process stdin not available (non-local backend or stdin closed)"}`.
2. Pipe-backed spawn (when stdin is an active pipe):
   - `write_stdin(session_id, data)`: writes raw bytes, returns `{"status": "ok", "bytes_written": len(data)}`.
   - `submit_stdin(session_id, data)`: appends `\n` on POSIX, returns `{"status": "ok", "bytes_written": len(data) + 1}`.
   - `close_stdin(session_id)`: closes the stdin pipe, delivers EOF to child, returns `{"status": "ok", "message": "stdin closed"}`.
   - Writing to stdin after process exit returns `{"status": "already_exited", "error": "Process has already finished"}`.
   - Stdin operations on missing IDs return `{"status": "not_found", "error": "No process with ID ..."}`.

### 4.8 `_handle_process` Dispatch & Validation Envelopes

The tool handler `_handle_process(args, **kw)` enforces:
- Missing action: returns JSON tool error `"Unknown process action: . Use: list, poll, log, wait, kill, write, submit, close"`.
- Unknown action: returns JSON tool error `"Unknown process action: <act>. Use: list, poll, log, wait, kill, write, submit, close"`.
- Missing `session_id` for actions requiring it (`poll`, `log`, `wait`, `kill`, `write`, `submit`, `close`): returns `"session_id is required for <action>"`.
- Integer `session_id`: coerced to string `str(session_id)` before prefix lookup.
- Ambiguous prefix: returns `{"status": "not_found", "error": "No process with ID <query>"}`.
- Successful prefix resolution: routes to the underlying registry method and serializes response.

### 4.9 Secret Redaction at the Process Tool Surface

Every payload returned by `_handle_process` passes through `_redact_process_result`:
- Env dump commands (`printenv`, `env`): opaque token assignments (`SERVICE_TOKEN=...`) are redacted to `SERVICE_TOKEN=***`, while non-secret lines (`HOME=/home/user`) are preserved.
- Inline tokens in commands: Bearer tokens and API keys in `command` strings (e.g. `curl -H 'Authorization: Bearer sk-proj-...'`) are masked to `***`.
- Provider API keys in output: well-known provider tokens (`sk-ant-...`, `sk-proj-...`) in `output` and `output_preview` are masked.
- `list` action: both `command` and `output_preview` fields are redacted across all listed session dictionaries.

---

## 5. Golden Corpus Structure & Deterministic Normalization

The checked-in test suite [`rust/tools/background-process-contract-goldens.json`](file:///home/eins0fx/development/hermes-agent-port/rust/tools/background-process-contract-goldens.json) contains 71 test cases across 9 sections:

| Section Name | Test Cases | Key Invariants Tested |
| :--- | :---: | :--- |
| `spawn_and_status` | 7 | Return envelopes, PID/session tracking, invalid command rejection, workdir metacharacter blocking, misplaced `code` parameter recovery. |
| `poll_and_log` | 8 | Running poll, exited poll, ANSI stripping, full log reading, paged reading with `offset`/`limit`, default tail pagination, out-of-bounds offset. |
| `bounded_wait` | 5 | Fast process exit wait, slow process timeout elapsing, zero/negative timeout rejection, clamping to `TERMINAL_TIMEOUT`. |
| `exit_semantics` | 5 | Natural code 0, natural code 42, explicit SIGTERM kill (`-15`), redundant kill (`already_exited`), external SIGKILL (`-9`). |
| `prefix_lookup` | 12 | Exact full ID, 4-char prefix with `proc_`, bare 4-char suffix, too-short prefix rejection, ambiguous prefix rejection, disambiguation. |
| `ownership_filtering` | 7 | Unfiltered list, `task_id` filter, cross-task `session_scoped: true`, nonexistent task list, active process checks, task bulk kill. |
| `stdin_lifecycle` | 9 | Non-PTY DEVNULL error envelopes, pipe write raw bytes, submit with `\n`, close EOF delivery, write after exit, missing ID. |
| `handle_process_envelopes` | 14 | Unknown action, empty action, missing `session_id` across 7 actions, integer coercion, ambiguous prefix, successful prefix dispatch. |
| `secret_redaction` | 4 | Env dump token masking, inline bearer token in command, provider API key in output preview, list command/output redaction. |

### Deterministic Normalization Rules
To ensure 100% byte-for-byte reproducibility across runs and environments without relying on mocks:
1. Sequential UUIDs: `uuid.uuid4` is patched to return sequential deterministic UUIDs (`proc_000000000001`, `proc_000000000002`, ...).
2. Virtual PIDs: real operating system PIDs assigned to subprocesses are mapped sequentially to virtual integers (`9001`, `9002`, ...), preserving JSON numeric types.
3. Timestamp Normalization: `uptime_seconds` is normalized to `0`, ISO 8601 timestamps are normalized to `"2026-09-09T12:00:00"`, and `timeout_note` uptime fragments are normalized to `"Uptime: 0s."`.
4. Path Masking: temporary directory paths are masked to `"[TEMPDIR]"`.
5. Child Process Reaping: `ProcessTracker` registers every spawned process and ensures `proc.kill()` and `proc.wait()` in `finally:` blocks, preventing zombie or orphan leaks.

---

## 6. Verification Runs & Exact Commands

The oracle script was executed and verified against the repository:

### 1. Golden Generation Run
```bash
.venv/bin/python rust/tools/background-process-contract-oracle.py
```
**Output:**
```
wrote /home/eins0fx/development/hermes-agent-port/rust/tools/background-process-contract-goldens.json (71 cases across 9 sections)
```
Execution time: ~4.1 seconds.

### 2. Byte-for-Byte Verification Run
```bash
.venv/bin/python rust/tools/background-process-contract-oracle.py --check
```
**Output:**
```
OK: goldens match freshly generated corpus
```
Exit code: `0`.

---

## 7. Direct Guidance for the Rust Native Module

For [`rust/crates/hermes-gateway/src/background_process.rs`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/background_process.rs):

1. **Process Group Management**:
   On Unix, background child processes must be spawned in their own process group (`setpgid(0, 0)`) so that termination (`SIGTERM` / `SIGKILL`) reaps both the invoking shell and any spawned child commands.
2. **Standard Streams**:
   Default non-PTY spawns must attach `stdin` to `Stdio::null()` (or close it) unless an interactive pipe is explicitly requested. `stdout` and `stderr` must be combined (or merged via `dup2` / separate readers into a shared rolling buffer).
3. **Rolling Buffer**:
   Implement a rolling character or byte buffer capped at 200,000 characters (`MAX_OUTPUT_CHARS`), retaining the most recent output when overflow occurs.
4. **ANSI Stripping**:
   Strip ANSI escape sequences on `output_preview` (last 1,000 chars) and log reads to avoid polluting model context.
5. **Prefix Resolution**:
   Implement unambiguous prefix resolution with `_MIN_PREFIX_CHARS = 4`. Support both full ID (`proc_...`) and bare hex tail queries, rejecting ambiguous matches with `not_found`.
6. **Error Envelopes**:
   Match the exact JSON error structures emitted by `_handle_process` and `tool_error` for missing arguments, unknown actions, and missing session IDs.
