# Python Contract for Native Gateway Interactive Terminal Approval

## Primary integration correction

The concurrency conclusion in section 1.2 was based on the ordinary-turn path
and is not true of the integrated Rust gateway. Both native ingress paths
intercept approval control replies before slash dispatch, session admission,
and transcript-lease acquisition. A running tool may therefore await the
route-scoped broker while retaining its transcript lease, and `/approve` or
`/deny` still resolves immediately without entering conversation history. The
live dispatcher integration test proves this ordering.

The original classification table also labeled `kill -9 12345` dangerous.
Source execution shows that Python intentionally classifies a single numeric
PID kill as safe; only the broader kill patterns in the exhaustive corpus are
approval-gated. The oracle and table below use that corrected result.

## 1. Executive Summary and Architectural Boundary

### 1.1 Core Decision and Scope
This analysis specifies the reference contract for native gateway interactive terminal-command approval across `approvals.mode: manual` and `approvals.mode: smart`, limited strictly to:
1. Local non-PTY foreground terminal executions (`pty=False`, `background=False`).
2. Managed non-PTY background terminal executions (`pty=False`, `background=True`).

Static deny-rule matching (`approvals.deny`) and unconditional execution bypass (`approvals.mode: off`) were previously analyzed in [`rust/analysis/native-approval-deny-contract-agy.md`](file:///home/eins0fx/development/hermes-agent-port/rust/analysis/native-approval-deny-contract-agy.md) and verified by [`rust/tools/approval-deny-contract-oracle.py`](file:///home/eins0fx/development/hermes-agent-port/rust/tools/approval-deny-contract-oracle.py). This specification governs the subsequent interactive security slice: the synchronization, route ownership, authorization, prompting, and terminal envelope semantics required when a command is flagged as dangerous.

### 1.2 Architectural Divergence: The Turn Lease vs Worker Thread Model
As identified in [`rust/analysis/native-approval-wiring-claude.md`](file:///home/eins0fx/development/hermes-agent-port/rust/analysis/native-approval-wiring-claude.md), Python and Rust currently feature a fundamental concurrency divergence:
- In Python ([`tools/approval.py:4549-4728`](file:///home/eins0fx/development/hermes-agent-port/tools/approval.py#L4549-L4728)), the agent runs on a `DaemonThreadPoolExecutor` worker thread while the `asyncio` event loop remains unblocked. When a dangerous command is flagged, the worker thread blocks synchronously on a `threading.Event.wait()`. The `asyncio` event loop continues running freely, admits the incoming `/approve` or `/deny` message over WebSocket or platform webhook, and sets the event to wake the waiting worker.
- In the Rust gateway (`rust/crates/hermes-gateway`), the tool loop executes inside the single async task that holds the exclusive per-session turn lease (`AdmittedTurnOwnership`). A synchronous or async wait inside `tool.call().await` pins the turn lease. Any incoming human `/approve` message flows into `dispatch.rs` and attempts to acquire the same session lease, timing out after `DEFAULT_LEASE_WAIT = 5s` and failing closed.

Because interactive approval cannot land safely in Rust without solving in-turn lease suspension or multi-turn confirmation routing, this specification separates what is required for a first production checkpoint from what can truthfully remain deferred.

### 1.3 Accompanying Hermetic Oracle
This contract is verified by the offline, credential-free, deterministic oracle at [`rust/tools/interactive-approval-contract-oracle.py`](file:///home/eins0fx/development/hermes-agent-port/rust/tools/interactive-approval-contract-oracle.py) and test corpus at [`rust/tools/interactive-approval-contract-goldens.json`](file:///home/eins0fx/development/hermes-agent-port/rust/tools/interactive-approval-contract-goldens.json). The oracle source-executes the real Python implementations, mocks all execution seams to guarantee zero candidate shell commands are run, and validates 76 discrete test cases across 11 contract categories.

---

## 2. Mode Coercion and Evaluation Precedence

### 2.1 Mode Ingestion and Coercion
Approval mode configuration is ingested via [`tools/approval.py:3450-3464`](file:///home/eins0fx/development/hermes-agent-port/tools/approval.py#L3450-L3464) and normalized by [`_normalize_approval_mode`](file:///home/eins0fx/development/hermes-agent-port/tools/approval.py#L3405-L3433):

| Input Value | Type | Normalized Mode | Rationale |
| :--- | :--- | :--- | :--- |
| `False` | Boolean | `"off"` | YAML 1.1 parses unquoted `off` as boolean `False`. Coerced to `"off"`. |
| `True` | Boolean | `"manual"` | YAML 1.1 parses unquoted `on` as boolean `True`. Coerced to safe `"manual"`. |
| `"off"` / `"  OFF  "` | String | `"off"` | Whitespace-stripped and case-folded. |
| `"smart"` / `"  Smart  "` | String | `"smart"` | Whitespace-stripped and case-folded. |
| `"manual"` / `"MANUAL"` | String | `"manual"` | Canonical manual approval. |
| `""` / `"   "` | String | `"manual"` | Empty strings default to safe `"manual"`. |
| `"auto"` / `"unknown"` | String | `"manual"` | Unrecognized string values log a warning and default to safe `"manual"`. |
| `None` / numbers / other | Non-string | `"manual"` | Non-string fallback defaults to `"manual"`. |

### 2.2 Security Precedence Ladder
The execution gate in [`tools/approval.py:4744-4805`](file:///home/eins0fx/development/hermes-agent-port/tools/approval.py#L4744-L4805) evaluates security boundaries in strict hierarchical order:

```
                  [ Inbound Terminal Command ]
                               |
                               v
            [ 1. detect_hardline_command ] ------------> BLOCKED (hardline)
                               |
                               v
           [ 2. _check_sudo_stdin_guard ] -------------> BLOCKED (sudo guess)
                               |
                               v
             [ 3. _match_user_deny_rule ] -------------> BLOCKED (user deny)
                               |
                               v
        [ 4. Bypass Check: YOLO / mode: off ] ---------> APPROVED (auto bypass)
                               |
                               v
      [ 5. _command_matches_permanent_allowlist ] ------> APPROVED (allowlist bypass)
                               |
                               v
        [ 6. Non-interactive Context Evaluation ] -----> BLOCKED or AUTO-APPROVED
             (cron / -q / unattended platforms)          (governed by context configs)
                               |
                               v
           [ 7. Interactive Gate: Gateway / CLI ]
             (mode: smart vs mode: manual)
```

1. **Hardline Floor ([`detect_hardline_command`](file:///home/eins0fx/development/hermes-agent-port/tools/approval.py#L754-L790))**: Unconditional static rejection for catastrophic commands (`rm -rf /`, `mkfs`, `dd` to raw disk, `shutdown`, fork bomb, `kill -1`). Evaluates before any bypass, allowlist, or interactive prompt.
2. **Sudo Stdin Guessing Guard ([`_check_sudo_stdin_guard`](file:///home/eins0fx/development/hermes-agent-port/tools/approval.py#L735-L751))**: Unconditional static rejection when piping strings to `sudo -S` without a configured `SUDO_PASSWORD`.
3. **User Deny Rules ([`_match_user_deny_rule`](file:///home/eins0fx/development/hermes-agent-port/tools/approval.py#L794-L823))**: Unconditional static rejection against patterns configured in `approvals.deny` in `config.yaml`.
4. **Bypass Checks ([`is_approval_bypass_active`](file:///home/eins0fx/development/hermes-agent-port/tools/approval.py#L3466-L3492))**:
   - `HERMES_YOLO_MODE=1` (frozen at startup)
   - `is_current_session_yolo_enabled(session_key)` (set via `/yolo`)
   - `approvals.mode: off`
   If any condition holds, the command is auto-approved without prompting or calling the auxiliary LLM.
5. **Permanent Allowlist ([`_command_matches_permanent_allowlist`](file:///home/eins0fx/development/hermes-agent-port/tools/approval.py#L3145-L3172))**: Simple commands matching entries in `command_allowlist` auto-approve. Commands carrying shell operators (`&&`, `;`, `\|`, backticks, `$()`) are disqualified from simple allowlist matching and escalate to interactive review.
6. **Non-Interactive Context Guard ([`tools/approval.py:4803-4997`](file:///home/eins0fx/development/hermes-agent-port/tools/approval.py#L4803-L4997))**: Single-query sessions (`-q`), cron jobs, and unattended webhook platforms fail closed with a structured refusal when unconfigured, avoiding hanging indefinitely on a missing human.
7. **Interactive Gate**: If the command reaches this point and is flagged as dangerous, it enters either smart approval or manual approval.

---

## 3. Dangerous versus Safe Command Classification

### 3.1 Detection Rules
[`detect_dangerous_command`](file:///home/eins0fx/development/hermes-agent-port/tools/approval.py#L2541-L2568) evaluates commands across all deobfuscated variants from `_command_detection_variants`:
- Searches `DANGEROUS_PATTERNS_COMPILED` ([`tools/approval.py:945-1280`](file:///home/eins0fx/development/hermes-agent-port/tools/approval.py#L945-L1280)).
- Scans for execution flags (`-c`, `-e`, `--eval`, `--command`) via `_execution_flag_findings` ([`tools/approval.py:1960-1995`](file:///home/eins0fx/development/hermes-agent-port/tools/approval.py#L1960-L1995)).
- Identifies spliced gateway lifecycle mutations.

### 3.2 Classification Contract Matrix

| Command Snippet | Classification | Pattern Key / Rule | Gated Under Manual? |
| :--- | :--- | :--- | :--- |
| `ls -la /tmp` | Safe | `None` | No (auto-executes, no approval note) |
| `cargo check --workspace` | Safe | `None` | No (auto-executes, no approval note) |
| `echo "hello world"` | Safe | `None` | No (auto-executes, no approval note) |
| `rm -rf /tmp/workdir` | Dangerous | `"recursive delete"` | Yes (requires approval) |
| `git push --force origin main` | Dangerous | `"git push force"` | Yes (requires approval) |
| `chmod 777 script.sh` | Dangerous | `"chmod 777"` | Yes (requires approval) |
| `kill -9 12345` | Safe | `None` | No (auto-executes) |
| `bash -c "rm foo"` | Dangerous | `"script execution via -c flag"` | Yes (requires approval) |

---

## 4. Request Registration, Queue Semantics, and Coalescing

### 4.1 Queue Data Structure: `_ApprovalEntry`
Each pending request is encapsulated by `_ApprovalEntry` ([`tools/approval.py:2820-2834`](file:///home/eins0fx/development/hermes-agent-port/tools/approval.py#L2820-L2834)):
```python
class _ApprovalEntry:
    __slots__ = ("event", "data", "result", "reason", "acknowledged")
```
- `event`: A synchronization primitive (`threading.Event`) that unblocks the waiting agent worker thread when resolved.
- `data`: A snapshot dictionary containing:
  - `command`: The credential-redacted command string.
  - `description`: The detector warning description.
  - `pattern_key`: The primary pattern key for allowlist persistence.
  - `pattern_keys`: The full list of warning keys.
  - `allow_permanent`: Boolean indicating whether permanent scope is permitted.
  - `allow_session`: Boolean indicating whether session scope is permitted.
  - `smart_denied`: Boolean set to `True` if flagged by smart guardian review.
  - `request_id`: A unique, deterministic hex string (from `uuid.uuid4().hex`).
- `result`: Outcome choice string: `"once"`, `"session"`, `"always"`, or `"deny"`.
- `reason`: Optional free-text reason attached by the user via `/deny <reason>`.
- `acknowledged`: Boolean flipped to `True` when an external client acknowledges receipt.

### 4.2 Queue Management and Resolution Semantics
Queues are stored in `_gateway_queues[session_key]` as a FIFO list ([`tools/approval.py:2836`](file:///home/eins0fx/development/hermes-agent-port/tools/approval.py#L2836)):
- **FIFO Resolution ([`resolve_gateway_approval`](file:///home/eins0fx/development/hermes-agent-port/tools/approval.py#L2865-L2905))**: When invoked without `resolve_all` or `request_id`, the oldest entry is popped (`queue.pop(0)`) and resolved.
- **Batch Resolution (`resolve_all=True`)**: Resolves every pending entry in `_gateway_queues[session_key]` simultaneously, clearing the queue.
- **Targeted Resolution (`request_id="<hex>"`)**: Resolves only the entry whose `data["request_id"]` matches the requested identifier.

### 4.3 Concurrent Request Coalescing
When parallel tool calls in the same session encounter identical commands and detector patterns ([`tools/approval.py:4569-4603`](file:///home/eins0fx/development/hermes-agent-port/tools/approval.py#L4569-L4603)):
- The follower thread detects the existing leader entry in `_gateway_queues[session_key]`.
- Instead of spamming the user with duplicate cards, the follower blocks on [`_await_coalesced_leader`](file:///home/eins0fx/development/hermes-agent-port/tools/approval.py#L4451-L4547).
- **Adoption Contract**:
  - If leader resolves `"session"` or `"always"`: Follower adopts the approval.
  - If leader resolves `"deny"` or times out: Follower adopts the refusal and relay reason.
  - If leader resolves `"once"`: Follower receives `None` and falls through to issue a fresh prompt, preserving single-use consent.

---

## 5. Prompt Text Formatting and Platform Delivery

### 5.1 Fallback Markdown Text Rendering
When an adapter lacks interactive button capabilities, [`_format_exec_approval_fallback`](file:///home/eins0fx/development/hermes-agent-port/gateway/run.py#L904-L930) formats the plain text prompt:

```python
def _format_exec_approval_fallback(
    command: str,
    description: str,
    command_prefix: str,
    *,
    allow_permanent: bool = True,
    allow_session: bool = True,
    smart_denied: bool = False,
) -> str:
```

### 5.2 Formatting Rules and Variants

1. **Heading**:
   - Standard: `⚠️ **Dangerous command requires approval:**`
   - Smart DENY override: `⚠️ **Smart DENY - owner override for one operation:**`
2. **Command Block**:
   - Code block with backticks: `\n```\n{cmd_preview}\n```\n`
   - Preview truncation: Truncated to 200 characters (`command[:200] + "..."` if `len(command) > 200`).
3. **Reason Line**:
   - `Reason: {description}`
4. **Action Choices**:
   - Standard options:
     - `Reply `{p}approve` to execute this one operation`
     - `{p}approve session to approve this pattern for the session` (omitted if `allow_session=False` or `smart_denied=True`)
     - `{p}approve always to approve permanently` (omitted if `allow_permanent=False` or `smart_denied=True`)
     - `{p}deny to cancel`
   - Typed prefix `{p}`: Configured per-platform (`/` on Telegram/Discord, `!` on Slack/Matrix).
   - Joins choices with commas and concluding `", or "`.

---

## 6. Route Ownership, Sender Authorization, and Plain-Text Routing

### 6.1 Canonical Session Key Construction
All approval queues and callbacks are partitioned strictly by `session_key` ([`gateway/session.py:1098-1180`](file:///home/eins0fx/development/hermes-agent-port/gateway/session.py#L1098-L1180)):
- **Telegram DM**: `agent:main:telegram:dm:<chat_id>`
- **Discord Threaded DM**: `agent:main:discord:dm:<chat_id>:<thread_id>`
- **Slack Group Shared Thread**: `agent:main:slack:<chat_id>:<thread_id>`
- **Telegram Group User-Isolated**: `agent:main:telegram:<chat_id>:<user_id>`

### 6.2 Sender Authorization Gate
When an incoming message or slash command is received:
- [`_is_user_authorized(event.source)`](file:///home/eins0fx/development/hermes-agent-port/gateway/authz_mixin.py#L503-L560) verifies the sender against environment allowlists, platform allow-all flags, and pairing databases.
- In [`_handle_active_session_busy_message`](file:///home/eins0fx/development/hermes-agent-port/gateway/run.py#L11279-L11289): Unauthorized senders in shared chats are dropped immediately (`return True`). Unauthenticated users cannot unblock or cancel pending approvals.

### 6.3 Plain-Text Approval Resolution Vocabulary
In active sessions waiting on a blocking approval (`has_blocking_approval(session_key) == True`), natural language replies are mapped to canonical slash commands ([`gateway/run.py:11343-11370`](file:///home/eins0fx/development/hermes-agent-port/gateway/run.py#L11343-L11370)):

| Plain-Text Tokens | Synthesized Slash Form | Resolved Choice |
| :--- | :--- | :--- |
| `"approve"`, `"yes"`, `"ok"`, `"okay"`, `"confirm"`, `"y"`, `"👍"` | `/approve` | `"once"` |
| `"deny"`, `"no"`, `"reject"`, `"cancel"`, `"n"`, `"👎"` | `/deny` | `"deny"` |
| `"always"`, `"approve always"`, `"always approve"` | `/approve always` | `"always"` |
| `"session"`, `"approve session"`, `"session approve"` | `/approve session` | `"session"` |

If no blocking approval is pending, conversational words like `"yes"` or `"confirm"` are never treated as approvals and fall through to standard agent busy handling.

---

## 7. Resolution Scopes and Execution Semantics

### 7.1 Scope Lifecycle Comparison

| Scope / Action | Unblocks Run | Added to `_session_approved` | Added to `_permanent_approved` | Relay Reason? |
| :--- | :--- | :--- | :--- | :--- |
| `/approve` (`"once"`) | Yes (`approved: True`) | No | No | No |
| `/approve session` | Yes (`approved: True`) | Yes (persists until session clear) | No | No |
| `/approve always` | Yes (`approved: True`) | Yes | Yes (saved to `command_allowlist`) | No |
| `/deny` | No (`approved: False`) | No | No | Optional |
| `/deny <reason>` | No (`approved: False`) | No | No | Yes (relayed in `message`) |
| `/cancel` | No (`approved: False`) | No | No | No |

### 7.2 Explicit Deny Reason Relay
When a user denies via `/deny <reason>` ([`gateway/slash_commands.py:6283-6340`](file:///home/eins0fx/development/hermes-agent-port/gateway/slash_commands.py#L6283-L6340)):
- `reason` is truncated to 280 characters.
- Captured in `entry.reason` and returned in `check_all_command_guards`.
- Embedded into the tool refusal message:
  `"BLOCKED: Command denied by user. Reason given by the user: \"<reason>\". The user has NOT consented to this action..."`

---

## 8. Smart Approval Mode Contract

### 8.1 Auxiliary LLM Risk Assessment
When `approvals.mode: smart` ([`tools/approval.py:3640-3758`](file:///home/eins0fx/development/hermes-agent-port/tools/approval.py#L3640-L3758)):
- Gathers all warnings and calls `agent.auxiliary_client.call_llm(task="approval", ...)`.
- Wraps command in XML `<command>` tags and strips comments to prevent prompt injection.
- Prompt instructs guardian to respond with exactly: `APPROVE`, `DENY`, or `ESCALATE`.

### 8.2 Smart Mode Outcome Ladder

| Guardian Verdict | Interactive Context Action | Persistence State | Audit Note in Envelope |
| :--- | :--- | :--- | :--- |
| `APPROVE` | Auto-approved immediately. No user prompt. | Per-command only. Does not persist pattern to session or permanent. | `"Command was flagged (<desc>) and auto-approved by smart approval."` |
| `DENY` | Escalates to human owner for one-operation override. Fallback prompt shows `Smart DENY` heading. Scopes `session` and `always` are forbidden. | If human approves: executed as `"once"` only. Even if client sends `"always"`, scope is coerced to one operation. | If approved by human: `"Command required approval (<desc>) and was approved by the user."` |
| `ESCALATE` | Escalates to human with normal scopes (`session` and `always` permitted). | Follows human choice (`once`, `session`, or `always`). | Standard user approval note. |
| Exception / Timeout | Guardian failure is logged as warning; falls through to standard `ESCALATE` prompt. | Follows human choice. | Standard user approval note. |

---

## 9. Timeout, Disconnect, and Run-End Cleanup

### 9.1 Approval Timeout (`approvals.timeout`)
- Configured via `approvals.timeout` (default 300 seconds, clamped to platform-safe max).
- Polled in 1.0 second slices while emitting inactivity heartbeats.
- When expired without response:
  - Resolves `approved: False`, `outcome: "timeout"`.
  - Structured refusal: `"BLOCKED: Command timed out without user response. Silence is not consent..."`
  - Entry dropped from `_gateway_queues`.

### 9.2 Turn Finalization and Cleanup
- **`unregister_gateway_notify(session_key)` ([`tools/approval.py:2852-2863`](file:///home/eins0fx/development/hermes-agent-port/tools/approval.py#L2852-L2863))**:
  - Called in the `finally` block of `GatewayRunner._run_agent` ([`gateway/run.py:7129`](file:///home/eins0fx/development/hermes-agent-port/gateway/run.py#L7129)).
  - Clears `_gateway_notify_cbs[session_key]`.
  - Pops all entries in `_gateway_queues[session_key]` and calls `entry.event.set()`, guaranteeing blocked agent threads wake up and do not hang forever if interrupted.
- **`clear_session(session_key)` ([`tools/approval.py:2995-3027`](file:///home/eins0fx/development/hermes-agent-port/tools/approval.py#L2995-L3027))**:
  - Cleans `_session_approved`, `_session_yolo`, and pending queues.
  - Sets `entry.result = "deny"` and unblocks waiting entries.
- **User Interrupt (`is_interrupted()`)**:
  - Detected inside the polling wait loop.
  - Sets `entry.result = "deny"` and breaks immediately, failing closed.

---

## 10. Replay, List, and Ack Protocols

For reconnecting clients (e.g. mobile or web frontends) ([`tools/approval.py:2907-2943`](file:///home/eins0fx/development/hermes-agent-port/tools/approval.py#L2907-L2943)):
1. **`list_gateway_approvals(session_key) -> list[dict]`**: Returns snapshot dictionaries of all unresolved approvals in the session. Read-only and replay-safe.
2. **`get_pending_gateway_approval(session_key) -> dict | None`**: Returns a copy of the oldest unresolved approval. Used to restore UI prompts after transport reconnection.
3. **`ack_gateway_approval(session_key, request_id) -> bool`**: Marks `entry.acknowledged = True` when delivered to a client. Does not claim or remove the approval; queue remains authoritative until resolved.

---

## 11. Returned Terminal Tool Envelopes

The JSON return envelopes produced by [`tools/terminal_tool.py`](file:///home/eins0fx/development/hermes-agent-port/tools/terminal_tool.py) adhere to exact structural schemas across execution modes:

### 11.1 Local Non-PTY Foreground (`background=False`, `pty=False`)

#### Case A: Safe Command (No Approval Required)
```json
{
  "output": "mocked_execution_output\n",
  "exit_code": 0,
  "error": null,
  "cwd": "/workspace"
}
```

#### Case B: Dangerous Command Approved by User
```json
{
  "output": "mocked_execution_output\n",
  "exit_code": 0,
  "error": null,
  "cwd": "/workspace",
  "approval": "Command required approval (recursive delete) and was approved by the user."
}
```

#### Case C: Approved Command Interrupted Mid-Execution
```json
{
  "output": "[Command interrupted]\n",
  "exit_code": 130,
  "error": null,
  "cwd": "/workspace",
  "approval": "Command required approval (recursive delete) and was approved by the user, then interrupted."
}
```

#### Case D: Dangerous Command Auto-Approved by Smart Mode
```json
{
  "output": "mocked_execution_output\n",
  "exit_code": 0,
  "error": null,
  "cwd": "/workspace",
  "approval": "Command was flagged (recursive delete) and auto-approved by smart approval."
}
```

#### Case E: Dangerous Command Blocked (Denied or Timed Out)
```json
{
  "output": "",
  "exit_code": -1,
  "error": "BLOCKED: Command denied by user. The user has NOT consented to this action. Do NOT retry this command, do NOT rephrase it, and do NOT attempt the same outcome via a different command. Stop the current workflow and wait for the user to respond before taking any further destructive or irreversible action.",
  "status": "blocked"
}
```

#### Case F: Gateway Ask Mode with No Callback Registered
```json
{
  "output": "",
  "exit_code": -1,
  "error": "",
  "status": "pending_approval",
  "approval_pending": true,
  "command": "rm -rf /tmp/workdir",
  "description": "recursive delete",
  "pattern_key": "recursive delete",
  "smart_denied": false,
  "allow_permanent": true
}
```

### 11.2 Managed Non-PTY Background (`background=True`, `pty=False`)

#### Case A: Safe Background Command
```json
{
  "output": "Background process started",
  "session_id": "mock-proc-session-1234",
  "pid": 9001,
  "exit_code": 0,
  "error": null,
  "hint": "background=true without notify_on_complete=true means this process runs SILENTLY - you will not be told when it exits..."
}
```

#### Case B: Dangerous Background Command Approved by User
```json
{
  "output": "Background process started",
  "session_id": "mock-proc-session-1234",
  "pid": 9001,
  "exit_code": 0,
  "error": null,
  "approval": "Command required approval (recursive delete) and was approved by the user.",
  "hint": "background=true without notify_on_complete=true means this process runs SILENTLY - you will not be told when it exits..."
}
```

#### Case C: Dangerous Background Command Auto-Approved by Smart Mode
```json
{
  "output": "Background process started",
  "session_id": "mock-proc-session-1234",
  "pid": 9001,
  "exit_code": 0,
  "error": null,
  "approval": "Command was flagged (recursive delete) and auto-approved by smart approval.",
  "hint": "background=true without notify_on_complete=true means this process runs SILENTLY - you will not be told when it exits..."
}
```

#### Case D: Dangerous Background Command Denied
```json
{
  "output": "",
  "exit_code": -1,
  "error": "BLOCKED: Command denied by user...",
  "status": "blocked"
}
```

---

## 12. Checkpoint Scope: Production Requirements vs Deferred Capabilities

### 12.1 Required for First Production Checkpoint
1. **Mode Ingestion**: Normalize and coerce `approvals.mode` (`manual`, `smart`, `off`).
2. **Precedence Ladder**: Maintain hardline floor, sudo stdin guard, user deny rules, YOLO bypass, and allowlist bypass before interactive prompting.
3. **Classification**: Exact detection of safe vs dangerous commands for terminal invocation.
4. **Queue & Request Registration**: Create pending requests with UUID identifiers and emit notifications to registered session callbacks.
5. **Fallback Prompt Formatting**: Exact text formatting matching `_format_exec_approval_fallback`.
6. **Route Ownership**: Isolate queues strictly by canonical session key.
7. **Sender Authorization Gate**: Drop unauthorized resolution attempts in shared channels.
8. **Resolution Scopes**: Support `/approve` (`once`), `/approve session`, `/approve always`, `/deny [reason]`, and `/cancel`.
9. **Returned Terminal Envelopes**: Conform to the exact JSON envelope structures for both foreground and background executions.
10. **Cleanup & Timeout**: Guaranteed fail-closed on timeout and thread wake-up on session unregister.

### 12.2 Truthfully Deferred Capabilities
1. **In-Turn Tool Suspension under Single-Task Turn Lease**:
   *Blocker*: Pinned session lease deadlock. As established in `native-approval-wiring-claude.md`, the Rust tool loop runs in the admitted turn task. Suspending mid-tool requires a turn suspension mechanism or multi-turn confirmation queue before in-turn interactive blocking can operate safely.
2. **Platform-Native UI Buttons (`send_exec_approval`)**:
   *Reason*: Discord, Slack, and Telegram button cards require platform-specific interactive components and callback servers. Text fallback is universally supported across all platforms.
3. **Auxiliary LLM Smart Evaluator (`_smart_approve`)**:
   *Reason*: Requires live auxiliary LLM inference provider routing, rate limiting, and timeout watchdogs. In an initial offline checkpoint, smart mode can safely escalate directly to human review.
4. **Concurrent Multi-Worker Leader Coalescing**:
   *Reason*: Only needed when running parallel subagent tool executions in the same conversation.
5. **Reconnectable WebSocket List/Ack Protocol**:
   *Reason*: Specific to stateful UI frontends recovering from disconnects; text-based gateway messaging does not require it.
6. **Disk Serialization of `command_allowlist` to `config.yaml`**:
   *Reason*: Live in-memory allowlist (`_permanent_approved`) suffices during runtime; modifying user configuration on disk can remain a host-managed responsibility.

---

## 13. Case Counts and Verification Command

### 13.1 Case Counts by Category

| Category Key | Category Description | Case Count |
| :--- | :--- | :--- |
| `mode_coercion_and_precedence` | Mode normalization, precedence floor, bypasses | 11 |
| `dangerous_vs_safe_classification` | Detection and pattern key classification | 8 |
| `queue_registration_and_fifo_batching` | Entry registration, FIFO ordering, batching, coalescing | 7 |
| `prompt_text_formatting` | Fallback prompt markdown formatting and variants | 5 |
| `stable_route_ownership_and_isolation` | Canonical session keys and cross-session isolation | 7 |
| `sender_authorization_and_plaintext` | Unauthorized drops and plaintext approval vocabulary | 8 |
| `resolution_scopes_once_always_deny_cancel` | Execution effects of once, session, always, deny, cancel | 6 |
| `smart_approval_mode` | Auxiliary LLM assessment, escalation, and owner override | 5 |
| `timeout_disconnect_and_cleanup` | Timeout fail-closed, unregister wakeups, session clearing | 4 |
| `replay_list_and_ack` | Listing, snapshot reading, and client acknowledgments | 4 |
| `returned_terminal_envelopes` | Foreground and background JSON envelopes | 11 |
| **Total** | **All Categories** | **76** |

### 13.2 Exact Verification Command
To verify the checked-in goldens byte-for-byte against the real Python implementation:

```bash
.venv/bin/python rust/tools/interactive-approval-contract-oracle.py --check
```

To run lint checks on the contract oracle script:

```bash
.venv/bin/ruff check rust/tools/interactive-approval-contract-oracle.py
```
