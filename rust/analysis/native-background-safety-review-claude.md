# Native managed-background: process-safety review

Scope: the uncommitted native managed-background implementation.

Files read in full:
- `rust/crates/hermes-gateway/src/background_process.rs`
- `rust/crates/hermes-gateway/src/native_process.rs`
- `rust/crates/hermes-gateway/src/native_terminal.rs`
- wiring and shutdown in `rust/crates/hermes-gateway/src/main.rs`
- supporting: `foreground_exec.rs` (comparison), `secret_scope.rs::isolated_profile_environment`, `compression_redact.rs::redact`, `session_reset.rs`

This review is read-only. No production code, tests, oracle, goldens, PORT.md, or INDEX.md were touched.

## Bottom line

One blocking correctness finding: finished processes are retained by age since spawn, not age since exit, so a long-running background job loses its final output and exit code almost immediately after it finishes. That directly defeats the feature's main use case (long-lived background work observed later through `process_manage`).

Everything else I checked is sound: process-group ownership, owner isolation, environment construction, secret redaction at the tool boundary, output bounding, the exit/kill status race, capture-task cleanup, and schema truthfulness. The remaining items below are hardening ideas or deliberate exclusions, not blockers.

## Primary resolution after review

The primary implementation lane fixed B1 by recording a monotonic finish instant before publishing terminal status and measuring `FINISHED_TTL` from that instant. A regression test constructs a process older than the TTL whose exit is recent and proves pruning retains it.

The primary lane also addressed H2 by batching TERM, waiting once for the whole set, batching KILL for survivors, and waiting once more. Shutdown is now bounded by two shared grace windows rather than two windows per process. The finish-time fix also removes H4's practical prune race during a kill grace wait. H3 remains intentionally aligned with Python, which also silently retains only its rolling 200,000-character window. H1 was narrowed by recording direct-child exit before capture settlement and avoiding a TERM when the leader is already known to have exited.

## Blocking findings at review time

### B1. Retention window is measured from spawn, not from exit

`background_process.rs:471-472`

```rust
processes.retain(|process| {
    process.status() == Status::Running || process.started.elapsed() <= FINISHED_TTL
});
```

`FINISHED_TTL` is named and documented as a finished-process time-to-live (30 minutes), but the predicate compares `process.started.elapsed()`, which is wall time since spawn. `Process` records `started_at` and `started` (`background_process.rs:194-195`) but has no exit timestamp, so "time since finish" is not available to prune against.

Consequences:
- A background process that runs longer than 30 minutes and then exits or crashes is eligible for pruning on the very next `spawn()` or `list()` call (prune runs in both, `background_process.rs:239` and `:303`). Its retained output, exit code, and Killed/Exited status are dropped, and a follow-up `poll`, `log`, or `wait` returns `not_found` (`native_process.rs:78-181`).
- This is exactly the long-lived server/build/watch workload the background path exists for (`native_terminal.rs:217-252`, and the long-lived-command guidance at `native_terminal.rs:502-519`). The result the agent launched the job to collect can vanish before it is ever read.
- Conversely, a short job that finished seconds ago but was spawned 31 minutes ago is also pruned, while a 5-second job that started 1 minute ago is kept for the full window. Retention is inverted relative to how long ago the process actually finished.

Fix direction (not applied): record a monotonic finish instant when the supervisor publishes a terminal status (`background_process.rs:283`) and prune terminal processes on `now - finished_at > FINISHED_TTL`. Running processes should continue to be retained unconditionally, as they are today.

## Hardening ideas (non-blocking)

### H1. `signal_group` can address a reused process group during the settle window

`background_process.rs:266-284`, `:387-399`, `:424-438`, `:587-597`

The supervisor reaps the child with `child.wait().await` (`:267`) and only publishes a terminal status after settling both capture tasks, up to `CAPTURE_SETTLE` per stream (`:268-283`). Between the reap and the status publish, `process.status()` still reads `Running`. A concurrent `kill()` (`:389-392`) or `shutdown()` (`:427-431`) sees a non-terminal status and calls `signal_group(pid, ...)`, which sends `kill(-pid, ...)` (`:587-594`).

Once the group-leader child is reaped and the group is empty, that pid is free for OS reuse. If the pid is reused as a new group leader inside the up-to-600ms settle window, the signal targets an unrelated group. Probability is low on default `pid_max` (reuse needs a full pid wraparound) and higher where `pid_max` is small. The group-leader design and `kill_on_drop(true)` (`:250-252`) mostly contain this, but the window is real. Consider gating `signal_group` on a still-live status that is cleared before the reap, or capturing the exit before settling so the terminal status is published atomically with the reap.

### H2. `shutdown()` group-kill is sequential and unbounded

`background_process.rs:424-438`, `main.rs:1419`

`shutdown()` batches a first SIGTERM to every running process (`:426-431`, good), then loops calling `self.kill()` sequentially (`:432-437`). Each `kill()` can wait `KILL_GRACE` (2s), escalate to SIGKILL, and wait another `KILL_GRACE` (`:393-399`). A batch of children that ignore SIGTERM therefore costs up to ~4s each, in series, for up to `MAX_PROCESSES` (64). `main.rs:1419` awaits `background_processes.shutdown()` with no timeout, after the 45s cache drain, so a wedged child set can stall gateway shutdown for minutes. The initial batched SIGTERM makes the common case fast, but consider bounding total shutdown time or killing the survivors concurrently.

### H3. No lost-output signal when the retained window rolls

`background_process.rs:162-186`, `:313-323`

`OutputStore` drops oldest characters once its retained-character limit (default 200k) is exceeded. Unlike the foreground path, which surfaces `truncation_note` and a spill (`native_terminal.rs:314-323`), the background path gives the caller no indication that earlier output was discarded. `log.total_lines` only counts retained lines, so a caller cannot tell a short job from a truncated one. A `lost_output` or dropped-count flag on `Poll`/`Log` would let the agent reason about completeness.

### H4. `kill()` grace-wait re-resolves by id and can race prune

`background_process.rs:394-398`

`kill()` holds an `Arc<Process>` but its grace `wait()` re-resolves the id from the vector. If the process becomes terminal and is concurrently pruned (needs a `spawn`/`list` plus TTL or MAX pressure) between the two calls, the grace `wait()` returns `ResolveError::Missing`, which `?` propagates as a kill failure even though termination effectively succeeded. Low probability; waiting on the already-held `status_rx` instead of re-resolving would remove it.

## Confirmed sound (checked, no action needed)

- **Process-group ownership.** `process_group(0)` makes the child a group leader with pgid == pid (`background_process.rs:251-252`); `signal_group` sends to `-pid`, and `kill_on_drop(true)` backstops the direct child. Windows uses a job-friendly creation flag and the module degrades to direct-child reaping there, matching the stated Unix focus.
- **Exit vs kill status race.** `kill_requested` is set before signaling (`:391`, `:428`, `:528`) and read by the supervisor (`:273`) so a killed process reports `Status::Killed` with `-SIGTERM` exit code (`:56-60`), and `already_finished` is computed from the pre-kill status (`:389`, `:403`).
- **Capture-task cleanup.** If a descendant holds the pipe open after the main child exits, `settle()` aborts the capture task after `CAPTURE_SETTLE` and the supervisor SIGKILLs the lingering group (`:268-272`, `:551-562`). Supervisors for terminal processes have already completed; `Drop` aborts any still-live supervisor (`:531-535`).
- **Owner isolation.** Every accessor routes through `resolve()`, which requires `process.owner == *owner` for both exact and prefix matches (`:452-453`, `:462`); `list` and `has_active_for_session` filter by owner (`:308`, `:417-418`). Knowing another owner's id does not grant access. Ambiguous prefixes collapse to `Missing` (`:463-466`), which is safe (no wrong-process access) and surfaces as `not_found`.
- **Prefix floor.** `MIN_PREFIX_CHARS = 4` (`:21`, `:457`) matches the schema text "at least four characters" (`native_process.rs:197`).
- **Environment construction.** Background spawn clears then repopulates env (`:245-246`) from `isolated_profile_environment` (global-allowlist + profile overlay + `HERMES_HOME`, `secret_scope.rs:181-191`) plus `PYTHONUNBUFFERED` (`native_terminal.rs:218-219`). The wrapper's positional args ($1 command, $2 snapshot, $3 home) line up with the spawn arg vector (`native_terminal.rs:220-236`, `:372-383`), stdin is `/dev/null`, and the persisted snapshot is written under a 0700 parent with umask 077 (`native_terminal.rs:397`, `:569-578`).
- **Secret redaction.** All provider-facing text (command, output, previews, logs) passes through `clean()` -> `redact_output` -> `strip_ansi` + `compression_redact::redact` (`native_process.rs:266-268`, applied at `:48-49`, `:99-102`, `:131`, `:171-179`, `:221`, `:238`). Raw bytes live only in the bounded in-memory `OutputStore`; there is no background spill file to leak. Spawn-failure text is redacted too (`native_terminal.rs:249`).
- **Output bounding.** The retained-character limit caps the store at a minimum of one character; previews are char-bounded via `tail_chars`; `log` paginates with saturating arithmetic.
- **Cache eviction.** The registry is created once (`main.rs:1197`), shared by `Arc` into the per-client factory (`:1206`, `:1219-1228`, `:854-858`), and keyed by `Owner(profile_home, gateway_session_key)`. A client eviction and rebuild reconstructs the same owner and sees the same processes. Registry state is bounded by `MAX_PROCESSES`.
- **Session-reset integration.** `has_active_for_session` counts only `Running` processes within `max_age` (`:416-421`), and the probe closure fails safe: a registry error keeps the session alive rather than discarding context (`session_reset.rs:82-90`, `main.rs:1272-1274`).
- **Schema truthfulness.** `process_manage` advertises exactly the implemented actions (`native_process.rs:196`, enforced at `:60-67`), and `terminal` exposes only command/background/timeout/workdir while rejecting unimplemented `pty`/`notify` with explicit errors (`native_terminal.rs:129-159`, `:352-362`). The tool descriptions match behavior (poll returns the current tail, wait blocks a bounded window).

## Deliberate exclusions (as designed)

- PTY, background notifications, and watch patterns are rejected at the terminal boundary rather than silently accepted (`native_terminal.rs:142-159`), consistent with the not-yet-available scope.
- No stdin write / submit / close on the background path; stdin is `/dev/null` (`:247`), which removes an injection surface. Any future interactive path should re-review this.
- Non-Unix builds compile but only reap the direct child; the module documents and tests the Unix path.
- The prior design note described per-stream cursors and a `lost_output` flag; the shipped registry merges streams and returns a tail. The provider-facing schema and descriptions reflect the shipped behavior, so there is no truthfulness gap, only the missing truncation signal noted in H3.
