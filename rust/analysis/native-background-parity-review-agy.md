# Native Managed-Background Implementation Parity Review

## 1. Executive Summary

No blocking correctness finding exists.

The uncommitted native background runtime implemented across [`rust/crates/hermes-gateway/src/background_process.rs`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/background_process.rs), [`rust/crates/hermes-gateway/src/native_process.rs`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/native_process.rs), [`rust/crates/hermes-gateway/src/native_terminal.rs`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/native_terminal.rs), and [`rust/crates/hermes-gateway/src/main.rs`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/main.rs) accurately reproduces the core observable semantics defined by the Python authoritative sources ([`tools/process_registry.py`](file:///home/eins0fx/development/hermes-agent-port/tools/process_registry.py), [`tools/terminal_tool.py`](file:///home/eins0fx/development/hermes-agent-port/tools/terminal_tool.py), and [`agent/redact.py`](file:///home/eins0fx/development/hermes-agent-port/agent/redact.py)), as recorded in [`rust/tools/background-process-contract-goldens.json`](file:///home/eins0fx/development/hermes-agent-port/rust/tools/background-process-contract-goldens.json) and [`rust/analysis/native-background-python-contract.md`](file:///home/eins0fx/development/hermes-agent-port/rust/analysis/native-background-python-contract.md).

The gateway suite passed at review time. Process group creation, rolling capture bounding, unambiguous 4-character prefix lookup, ANSI stripping, tail pagination, bounded wait with clamping notes, redundant kill handling, session age-filtered reset liveness, and graceful gateway shutdown function in accordance with the contract.

Differences between the Rust implementation and the Python reference fall strictly into two categories: deliberate architectural scope exclusions (such as PTY allocation, notification delivery, systemd cgroup wrapping, and subagent task attribution) and minor non-blocking behavioral adaptations (such as localized secret redaction markers, tool-specific hint phrasing, and scoped lookup isolation).

---

## 2. Behavioral Parity Analysis by Functional Area

### 2.1 Observable Envelope

1. **Background Spawn Return Envelope**:
   - Location: [`rust/crates/hermes-gateway/src/native_terminal.rs:239-245`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/native_terminal.rs#L239-L245) vs. Python [`tools/terminal_tool.py:3392-3431`](file:///home/eins0fx/development/hermes-agent-port/tools/terminal_tool.py#L3392-L3431).
   - Both return an immediate JSON object with keys `output`, `session_id`, `pid`, `exit_code: 0`, `error: null`, and `hint`.
   - In Python, the `hint` field provides a multi-sentence explanation referencing `notify_on_complete=true` and `process(action='poll')`.
   - In Rust, `hint` states: `"This process runs silently. Use process_manage(action='poll' or 'wait') to observe completion."`.
   - Classification: Non-blocking deliberate adaptation. Because notifications are not yet implemented in the native slice, directing models to `process_manage` avoids prompting them to use an unsupported notification parameter.

2. **Terminal Input Validation Error Envelopes**:
   - Location: [`rust/crates/hermes-gateway/src/native_terminal.rs:105-133, 460-468, 547-567`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/native_terminal.rs#L105-L133).
   - Non-string command rejection: returns `{"output": "", "exit_code": -1, "error": "Invalid command: expected string, got <type>", "status": "error"}` matching Python ([`tools/terminal_tool.py:2917-2922`](file:///home/eins0fx/development/hermes-agent-port/tools/terminal_tool.py#L2917-L2922)).
   - Shell metacharacters in workdir: returns `{"output": "", "exit_code": -1, "error": "Blocked: workdir contains disallowed character ';'. Use a simple filesystem path without shell metacharacters.", "status": "blocked"}` matching Python ([`tools/terminal_tool.py:1003-1008`](file:///home/eins0fx/development/hermes-agent-port/tools/terminal_tool.py#L1003-L1008)).
   - Misplaced `code` parameter recovery: returns exact error message matching Python ([`tools/terminal_tool.py:4220-4226`](file:///home/eins0fx/development/hermes-agent-port/tools/terminal_tool.py#L4220-L4226)).
   - Foreground `notify` or `pty`: returns exact rejection errors matching Python ([`tools/terminal_tool.py:4228-4240`](file:///home/eins0fx/development/hermes-agent-port/tools/terminal_tool.py#L4228-L4240)).

3. **Tool Naming and Action Validation**:
   - Location: [`rust/crates/hermes-gateway/src/native_process.rs:60-67, 190-204`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/native_process.rs#L60-L67).
   - Tool name is `process_manage` in both systems ([`tools/process_registry.py:3584`](file:///home/eins0fx/development/hermes-agent-port/tools/process_registry.py#L3584)).
   - When an invalid action is provided, Python reports: `"Unknown process action: <act>. Use: list, poll, log, wait, kill, write, submit, close"` ([`tools/process_registry.py:3581`](file:///home/eins0fx/development/hermes-agent-port/tools/process_registry.py#L3581)).
   - Rust reports: `"Unknown process action: <act>. Use: list, poll, log, wait, kill"`.
   - Classification: Non-blocking deliberate adaptation. The Rust tool schema intentionally restricts its enum to the 5 implemented actions.

4. **Secret Redaction Token Representation**:
   - Location: [`rust/crates/hermes-gateway/src/compression_redact.rs:16, 36`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/compression_redact.rs#L16) vs. Python [`agent/redact.py:1052-1175`](file:///home/eins0fx/development/hermes-agent-port/agent/redact.py#L1052-L1175).
   - In Python, `_redact_process_result` produces `***` for generic env tokens or truncated key patterns like `sk-ant...ZZZZ`.
   - In Rust, `native_terminal::redact_output` runs `compression_redact::redact`, replacing matched keys, Bearer headers, and assignment tokens with `[REDACTED]`.
   - Classification: Non-blocking representation difference. Both redact secrets effectively.

---

### 2.2 Lookup and Prefix Resolution

1. **Algorithm Parity**:
   - Location: [`rust/crates/hermes-gateway/src/background_process.rs:440-467`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/background_process.rs#L440-L467) vs. Python [`tools/process_registry.py:2116-2144`](file:///home/eins0fx/development/hermes-agent-port/tools/process_registry.py#L2116-L2144).
   - Exact ID match is evaluated before prefix scans.
   - Suffix normalization automatically prepends `proc_` if omitted.
   - Minimum prefix length guard (`MIN_PREFIX_CHARS = 4`) rejects 3-character prefixes (`proc_bbb` and `bbb`).
   - Ambiguous prefixes matching more than one session return `ResolveError::Missing`, which `ProcessTool` maps to `{"status": "not_found", "error": "No process with ID <id>"}`.
   - Disambiguated prefixes resolve cleanly.

2. **Ownership Scoping**:
   - In Python, `process_registry.get(session_id)` is a global un-scoped lookup across all sessions in the registry.
   - In Rust, `Registry::resolve(&self, owner: &Owner, id: &str)` requires `process.owner == *owner`.
   - Classification: Deliberate architectural enhancement. A tool instance bound to one conversation cannot poll, log, wait on, or kill a background process belonging to another session key or profile directory.

---

### 2.3 Output-Tail Semantics

1. **Character Bound Parity Across Operations**:
   - `poll`: returns the trailing 1,000 characters in both Rust ([`background_process.rs:321`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/background_process.rs#L321)) and Python ([`tools/process_registry.py:2233`](file:///home/eins0fx/development/hermes-agent-port/tools/process_registry.py#L2233)).
   - `wait` (finished): returns the trailing 2,000 characters in both Rust ([`background_process.rs:377`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/background_process.rs#L377)) and Python ([`tools/process_registry.py:2370`](file:///home/eins0fx/development/hermes-agent-port/tools/process_registry.py#L2370)).
   - `wait` (timed out): returns the trailing 1,000 characters in both Rust ([`background_process.rs:381`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/background_process.rs#L381)) and Python ([`tools/process_registry.py:2395`](file:///home/eins0fx/development/hermes-agent-port/tools/process_registry.py#L2395)).
   - `kill` (running and already exited): returns the trailing 2,000 characters in both Rust ([`background_process.rs:407`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/background_process.rs#L407)) and Python ([`tools/process_registry.py:2461, 2532`](file:///home/eins0fx/development/hermes-agent-port/tools/process_registry.py#L2461)).
   - `list`: returns the trailing 200 characters in both Rust ([`background_process.rs:220`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/background_process.rs#L220)) and Python ([`tools/process_registry.py:2706`](file:///home/eins0fx/development/hermes-agent-port/tools/process_registry.py#L2706)).

2. **Ring Buffer Bounds**:
   - Location: [`rust/crates/hermes-gateway/src/background_process.rs:22, 162-186`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/background_process.rs#L162-L186).
   - Maximum output capacity is bounded at 200,000 characters (`DEFAULT_RETAINED_CHARS = 200_000`), matching Python's `MAX_OUTPUT_CHARS = 200_000` ([`tools/process_registry.py:488`](file:///home/eins0fx/development/hermes-agent-port/tools/process_registry.py#L488)). Incremental UTF-8 decoding preserves characters split across pipe reads.

3. **ANSI Stripping**:
   - Both run ANSI stripping on output previews and log reads before presenting text to callers ([`native_terminal.rs:580`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/native_terminal.rs#L580) vs. Python [`tools/process_registry.py:2222, 2265, 2317`](file:///home/eins0fx/development/hermes-agent-port/tools/process_registry.py#L2222)).

---

### 2.4 Pagination

1. **Log Slicing Logic**:
   - Location: [`rust/crates/hermes-gateway/src/background_process.rs:325-352`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/background_process.rs#L325-L352) and [`rust/crates/hermes-gateway/src/native_process.rs:87-106`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/native_process.rs#L87-L106).
   - Default tail read (`offset = None`): returns up to `limit` lines from the end of the log using `(total_lines.saturating_sub(limit), total_lines)`, matching Python [`tools/process_registry.py:2282-2284`](file:///home/eins0fx/development/hermes-agent-port/tools/process_registry.py#L2282-L2284).
   - Windowed read (`offset = Some(n)`): returns lines `[start, end]` where `start = offset.min(total_lines)` and `end = start.saturating_add(limit).min(total_lines)`.
   - Out-of-bounds offset (`offset > total_lines`): returns empty string `""`, `showing: "0 lines"`, and preserves `total_lines: 5`, exactly matching Python.

2. **Argument Validation**:
   - Rust validates `offset >= 0` and `limit >= 1` in [`native_process.rs:281-310`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/native_process.rs#L281-L310), returning structured error envelopes if invalid.
   - Classification: Parity maintained; defensive validation conforms to schema declarations.

---

### 2.5 Bounded Wait

1. **Timeout Validation and Clamping**:
   - Location: [`rust/crates/hermes-gateway/src/native_process.rs:108-159, 281-294`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/native_process.rs#L108-L159).
   - Zero or negative timeout: returns `{"status": "error", "error": "timeout must be positive (got ...)"}` matching Python [`tools/process_registry.py:2333-2337`](file:///home/eins0fx/development/hermes-agent-port/tools/process_registry.py#L2333-L2337).
   - Clamping: when requested timeout exceeds `max_wait_seconds`, timeout is clamped and `timeout_note` is generated: `"Requested wait of Xs was clamped to configured limit of Ys"`.
   - Window expiration on slow process: returns `status: "timeout"`, `process_running: true`, and non-error guidance in `timeout_note`.
   - Clean exit before timeout: returns immediately with `status: "exited"` and exit metadata.

2. **Async Implementation**:
   - Python executes `session._completion_event.wait(timeout=...)` on a synchronous worker thread.
   - Rust executes `tokio::time::timeout(timeout, status.changed())` via Tokio channels without blocking thread pool workers.
   - Classification: High quality async implementation maintaining exact external behavior.

---

### 2.6 Kill Semantics

1. **Signal Escalation and Statuses**:
   - Location: [`rust/crates/hermes-gateway/src/background_process.rs:387-408, 574-597`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/background_process.rs#L387-L408).
   - Running process kill: issues `libc::kill(-pid, libc::SIGTERM)`. If the process does not terminate within `KILL_GRACE` (2 seconds), it escalates to `libc::kill(-pid, libc::SIGKILL)`.
   - Status transitions to `Status::Killed` with exit code `-15` (`-libc::SIGTERM`), `completion_reason: "killed"`, and `termination_source: "process.kill"`.
   - Redundant kill on already-exited process returns `status: "already_exited"` without re-signaling, preserving original exit code and reason.
   - External `SIGKILL` termination without explicit tool kill is reconciled as `exit_code: -9`, `completion_reason: "exited"`, `termination_source: ""`.

2. **Process Group Isolation**:
   - Children are spawned with `command.process_group(0)` on Unix ([`background_process.rs:252`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/background_process.rs#L252)).
   - Negative PID signaling targets the child process group rather than only the top-level bash wrapper.
   - In [`background_process.rs:268-272, 551-562`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/background_process.rs#L268-L272), capture settle timeout (`CAPTURE_SETTLE = 300ms`) terminates surviving process group members if an orphaned descendant holds standard streams open, resolving the hang condition documented in Python issue #17327.

---

### 2.7 Liveness and Session Reset Integration

1. **Active Process Probe**:
   - Location: [`rust/crates/hermes-gateway/src/background_process.rs:410-422`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/background_process.rs#L410-L422) vs. Python [`tools/process_registry.py:2744-2774`](file:///home/eins0fx/development/hermes-agent-port/tools/process_registry.py#L2744-L2774).
   - `has_active_for_session` checks for running processes belonging to `(profile_home, session_key)`.
   - If `max_age` is configured, processes older than the threshold are ignored so long-lived preview servers do not permanently block session resets.

2. **Gateway Lifecycle Wiring**:
   - Location: [`rust/crates/hermes-gateway/src/main.rs:1262-1275, 1419`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/main.rs#L1262-L1275).
   - The probe closure is passed to `session_store::SessionStore::open`, replacing the former `|_| Ok(false)` placeholder.
   - `background_processes.shutdown().await` is invoked during server shutdown, terminating all active background process groups.

---

### 2.8 Ownership and Lifecycle Retention

1. **Owner Abstraction**:
   - Location: [`rust/crates/hermes-gateway/src/background_process.rs:28-41`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/background_process.rs#L28-L41).
   - Uses `Owner { profile_home: PathBuf, session_key: String }`.
   - Python separates `task_id` (subagent container) from `session_key` (gateway conversation).
   - In Python, cross-task processes sharing a `session_key` receive `"session_scoped": true` in `list_sessions`.
   - In Rust, subagent task tracking is excluded under the `delegation_attribution` scope boundary; all processes in the session are listed together without the tag.

2. **Pruning Limits**:
   - Location: [`rust/crates/hermes-gateway/src/background_process.rs:23-24, 469-486`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/background_process.rs#L23-L24).
   - Constants match Python exactly: `MAX_PROCESSES = 64` and `FINISHED_TTL = 30 minutes` ([`tools/process_registry.py:489, 490, 2860-2877`](file:///home/eins0fx/development/hermes-agent-port/tools/process_registry.py#L489)).

---

### 2.9 Unsupported Capabilities and Scope Boundaries

The following capabilities are excluded from the current native slice. All are handled safely at boundary interfaces:

| Feature / Area | Python Mechanism | Rust Native Handling | Classification |
| :--- | :--- | :--- | :--- |
| **Interactive Stdin (`write`, `submit`, `close`)** | Methods on `ProcessRegistry`; in default non-PTY local spawn returns `"Process stdin not available"` | Omitted from `process_manage` schema; returns `"Unknown process action: <act>"` | Deliberate Scope Exclusion |
| **PTY Allocation (`pty=true`)** | `ptyprocess.PtyProcess.spawn` | Explicit rejection: `"Native background PTY sessions are not available yet. Omit pty for a non-interactive background process."` ([`native_terminal.rs:142-146`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/native_terminal.rs#L142-L146)) | Deliberate Scope Exclusion |
| **Notifications and Watchers** | `notify=true`, `notify_on_complete=true`, `watch_patterns` queue injection | Explicit rejection: `"Native background notifications are not available yet. Omit notify and use process_manage(action='poll' or 'wait')."` ([`native_terminal.rs:147-159`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/native_terminal.rs#L147-L159)) | Deliberate Scope Exclusion |
| **Remote Sandboxes** | Docker, Singularity, Modal, Daytona, SSH wrappers | Local host execution only | Deliberate Scope Exclusion |
| **Systemd cgroup Isolation** | `systemd-run --user --scope` transient unit tracking | Unix process group isolation (`setpgid(0, 0)`) | Deliberate Scope Exclusion |
| **Delegation Attribution** | Hierarchical subagent task tracking (`owner_task_id` vs `task_id`) | Conversation session key and profile home ownership | Deliberate Scope Exclusion |
| **Restart Adoption** | Checkpoint state recovery from `processes.json` | In-memory registry; processes reaped on shutdown | Deliberate Scope Exclusion |
| **Windows Process Trees** | `taskkill /T /F` and ConPTY CRLF cooking | Unix process group signaling (`libc::kill(-pid, signal)`) | Deliberate Scope Exclusion |

---

## 3. Summary of Findings

1. **Blocking Correctness Findings**: None.
2. **Deliberate Scope Boundaries**:
   - Interactive stdin operations (`write`, `submit`, `close`) are excluded because non-interactive background processes attach stdin to `/dev/null`.
   - Background PTY allocation and completion notification parameters are cleanly rejected at tool validation boundaries with descriptive guidance.
   - Remote sandboxes, systemd unit wrapping, restart checkpointing, and subagent task attribution are deferred to later milestones.
3. **Non-Blocking Behavioral Nuances**:
   - Hint text on spawn directs callers to `process_manage(action='poll' or 'wait')`.
   - Secret redaction displays `[REDACTED]` rather than `***`.
   - Lookups are strictly isolated by session owner key rather than globally accessible.
