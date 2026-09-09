# Authoritative Python Terminal Runtime Contract for the Native Rust Port

## Executive Summary
This document provides an exhaustive, line-level audit of the Python terminal runtime contract across [`tools/terminal_tool.py`](file:///home/eins0fx/development/hermes-agent-port/tools/terminal_tool.py), [`tools/environments/base.py`](file:///home/eins0fx/development/hermes-agent-port/tools/environments/base.py), [`tools/environments/local.py`](file:///home/eins0fx/development/hermes-agent-port/tools/environments/local.py), [`tools/approval.py`](file:///home/eins0fx/development/hermes-agent-port/tools/approval.py), [`tools/process_registry.py`](file:///home/eins0fx/development/hermes-agent-port/tools/process_registry.py), and [`gateway/session_context.py`](file:///home/eins0fx/development/hermes-agent-port/gateway/session_context.py).

This audit defines the authoritative functional, security, isolation, and error contract that the native Rust port must replicate or explicitly adapt. It does not design the Rust module nor review any existing Rust code.

---

## 1. Provider-Visible Terminal Schema & Dispatch Validation Behavior

### 1.1 Tool Registration & Metadata
The primary terminal execution tool is registered with the system tool registry in [`tools/terminal_tool.py:4279-4288`](file:///home/eins0fx/development/hermes-agent-port/tools/terminal_tool.py#L4279-L4288):
- **Tool Name**: `"terminal"`
- **Toolset**: `"terminal"`
- **Emoji**: `"💻"`
- **Max Result Size**: `100_000` characters (`max_result_size_chars=100_000`)
- **Handler**: [`_handle_terminal`](file:///home/eins0fx/development/hermes-agent-port/tools/terminal_tool.py#L4220-L4277)
- **Pre-dispatch Requirements Check**: `check_terminal_requirements`

### 1.2 Schema Definition ([`TERMINAL_SCHEMA`](file:///home/eins0fx/development/hermes-agent-port/tools/terminal_tool.py#L4175-L4217))
```json
{
  "type": "object",
  "properties": {
    "command": {
      "type": "string",
      "description": "The shell command to execute"
    },
    "background": {
      "type": "boolean",
      "description": "Run in the background, returning a session_id. Pair with notify=true for anything with a defined end (tests, builds, deploys) - without it the process runs silently. Only servers/watchers/daemons that never exit should stay silent. Short commands: prefer foreground with a generous timeout.",
      "default": false
    },
    "timeout": {
      "type": "integer",
      "description": "Max seconds to wait (default: 180, foreground max: 600). Returns INSTANTLY when command finishes - set high for long tasks, you won't wait unnecessarily. Foreground timeout above 600s is rejected; use background=true for longer commands.",
      "minimum": 1
    },
    "workdir": {
      "type": "string",
      "description": "Working directory for this command (absolute path). Defaults to the session working directory."
    },
    "pty": {
      "type": "boolean",
      "description": "With background=true: run in a pseudo-terminal for interactive CLI tools (Codex, Claude Code, Python REPL). Local backend only. Default: false.",
      "default": false
    },
    "notify": {
      "description": "With background=true: notify=true fires exactly one notification when the process exits (the right choice for nearly every bounded task - builds, tests, deploys). notify=['pattern', ...] instead notifies when a line matches a pattern - ONLY for one-shot readiness signals on processes that never exit (e.g. ['Application startup complete']); rate-limited and auto-disabled if it over-fires. Omit for silent daemons.",
      "anyOf": [
        {"type": "boolean"},
        {"type": "array", "items": {"type": "string"}}
      ]
    }
  },
  "required": ["command"]
}
```

### 1.3 Dispatch Validation & Coercion Rules ([`_handle_terminal`](file:///home/eins0fx/development/hermes-agent-port/tools/terminal_tool.py#L4220-L4277))
Before passing control to the core [`terminal_tool`](file:///home/eins0fx/development/hermes-agent-port/tools/terminal_tool.py#L2867-L3345), [`_handle_terminal`](file:///home/eins0fx/development/hermes-agent-port/tools/terminal_tool.py#L4220) enforces:
1. **Misplaced Argument Recovery (`code` -> `command`)**:
   - If `"command"` is missing from arguments but `"code"` is supplied (a common LLM hallucination confusing `terminal` with `execute_code`), execution is immediately aborted with a formatted error:
     `"terminal received a 'code' parameter, but it requires a shell command in 'command'. Use execute_code(code=...) for Python; for shell, retry as terminal(command=...)."`
2. **Background Modifiers on Foreground Execution**:
   - If `background` is `False` (or omitted):
     - If `notify`, `watch_patterns`, or `notify_on_complete` is passed: rejects with error:
       `"notify only applies to background commands (foreground results return directly). Either drop notify, or run as terminal(command=..., background=true, notify=...)."`
     - If `pty` is truthy: rejects with error:
       `"pty requires background=true (a PTY session is interacted with via process(action='write'/'submit'), which needs a tracked background process). Retry as terminal(command=..., background=true, pty=true)."`
3. **`notify` Polymorphic Coercion**:
   - If `notify` is boolean `True`: sets `notify_on_complete = True`, `watch_patterns = None`.
   - If `notify` is boolean `False`: sets `notify_on_complete = False`, `watch_patterns = None`.
   - If `notify` is a `list`: sets `watch_patterns = notify`, `notify_on_complete = False`.
   - If `notify` is any other type: rejects with error:
     `"notify must be true/false (notify on exit) or a list of strings (notify on output pattern match)."`
   - Unadvertised legacy parameters `notify_on_complete` (bool) and `watch_patterns` (list) are still accepted from internal callers or legacy transcripts, but explicit `notify` takes precedence.

### 1.4 Core Parameter Validation ([`terminal_tool`](file:///home/eins0fx/development/hermes-agent-port/tools/terminal_tool.py#L2867-L3028))
Inside [`terminal_tool`](file:///home/eins0fx/development/hermes-agent-port/tools/terminal_tool.py#L2867):
1. **Command Type Validation**:
   - If `command` is not an instance of `str`: returns serialized JSON:
     `{"output": "", "exit_code": -1, "error": "Invalid command: expected string, got <typename>", "status": "error"}`.
2. **Timeout Bounds**:
   - If `timeout is not None and timeout <= 0`: rejected via `tool_error(f"timeout must be a positive number of seconds (got {timeout}).")`.
   - If `background` is `False` and `timeout is not None and timeout > FOREGROUND_MAX_TIMEOUT` (where `FOREGROUND_MAX_TIMEOUT = 600`): rejected via `tool_error(f"Foreground timeout {timeout}s exceeds the maximum of 600s. Use background=true with notify_on_complete=true for long-running commands.")`.
3. **Long-Lived Process Guidance Rejection**:
   - For foreground commands (`background=False`), [`_foreground_background_guidance(command)`](file:///home/eins0fx/development/hermes-agent-port/tools/terminal_tool.py#L823-L868) inspects the command via regex:
     - Detects background operators: trailing `&`, `nohup ...`, `disown`.
     - Detects persistent servers/watchers: `npm run dev`, `vite`, `uvicorn`, `python -m http.server`, `tail -f`, `watch ...`, `docker logs -f`.
     - Returns JSON error: `{"output": "", "exit_code": -1, "error": guidance, "status": "error"}`.
4. **Working Directory Validation ([`_validate_workdir`](file:///home/eins0fx/development/hermes-agent-port/tools/terminal_tool.py#L1003-L1034))**:
   - Rejects directory paths containing shell metacharacters: `;`, `&`, `|`, `>`, `<`, `$`, backticks, newlines, or carriage returns.
   - If invalid: returns `{"output": "", "exit_code": -1, "error": "Workdir path contains forbidden characters: ...", "status": "blocked"}`.

---

## 2. Foreground Local Command Lifecycle

### 2.1 Shell Choice & Discovery
When `env_type == "local"`, execution runs through [`LocalEnvironment`](file:///home/eins0fx/development/hermes-agent-port/tools/environments/local.py#L980-L2357):
- **POSIX Systems**:
  - [`_find_bash()`](file:///home/eins0fx/development/hermes-agent-port/tools/environments/local.py#L982) searches `PATH` for `bash`.
  - Fallbacks: inspects `SHELL` environment variable, then hardcoded locations `/bin/bash`, `/usr/bin/bash`, `/usr/local/bin/bash`, falling back to `/bin/sh`.
- **Windows Systems**:
  - Searches Git Bash installations: `C:\Program Files\Git\bin\bash.exe`, `C:\Program Files (x86)\Git\bin\bash.exe`, LocalAppData `Programs\Git\bin\bash.exe`, PortableGit paths, MinGit paths, and system `PATH`.
  - Probes candidate executable Viability: runs `<candidate> --noprofile --norc -c "exit 0"`.
  - Diagnostic traps: detects exit code `0xC0000005` (Access Violation / ASLR conflict on Cygwin/MSYS heap) and logs remedial actions.

### 2.2 Session Initialization & Environment Persistence
- **Login Snapshot Capture ([`BaseEnvironment.init_session`](file:///home/eins0fx/development/hermes-agent-port/tools/environments/base.py#L725-L800))**:
  - On the first command of a session, a base snapshot `hermes-snap-<session_id>.sh` is captured in `~/.hermes/sessions/<session_id>/` (permissions `0o700` on directory, `0o600` on snapshot file).
  - Runs login shell probe: `bash -l -c "export -p"` to record baseline environment variables and functions.
- **Command Wrapping ([`BaseEnvironment._wrap_command`](file:///home/eins0fx/development/hermes-agent-port/tools/environments/base.py#L872-L993))**:
  Every foreground command is executed by generating a compound bash wrapper script:
  ```bash
  # 1. Passthrough variable preservation (BUZZ_* and runtime profile vars)
  # 2. Source session snapshot
  source "/path/to/hermes-snap-<session_id>.sh" >/dev/null 2>&1 || true
  # 3. Restore profile passthrough variables
  # 4. Harness attribution
  export AI_AGENT="${AI_AGENT:-hermes-agent}" HERMES_AGENT="${HERMES_AGENT:-true}"
  # 5. Non-interactive pager overrides
  export GIT_PAGER="${GIT_PAGER:-cat}" PAGER="${PAGER:-cat}"
  # 6. Change directory with defensive option terminator
  builtin cd -- '/path/to/cwd' || exit 126
  # 7. Execute actual escaped command
  eval '<escaped_command>'
  __hermes_ec=$?
  umask 077
  # 8. Atomic environment re-dump
  __hermes_snap_tmp=$(mktemp "/path/to/hermes-snap-<session_id>.sh.tmp.XXXXXXXXXX") && {
    set +o | grep -v ... ; export -p ...
    mv -f "$__hermes_snap_tmp" "/path/to/hermes-snap-<session_id>.sh"
  } 2>/dev/null || rm -f "$__hermes_snap_tmp" 2>/dev/null || true
  # 9. Emit CWD stdout marker
  printf '\n__HERMES_CWD_<session_id>__%s__HERMES_CWD_<session_id>__\n' "$(pwd -P)"
  # 10. Exit with child return code
  exit $__hermes_ec
  ```

### 2.3 Working Directory (CWD) Tracking & Echo Semantics
- **Stdout Marker Parsing ([`BaseEnvironment._extract_cwd_from_output`](file:///home/eins0fx/development/hermes-agent-port/tools/environments/base.py#L1381-L1425))**:
  - The script outputs: `\n__HERMES_CWD_<session_id>__<canonical_pwd>__HERMES_CWD_<session_id>__\n`.
  - Output scanner searches backwards: `rfind("__HERMES_CWD_<session_id>__")`.
  - Extracts the path, sets `result["cwd"] = cwd_path` and `result["cwd_observed"] = True`.
  - Completely strips the marker line and preceding injected newline from model-visible output.
- **Windows Path Translation ([`LocalEnvironment._extract_cwd_from_output`](file:///home/eins0fx/development/hermes-agent-port/tools/environments/local.py#L2310-L2338))**:
  - Converts MSYS `/c/Users/...` to native `C:\Users\...` via `_msys_to_windows_path`.
  - Validates `os.path.isdir(normalized)`. If invalid/stale, rolls back to previous CWD and strips `cwd_observed`.
- **Durable Session Persistence vs Transient `workdir`**:
  - If `workdir` was specified in the tool call: `record_session_cwd(session_key, observed_cwd)` is **skipped** ([`tools/terminal_tool.py:3714`](file:///home/eins0fx/development/hermes-agent-port/tools/terminal_tool.py#L3714)).
  - If no `workdir` was passed: `record_session_cwd` persists the new directory under `session_key`.
- **JSON CWD Echo Behavior ([`tools/terminal_tool.py:3841-3847`](file:///home/eins0fx/development/hermes-agent-port/tools/terminal_tool.py#L3841-L3847))**:
  - The output JSON includes the `"cwd"` key **only when** `realpath(observed_cwd) != realpath(command_cwd)`.
  - If the directory did not change, `"cwd"` is omitted from the JSON dictionary.
  - If the command failed, was interrupted, or timed out before printing the marker, `cwd_observed` is False, and `"cwd"` is omitted.

### 2.4 Dual-Layer Timeout Bounds
1. **Inner Polling Deadline ([`BaseEnvironment._wait_for_process`](file:///home/eins0fx/development/hermes-agent-port/tools/environments/base.py#L1204-L1264))**:
   - `deadline = time.monotonic() + timeout`.
   - Loop polls `proc.poll()` every 5ms (`_poll_sleep = 0.005`).
   - If `time.monotonic() > deadline`:
     - Invokes `self._kill_process(proc)`.
     - Joins stdout/stderr drain thread with a 2.0s grace timeout (`drain_thread.join(timeout=2)`).
     - Renders captured partial output with appended suffix: `\n[Command timed out after {timeout}s]`.
     - Returns exit code `124`.
2. **Outer Hard Wall-Clock Backstop ([`agent.deadline.run_bounded_sync`](file:///home/eins0fx/development/hermes-agent-port/tools/environments/base.py#L1554-L1578))**:
   - Wraps the entire execution in a separate daemon worker thread with deadline:
     `bound_s = float(effective_timeout) + 2.0` (`_EXECUTE_WAIT_BOUND_GRACE_S = 2.0`).
   - If the inner poll loop or pipe reads hang, outer watchdog triggers `_on_timeout()`, kills the process tree, and forces a return of:
     `{"output": "[Command timed out after ...s]", "returncode": 124}`.

### 2.5 Cancellation & Process Tree Cleanup ([`LocalEnvironment._kill_process`](file:///home/eins0fx/development/hermes-agent-port/tools/environments/local.py#L2171-L2298))
- Command spawned with `start_new_session=True` (putting the shell wrapper in its own process group).
- On cancellation, interrupt, or timeout:
  1. **Descendant Snapshot**: Takes snapshot of descendant PIDs before sending signals via `psutil.Process(proc.pid).children(recursive=True)`.
  2. **Group SIGTERM**: Issues `os.killpg(pgid, signal.SIGTERM)`.
  3. **Grace Period**: Polls process group liveness for up to 1.0s (`_wait_for_group_exit(pgid, 1.0)`).
  4. **Escalation to SIGKILL**: If any process in the group remains alive, issues `os.killpg(pgid, signal.SIGKILL)` and waits up to 2.0s.
  5. **Orphan Sweeper (`_sweep_escaped_descendants`)**: Inspects snapshotted descendants. Any surviving process that detached from the process group via `setsid` is explicitly targeted with `SIGKILL`.
  6. **Windows Cleanup**: Calls `terminate_pid(proc.pid, force=True, ...)` or `proc.kill()` with a 2.0s wait.

### 2.6 Output Cap, Truncation, & Redacted Spill Files
- **In-Memory Streaming Cap ([`_BoundedOutputCollector`](file:///home/eins0fx/development/hermes-agent-port/tools/environments/base.py#L95-L180))**:
  - Collects chunks streamed from stdout and stderr.
  - Default cap: `MAX_OUTPUT_CHARS = 50_000` (`get_max_bytes()`).
  - Allocation: 40% Head (`20_000` chars) and 60% Tail (`30_000` chars).
  - Truncation notice inserted in middle:
    `\n\n... [OUTPUT TRUNCATED - <omitted_count> chars omitted out of <total> total] ...\n\n`
- **Spill File Creation**:
  - When output exceeds `max_chars`, raw output is teed to a private spill file:
    `~/.hermes/cache/terminal-output/out-<session_id>-<timestamp>-<rand>.log`
  - Created with permissions `0o600` via `open_exclusive` (refusing symlink diversion).
  - Spill ceiling: hard cap at `5_000_000` characters (`_SPILL_CAP_CHARS`).
- **Post-Execution In-Place Spill Redaction ([`tools/terminal_tool.py:3854-3883`](file:///home/eins0fx/development/hermes-agent-port/tools/terminal_tool.py#L3854-L3883))**:
  - Visible output and spill file content pass through `strip_ansi()` and `redact_terminal_output(text, command)`.
  - Spill file is rewritten in place using `write_text_exclusive(..., private=True, overwrite=True)`.
  - If redaction fails, the spill file is immediately deleted via `unlink()` to avoid persisting cleartext secrets.

### 2.7 Exact JSON Return Shapes

#### Success Shape (Exit Code 0)
```json
{
  "output": "total 8\ndrwxr-xr-x 2 user user 4096 ...",
  "exit_code": 0,
  "error": null,
  "cwd": "/home/user/newdir"
}
```
*(Note: `"cwd"` is only present if the working directory actually changed).*

#### Truncated Output Shape
```json
{
  "output": "<head_chars>\n\n... [OUTPUT TRUNCATED - 15000 chars omitted out of 65000 total] ...\n\n<tail_chars>",
  "exit_code": 0,
  "error": null,
  "output_total_chars": 65000,
  "full_output_path": "/home/user/.hermes/cache/terminal-output/out-sess-123.log",
  "truncation_note": "Output exceeded the capture window (head+tail shown). Full output (65,000 chars) saved to /home/user/.hermes/cache/terminal-output/out-sess-123.log - search it with search_files or page it with read_file instead of re-running the command."
}
```

#### Non-Zero Exit / Natural Command Failure
```json
{
  "output": "cat: nonexistent.txt: No such file or directory",
  "exit_code": 1,
  "error": null,
  "hint": "nonexistent.txt does not exist."
}
```

#### Inner Timeout Expiry (Return Code 124)
```json
{
  "output": "partial build output...\n[Command timed out after 30s]",
  "exit_code": 124,
  "error": null
}
```

#### Exception-Caught Timeout (Outer Backstop or Thread Failure)
```json
{
  "output": "",
  "exit_code": 124,
  "error": "Command timed out after 30 seconds"
}
```

#### User Interrupt (Return Code 130)
```json
{
  "output": "running tests...\n[Command interrupted]",
  "exit_code": 130,
  "error": null,
  "approval": "Command required approval and was approved by the user, then interrupted."
}
```

#### Pre-Execution Security / Validation Rejections
- **Non-string Command**:
  `{"output": "", "exit_code": -1, "error": "Invalid command: expected string, got int", "status": "error"}`
- **Forbidden Workdir Metacharacters**:
  `{"output": "", "exit_code": -1, "error": "Workdir path contains forbidden characters: [;&]", "status": "blocked"}`
- **Hardline Security Block**:
  `{"output": "", "exit_code": -1, "error": "BLOCKED: Command flagged as catastrophic (rm -rf /). Execution refused.", "status": "blocked"}`
- **Interactive Approval Pending**:
  `{"output": "", "exit_code": -1, "error": "", "status": "pending_approval", "approval_pending": true, "command": "...", "description": "...", "pattern_key": "..."}`

---

## 3. Approval & Sudo Execution Boundaries

### 3.1 Security Guard Execution Pipeline ([`tools/approval.py:4730-4865`](file:///home/eins0fx/development/hermes-agent-port/tools/approval.py#L4730-L4865))
[`check_all_command_guards`](file:///home/eins0fx/development/hermes-agent-port/tools/approval.py#L4730) evaluates commands in strict priority order:
1. **Container Bypass Check**: If `env_type` is an isolated container without host bind mounts, checks are bypassed. Local environment **always** runs guards.
2. **Hardline Block Floor ([`detect_hardline_command`](file:///home/eins0fx/development/hermes-agent-port/tools/approval.py#L754-L790))**:
   - Blocks catastrophic commands: `rm -rf /`, `mkfs`, raw writes to `/dev/sd*` or `/dev/nvme*`, fork bombs (`:(){ :|:& };:`), system shutdown/reboot, `kill -1`.
   - **Absolute floor**: Cannot be bypassed by `--yolo`, `approvals.mode=off`, permanent allowlists, or agent prompts.
3. **Sudo Stdin Guessing Guard ([`_check_sudo_stdin_guard`](file:///home/eins0fx/development/hermes-agent-port/tools/approval.py#L735-L751))**:
   - Blocks piped or echoed sudo attempts (`echo password | sudo -S ...`).
   - Unconditional: Cannot be bypassed by `--yolo` or auto-approvals.
4. **User-Defined Deny Rules ([`_match_user_deny_rule`](file:///home/eins0fx/development/hermes-agent-port/tools/approval.py#L794-L820))**:
   - Matches regexes configured in `approvals.deny`.
   - Fires **before** YOLO mode: a user deny rule overrides YOLO mode.
5. **YOLO & Allowlist Bypass**:
   - If `approval_mode == "off"`, `--yolo` is active, or command matches `approvals.allowlist`, approval is granted automatically (`{"approved": True, "message": None}`).
6. **Interactive vs Non-Interactive Decisions**:
   - In interactive CLI or gateway context: prompts user or returns `status: "pending_approval"`.
   - In non-interactive contexts (`single_query_mode` / headless / cron / delegated subagent): fails closed immediately with `approved: False` and an informative rejection message.

### 3.2 Sudo Transformation & Credential Injection ([`tools/terminal_tool.py:1037-1120`](file:///home/eins0fx/development/hermes-agent-port/tools/terminal_tool.py#L1037-L1120))
1. **Command Rewriting**:
   - Matches bare `sudo <cmd>`.
   - Rewrites to `sudo -S -p '' <cmd>` to read password silently from stdin.
2. **Credential Resolution**:
   - Resolves password from `HERMES_SUDO_PASSWORD`, session credential cache, or keychain.
   - If unknown in an interactive session: invokes password prompt callback.
   - If in delegated subagent context ([`_in_delegated_child_context()`](file:///home/eins0fx/development/hermes-agent-port/tools/terminal_tool.py#L775)): **never prompts**; fails closed immediately.
3. **Probe Verification**:
   - Runs non-destructive probe: `sudo -S -p '' -v` before running the command.
4. **Auth Failure Cache Invalidation ([`tools/terminal_tool.py:3728-3743`](file:///home/eins0fx/development/hermes-agent-port/tools/terminal_tool.py#L3728-L3743))**:
   - If output contains `sudo: 1 incorrect password attempt` or `Sorry, try again`, the session credential cache is purged immediately:
     `_invalidate_cached_sudo_on_auth_failure(command, output)`.
   - Result flags `sudo_auth_failed: True`, `sudo_cache_cleared: True`.

---

## 4. Session, Environment, & Profile Isolation Rules

### 4.1 Routing & Identity Resolution
- Context is managed via task-local `ContextVar` instances in [`gateway/session_context.py`](file:///home/eins0fx/development/hermes-agent-port/gateway/session_context.py):
  - `_SESSION_ID`: conversation/database session UUID.
  - `_SESSION_KEY`: routing key (`<platform>:<chat_id>:<thread_id>`).
  - `_SESSION_UI_SESSION_ID`: transient desktop/TUI window ID.
- Fallback chain in [`terminal_tool`](file:///home/eins0fx/development/hermes-agent-port/tools/terminal_tool.py#L3125):
  `session_key = get_current_session_key(default="") or (task_id or "")`.

### 4.2 Subprocess Environment Sanitization
When spawning local child shells, [`tools/environments/local.py`](file:///home/eins0fx/development/hermes-agent-port/tools/environments/local.py) scrubs the inherited environment:
- If [`session_context_engaged()`](file:///home/eins0fx/development/hermes-agent-port/gateway/session_context.py#L63) is True: `os.environ` session variables (`HERMES_SESSION_*`) are stripped to prevent cross-talk from concurrent async turns.
- Blocklisted host keys removed: internal Python runtime variables, temporary credential tokens, internal gateway socket paths.
- Injected baseline variables: `AI_AGENT="hermes-agent"`, `HERMES_AGENT="true"`, `PAGER="cat"`, `GIT_PAGER="cat"`.

### 4.3 Profile Isolation & Passthrough Protection
- Profile-specific variables (e.g. `BUZZ_*`) are explicitly excluded from snapshot capture scripts (`_snapshot_excluded_passthrough_names`).
- In [`_wrap_command`](file:///home/eins0fx/development/hermes-agent-port/tools/environments/base.py#L894-L925), passthrough variables are captured from the current process memory, restored after the snapshot is sourced, and unset from disk dumps. This prevents one profile's tokens from leaking into another profile sharing a container or user session.

### 4.4 Environment Inactivity Teardown
- Active `LocalEnvironment` instances are cached in `_active_environments[task_id]` protected by `_env_lock`.
- Background reaper thread runs every 60s.
- Environments with no activity for > 300 seconds are cleaned up via `env.cleanup()`.
- **Protection**: If an environment has active background tasks registered in [`process_registry`](file:///home/eins0fx/development/hermes-agent-port/tools/process_registry.py), the reaper resets `_last_activity` to prevent tearing down active sessions.

---

## 5. Background Process Partitioning for Porting

### 5.1 Clear Architectural Boundary

| Feature / Responsibility | First Native Rust Checkpoint | Retained on Python Host |
| :--- | :--- | :--- |
| **`terminal` Foreground Tool** | **Full Native Implementation** | None (delegated to native) |
| **Schema Validation & Guidance** | Rejects `notify`/`pty`/invalid args natively | None |
| **Local Shell Execution** | Native process spawn, pipe drain, timeout | None |
| **CWD & Snapshot Persistence** | Script wrapping, marker parse, snapshot dump | None |
| **Output Cap & Spill Redaction** | 40/60 head/tail split, temp spill file | None |
| **Process Tree Cleanup** | Process group `SIGTERM`/`SIGKILL`, descendant sweep | None |
| **Pre-Exec Hardline Floor** | Hardline regex & sudo stdin guard | None |
| **Background Processes (`background=true`)**| Reject with routing error or forward to host | Host [`process_registry`](file:///home/eins0fx/development/hermes-agent-port/tools/process_registry.py) |
| **`process_manage` Tool** | None | Host handles all verbs (`poll`, `log`, `wait`, `write`, `kill`) |
| **PTY Management (`pty=true`)** | None | Host `ptyprocess` / `winpty` |
| **Streaming Output Ring Buffer** | None | Host 200KB in-memory rolling buffer |
| **Durable Checkpointing** | None | Host `processes.json` session database |
| **Async Notification Triggers** | None | Host event bus / gateway watcher thread |
| **Interactive Sudo UI Prompts** | None | Host gateway/TUI prompt modals |

### 5.2 Minimal Subset for First Native Checkpoint
To establish a fully functional, production-grade native terminal tool:
1. Support `terminal(command=..., timeout=..., workdir=...)`.
2. Reject `background=true`, `pty=true`, and `notify` with an explicit diagnostic message instructing callers that background tasks are handled by the host registry.
3. Fully implement the local POSIX process group lifecycle (`setsid`, `SIGTERM` -> 1.0s -> `SIGKILL` -> 2.0s -> `psutil` equivalent descendant sweep).
4. Implement atomic snapshot creation, sourcing, and CWD marker generation/stripping.
5. Implement 40/60 head-tail truncation and in-place secret redaction on spill files.

---

## 6. Concrete Regression & Property-Test Oracles

The existing Python test suite contains high-fidelity tests that serve as behavioral oracles:

### 6.1 Existing Unit & Integration Oracles
1. **CWD Echo Semantics** ([`tests/tools/test_terminal_cwd_echo.py`](file:///home/eins0fx/development/hermes-agent-port/tests/tools/test_terminal_cwd_echo.py)):
   - Verifies `"cwd"` key is included when directory changes via `cd`.
   - Verifies `"cwd"` is omitted when directory remains unchanged.
   - Verifies `"cwd"` is omitted when `workdir` parameter is provided (transient override).
   - Verifies `"cwd"` is omitted on command failure.
2. **Foreground Timeout Ceiling** ([`tests/tools/test_terminal_foreground_timeout_cap.py`](file:///home/eins0fx/development/hermes-agent-port/tests/tools/test_terminal_foreground_timeout_cap.py)):
   - Tests that foreground `timeout > 600` is rejected with model guidance.
   - Tests that `timeout <= 0` is rejected.
3. **Output Truncation & Redacted Spill** ([`tests/tools/test_terminal_truncation_spill.py`](file:///home/eins0fx/development/hermes-agent-port/tests/tools/test_terminal_truncation_spill.py)):
   - Verifies 40% head and 60% tail split when output exceeds `max_bytes`.
   - Verifies spill file permissions (`0o600`).
   - Verifies secrets in the spill file are scrubbed in place before returning.
4. **Dual Timeout & Signal Exits** ([`tests/tools/test_terminal_bounded_execute.py`](file:///home/eins0fx/development/hermes-agent-port/tests/tools/test_terminal_bounded_execute.py) & [`tests/tools/test_terminal_signal_exit.py`](file:///home/eins0fx/development/hermes-agent-port/tests/tools/test_terminal_signal_exit.py)):
   - Verifies timeout returns exit code `124` with partial output preserved.
   - Verifies SIGINT returns exit code `130` with `[Command interrupted]` note.
5. **Argument & Sudo Guards** ([`tests/tools/test_terminal_none_command_guard.py`](file:///home/eins0fx/development/hermes-agent-port/tests/tools/test_terminal_none_command_guard.py) & [`tests/tools/test_terminal_tool.py`](file:///home/eins0fx/development/hermes-agent-port/tests/tools/test_terminal_tool.py)):
   - Verifies `command=None` or non-string yields `exit_code: -1, status: "error"`.
   - Verifies `workdir` metacharacters yield `status: "blocked"`.
   - Verifies bare `sudo` is transformed to `sudo -S -p ''`.

### 6.2 Property-Based Test Invariants
- **Truncation Invariant**: For any command output of length $L > M$ (where $M = \text{max\_bytes}$), the returned output must satisfy:
  $$\text{length}(\text{head}) = \lfloor 0.4 \times M \rfloor, \quad \text{length}(\text{tail}) = M - \lfloor 0.4 \times M \rfloor$$
  and must contain the standard truncation notice in between.
- **Process Group Hygiene**: After any command completes (normally, via timeout 124, or via interrupt 130), zero child or grandchild processes belonging to that process group or descendant snapshot remain alive.
- **State Snapshot Invariant**: Given command $A$: `export FOO=BAR` followed by command $B$: `echo $FOO`, command $B$ must output `BAR` when executed under the same `session_id`.

---

## 7. Explicit Unknowns, Footguns, & Upstream Python Inconsistencies

1. **Inconsistent Error JSON Schemas**:
   - Validation rejections return: `{"exit_code": -1, "output": "", "status": "error", "error": "..."}`.
   - Security rejections return: `{"exit_code": -1, "output": "", "status": "blocked", "error": "..."}`.
   - Disabled environments return: `{"exit_code": -1, "output": "", "status": "disabled", "error": "..."}`.
   - Dispatch-level schema rejections (`_handle_terminal`) return plain strings via `tool_error(...)` instead of JSON objects.
   - *Rust Decision*: The port must determine whether to replicate the divergent string vs JSON error responses for exact wire compatibility, or standardize on a typed structured response.
2. **Timeout Partial Output Asymmetry**:
   - When the inner polling loop detects a timeout, it returns partial output + returncode `124` + `[Command timed out after ...s]`.
   - If an exception occurs or the outer wall-clock thread backstop fires before the inner loop finalizes, it returns an empty string `output: ""` and `error: "Command timed out after ... seconds"`.
3. **CWD State Divergence on `workdir`**:
   - When `workdir` is specified, `BaseEnvironment` executes `cd <workdir>` and dumps the CWD marker.
   - However, `terminal_tool.py:3714` skips calling `record_session_cwd` when `workdir` is present.
   - If the user runs `terminal(command="pwd", workdir="/tmp")`, the tool echoes `"cwd": "/tmp"`. But on the next command `terminal(command="pwd")`, the working directory reverts back to the previous session directory!
4. **`process_manage` Name vs Prompt Instruction Duality**:
   - The tool is registered in the tool registry as `"process_manage"`.
   - However, system prompts and error messages repeatedly instruct the model to call `process(action=...)` (e.g. `process(action='poll')`).
5. **Non-Idempotent Command Retries**:
   - `terminal_tool.py:3623-3674` wraps foreground execution in a 3-attempt retry loop on general `Exception` with exponential backoff (`2 ** retry_count`).
   - If a transient network or OS error occurs after a command has executed side-effects (e.g. `curl -X POST ...`, `rm file`, `git push`), retrying re-executes the command and duplicates mutations.
6. **Spill File Pre-Redaction Race Window**:
   - In `_BoundedOutputCollector`, raw bytes are teed to the disk spill file during streaming.
   - Redaction only occurs *after* execution finishes in `terminal_tool.py`.
   - If the host system crashes, is hard-killed, or the disk is read mid-execution, unredacted credentials exist on disk in `~/.hermes/cache/terminal-output/`.
