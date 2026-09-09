# Extension-host recovery review

Scope: the uncommitted recovery implementation in
`rust/crates/hermes-gateway/src/extension_host.rs` only. Read-only review, no
files edited. I did not review the native-terminal audit reports and do not
duplicate their scope (tool naming/collision, terminal registration).

## Post-review disposition

This report records the review against the pre-final diff, so its line numbers
are historical. Before the checkpoint was committed, the primary lane added a
real respawn secret-isolation assertion, disabled recovery once teardown begins,
and added the malformed non-null switch state test. Findings 1, 2, and 5 are
therefore resolved. Findings 3 and 4 remain documented follow-ups.

## Verdict

No blocking correctness or security defect found in the recovery path. The core
guarantees the task asked about hold in the code: a transport-failed request is
never auto-replayed, the frozen capability snapshot is re-checked on every
respawn, profile secrets and env isolation are re-applied on respawn, and
`session_switch` accepts only a JSON `null` success reply.

The findings below are all follow-ups, ordered by severity. The single most
important one is a test gap, not a code defect: the required "secret/env
isolation on every respawn" guarantee has zero coverage on the recovery path.

---

## Findings

### 1. (High, test gap) Profile-secret / env isolation on respawn is never tested

The task calls out "selected-profile secret and environment isolation on every
respawn" as a primary guarantee. The code does the right thing: the stored
`initialize_params` is a clone of the original initialize request params
(`run_worker`, line 649), which includes `profile_secrets` because
`skip_serializing_if` only drops it when `None` (lines 66-69). `recover_process`
re-sends those params on the private stdin pipe (lines 566-571), and
`ProcessConfig.clear_environment` is preserved across respawns (lines 226,
560-565, 458-470), so `env_clear` + `inherited_child_env_is_global` re-run on
every recovery.

But no test exercises this. The only recovery tests
(`transport_failure_recovers_without_replaying_and_keeps_rebound_session`,
line 1072, and `recovery_rejects_a_changed_capability_snapshot`, line 1133) both
pass `profile_secrets: None` (line 932). The only test that proves secret/foreign
env isolation (`real_python_host_renders_and_executes_then_shuts_down`, line
1407, asserts `profile_secret_ok` / `foreign_secret_absent` at lines 1614-1615)
never crashes the child, so it never covers a respawn.

Consequence: a regression that dropped `profile_secrets` from the stored params,
or reset `clear_environment` on the recovery config, would leak a foreign
process-global secret into the respawned child and every test would still pass.
Recommend a recovery test that sets `profile_secrets` + a foreign env var, forces
a crash, and asserts the recovered child still sees the profile secret and not
the foreign one.

### 2. (Medium, follow-up) `close()` can trigger a full respawn + re-initialize during teardown

In `run_worker`, a transport failure on any non-shutdown request drives the
inline recovery block (lines 666-683), which respawns and re-runs `initialize`.
`Client::close` sends `flush_pending` and `session_end` (lines 378-406) before
`shutdown`. If the child dies during `flush_pending` or `session_end` (a
plausible moment: the process is being torn down), the worker will respawn a
brand-new child and re-run `initialize` on it, which re-runs the Python memory
provider's `initialize(...)` (and any init-time side effects such as prefetch),
purely to immediately shut it back down.

This is not a "replay of the failed mutation" (the failed `flush`/`session_end`
is not resent), so it does not violate the no-replay guarantee. But re-running
provider init as part of teardown is surprising and can produce an unexpected
memory-side write during shutdown. Consider suppressing recovery once a
shutdown/close sequence has begun (e.g. skip the inline recovery block when the
current request method is `flush_pending`/`session_end`/`shutdown`).

Untested: there is no test for a transport failure during `close()`.

### 3. (Low, follow-up) Short turn/flush timeouts can convert a slow-but-healthy backend into a kill + respawn

`TURN_COMPLETE_TIMEOUT` is 5s (line 37) and the exchange timeout kills the
process tree on expiry (lines 785-793), which the worker then treats as a
transport failure and respawns (lines 666-683). A memory `sync_turn` that does
real network I/O can legitimately exceed 5s; the flush path is similarly bounded
(`MEMORY_FLUSH_TIMEOUT` 10s, line 38). The effect is a spurious kill + respawn
under load rather than data loss, but it is a retry-storm-adjacent behavior worth
confirming against the real memory providers. If `turn_complete` only queues
async work on the Python side this is fine; if `sync_turn` runs inline it is
tight.

### 4. (Low, follow-up) Recovery carries only `session_id` forward, not the rest of the session metadata

`update_recovery_session` (lines 587-597) rewrites only `session_id` in the
stored initialize params on a successful `session_switch`. `session_title`,
`user_id`, `chat_id`, `chat_type`, `thread_id`, `gateway_session_key` etc. are
left at their original spawn-time values. For the common rebind case (compression
rotation, `reset=false`) this is correct because those fields are stable within a
conversation. But when `session_switch` is called with `reset=true` (documented
at lines 311-316 as "a genuinely new logical conversation"), a subsequent respawn
re-initializes the new session id with stale title/user/chat metadata. Confirm
whether `reset=true` is actually reachable for this host; if so, the recovery
params should be refreshed more fully on a reset switch.

### 5. (Low, follow-up) Malformed non-null `session_switch` reply: Rust and Python can diverge

`session_switch` correctly rejects any non-null success reply (lines 338-345),
and `update_recovery_session` correctly refuses to advance the recovery session
id unless the reply is `null` (line 588), so the two stay consistent with each
other. The residual risk is with the Python peer: if the host actually performed
the switch but returned a malformed non-null body, Rust reports failure to the
caller and keeps the *old* session id for recovery. That is the safe direction
(no silent acceptance of a malformed reply), but it is a real Rust/Python state
divergence. This is a protocol-contract note for the Python lane, not a Rust
defect. No test asserts that `update_recovery_session` leaves the session id
unchanged on a non-null reply (only the client-facing rejection is tested at
line 1344).

---

## What I verified holds (no action needed)

- **No auto-replay of an ambiguous failed tool/mutation.** On transport failure
  the worker sends the error to the caller first (line 656), then respawns
  (lines 659-683); the failed request is not resent. `recover_process` only
  sends `initialize` (lines 566-571). The recovery test asserts exactly two
  distinct `call_tool` side effects with no duplicate (lines 1120-1127), and the
  fixture proves a partially executed mutation (write-then-crash, lines 988-995)
  is not replayed.

- **Frozen capability preservation on respawn.** `frozen_capabilities` is
  captured from the first `initialize` result (lines 647-650) and
  `recover_process` rejects any respawn whose snapshot differs (lines 578-583),
  killing the process tree first. The five compared keys include the full
  `plugin_tools`/`memory_tools` values, so intra-tool schema drift is also
  caught. Covered by `recovery_rejects_a_changed_capability_snapshot` (line
  1133).

- **`session_switch` success contract.** Only JSON `null` is accepted (lines
  338-345); remote failures propagate (test line 1321) and non-null bodies are
  rejected (test line 1344). Rebinding survives a respawn: the recovery test
  switches to `session-two` before the crash and asserts the recovered child
  reports `session-two` (lines 1103-1117).

- **Shutdown / channel-close.** `close` is bounded at every stage and runs later
  stages after an earlier best-effort failure (lines 372-438); the worker's
  post-loop cleanup re-sends shutdown only if it was not already delivered,
  drops stdin, waits with a bound, and kills the tree on timeout (lines
  686-698). Worker exit is signalled via the `worker_done` watch, and `close`
  bounds the wait (lines 412-432). A worker-task panic drops the watch sender, so
  `wait_for` returns `Err` rather than hanging.

- **Process-tree cleanup.** Children are spawned in their own process group
  (`process_group(0)`, line 482) with `kill_on_drop(true)` (line 480); both
  `Process::drop` (lines 174-185) and `kill_process_tree` (lines 796-818)
  SIGKILL the whole group on unix and `taskkill /T /F` on windows. Exchange
  timeouts kill the tree before returning (lines 787-792).

- **Retry storms.** After the one inline post-failure attempt, further recovery
  is gated by `RECOVERY_RETRY_DELAY` (5s) via `last_recovery_failure` (lines
  611-630); requests arriving inside the window fail fast with "extension host is
  not running" rather than spawning. Requests are serialized through a single
  bounded mpsc worker loop, so queue ordering is FIFO and there is no concurrent
  respawn.

- **Response framing / contamination.** Non-protocol stdout lines are skipped
  with per-request byte/line caps (lines 748-768, `MAX_CONTAMINATED_*`),
  oversized lines and premature EOF are transport errors (lines 738-747), and
  id mismatch is fatal (lines 770-774). Covered by
  `protocol_reader_skips_bounded_stdout_contamination` (line 1365).

## Suggested test additions (priority order)

1. Recovery preserves profile-secret + env isolation (see finding 1).
2. Transport failure during `close()` (flush/session_end) - assert behavior is
   intentional (finding 2).
3. Retry-delay gating: two rapid failures inside `RECOVERY_RETRY_DELAY` fail fast
   without a second spawn.
4. `update_recovery_session` leaves the session id unchanged on a non-null reply.
