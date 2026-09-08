# Native manual compression fix re-review (Claude)

Scope: the uncommitted `/compress` checkpoint on `rust-rewrite`, re-read from the
current source after the fixes made in response to
`native-compression-review-agy.md` and `native-compression-review-claude.md`.
No line numbers were trusted from the earlier reviews; every reference below is
from the current tree. No source was edited.

## What the fixes now do (verified against the code)

- **Tool-call fidelity - fixed.** `load_compression_snapshot`
  (`session_db.rs:2470`) now selects `tool_call_id, tool_calls, tool_name` and
  fills `CompressionHistoryMessage` (`session_db.rs:80`). `compression_prompt::build`
  (`compression_prompt.rs:11`) emits `role`, decoded `content`, and any
  `tool_calls`/`tool_call_id`/`tool_name` per row, so assistant tool turns and
  tool results reach the summarizer. Covered by
  `prompt_keeps_tool_identity_and_arguments`.
- **Strict redaction - fixed.** Input is scrubbed twice (`redact_value` on each
  row's content plus `redact` on the whole serialized block and the focus line,
  `compression_prompt.rs:17-52`). The generated summary is redacted before it is
  wrapped or stored (`session_commands.rs:235`), and the provider error is
  redacted before logging (`session_commands.rs:226`). `compression_redact.rs`
  carries the key-prefix, `Bearer`/`Basic`, URL-credential, JSON, and query
  patterns and leaves counts/SHAs intact.
- **Title transfer - fixed.** `publish_gateway_compression` reads the parent
  `title`/`title_source`, clears them on the parent, and sets them on the child,
  all inside the one `IMMEDIATE` transaction (`session_db.rs:1489-1521`). Covered
  by the compression title test at `session_db.rs:3267-3274`.
- **Rotation-stable prompt-cache scope - fixed.** `run_native_turn` sets
  `cache_scope` to `compression_lineage_root(session_id)`
  (`native_agent.rs:580-586`), so a rotated child resolves to the same lineage
  root as its parent and the cache prefix survives rotation. `release_conversation`
  is implemented on the conversation agent (`conversation_agent.rs:767`) and the
  live path calls it (`session_commands.rs:296`).
- **Anti-growth accounting - fixed.** The shrink guard now compares the
  redacted, *unwrapped* summary body against the source, so the ~530 chars of
  `SUMMARY_PREFIX`/`SUMMARY_END` no longer bias the check
  (`session_commands.rs:235-246`); `wrap` runs only after the guard passes.
- **Partial-boundary parity - fixed.** `partial_boundary` keeps the earliest
  available user turn as the tail start when fewer than `keep_last` user turns
  exist, and only falls back to full compression when the head would be empty
  (`partial_compress.rs:83-96`), matching the Python contract. Covered by
  `partial_boundary_starts_on_the_nth_newest_user`.
- **Neutral failure text - fixed.** The retry-exhaustion bail is
  `"session route kept changing during compression"` (`session_commands.rs:317`),
  and the push/HTTP handlers say "Compression could not complete. The
  conversation was not changed." (`dispatch.rs:419-423`, `message.rs:249-252`).
  No "preview" wording remains on the live path.
- **Durable turn lease / refresh / release - implemented.** `durable_turn_lease`
  acquires a DB row keyed on the lineage root, refreshes every 60s under a 300s
  TTL, and releases on drop (`durable_turn_lease.rs`). The lease is held for the
  whole turn in both ingress paths (`message.rs:373`, `dispatch.rs:588`), and
  `compress_session` takes it with a 1 ms wait and refuses cleanly when another
  process holds it (`session_commands.rs:126-147`). Stale rows are reclaimed by
  expiry or dead-PID detection (`session_db.rs:875-918, 363-378`), and the
  publish fence re-checks lease ownership before committing
  (`session_db.rs:1417-1430`).
- **Atomic publication fence - verified.** `publish_gateway_compression` still
  runs one `IMMEDIATE` transaction: lease check, role-alternation checks, title
  move, child insert, tail clone, message-count/route upsert, and the
  `ended_at IS NULL` parent-close CAS, aborting with `Ok(false)` on any mismatch
  (`session_db.rs:1385-1600`).

The rotation core, role alternation at the seam, secret handling, and the
in-process race handling all hold up. The findings below are what remains.

---

## Finding 1: Cross-process turn queued behind a compression can still write to the ended parent (Medium)

**Where:**
- `session_admission.rs:51-65` (entry resolved, tip healed, *before* the wait)
- `session_admission.rs:79-90` (durable lease acquired, may block up to the 5 s
  `DEFAULT_LEASE_WAIT`)
- `session_admission.rs:94-107` (post-wait verify)
- `session_store.rs:632, 684-687` (`get_compression_tip` + `heal_compression_tip`
  run only inside `get_or_create`, i.e. before the wait)
- `session_store.rs:864-872` + `session_routing.rs:558-560, 584-600`
  (`reconcile` is a no-op once `database_loaded` is true)

**What the fix does close:** the durable lease keys on the lineage root, so a
turn that is *in flight* holds the lease and makes a concurrent `/compress`
refuse (`session_commands.rs:136-143`), and the common "victim starts after the
commit" case is caught by `heal_compression_tip`, which advances a stale
in-memory route to the DB compression tip during `get_or_create`.

**The residual window:** `admit_turn` resolves the entry and heals the tip
*before* it waits on the durable lease, and the post-wait revalidation
(`session_admission.rs:94-107`) only re-reads the *in-memory* route via
`current_entry_for_source`. In steady state `reconcile` never re-reads
`gateway_routing` (`recovery_scope` returns `None` once `database_loaded` is set,
so `merge_recovered` is skipped), and nothing re-runs `get_compression_tip`
after the wait. So the compression tip is never re-resolved once the lease is
finally acquired.

**Failure scenario (two processes sharing the state DB, the documented
gateway + `hermes` CLI layout):**
1. Process G runs `/compress` on session S, takes the durable lease, and begins
   the multi-second summarization.
2. Process C begins a turn on S. `get_or_create_with_legacy` reads
   `get_compression_tip(S)` - G has not committed yet, so the tip is still S and
   `heal_compression_tip` is a no-op. C's `entry.session_id` stays S. C blocks
   on the durable lease.
3. G commits the rotation (inserts child C\_id, closes S, repoints the route)
   and releases the lease.
4. C acquires the lease (same lineage root). The verify step sees the in-memory
   route still naming S (it was never re-healed), so `same_instance` +
   `session_id == observed` passes. C runs its turn against S, whose `ended_at`
   is now set, and `begin_turn`/`end_turn` append the user+assistant rows to the
   ended parent. Those rows are never in the active child C\_id, so the exchange
   is dropped from the conversation the user sees.

The window is bounded (C must start waiting within the last ~5 s before G's
commit, otherwise its lease wait times out and the turn returns 503 instead of
orphaning), but it is exactly the cross-process lost-turn the durable lease was
added to prevent, so the fix narrows rather than eliminates it.

**Proposed fix:** after the durable lease is acquired, re-resolve the route
(re-run `get_or_create`/`get_compression_tip` for the key) and, if the tip
advanced, drop the lease and retry the admission loop instead of trusting the
pre-wait entry. Equivalently, heal the compression tip inside the post-wait
verify block so it observes cross-process rotations that landed during the wait.

---

## Finding 2: A normal lease release that races the 60 s refresh tick logs a false "ownership was lost" error (Low)

**Where:** `durable_turn_lease.rs:98-125` (refresh task) and
`durable_turn_lease.rs:22-35` (drop path).

On drop, the lease sends the stop signal and spawns a detached thread that
`DELETE`s its row. If the refresh task is already inside its `spawn_blocking`
`refresh_session_turn_lease` call when the delete commits first, the `UPDATE`
matches 0 rows and returns `Ok(false)`, which the task treats as lost ownership
and logs `tracing::error!("durable turn lease ownership was lost")`
(`durable_turn_lease.rs:111-113`). This is a benign teardown race, not a real
ownership loss, but it emits an ERROR-level line on ordinary turn completion
whenever the 60 s tick lands in that narrow window.

**Proposed fix:** suppress the error once the stop signal has been observed, or
have the drop path signal-then-join the refresh task before releasing so a
refresh cannot run concurrently with the release.

---

## Finding 3: The shrink guard counts only `content`, so tool-heavy heads are biased toward refusal (Low)

**Where:** `session_commands.rs:236-246`.

`source_chars` sums `item.message.content.chars().count()` across the head. With
the tool-call fidelity fix, an assistant turn that invoked tools typically has an
empty `content` while its real payload lives in `tool_calls`, which the summarizer
now sees but this sum ignores. A head that is mostly tool activity therefore has
a tiny `source_chars`, so a legitimate, information-dense summary can trip
`summary_body.chars().count() >= source_chars` and be refused. This is
conservative (it refuses rather than losing data) and matches the pre-existing
char-based heuristic, but the tool-call fix makes the asymmetry more visible.
Python's token estimate counts the same overhead on both sides and does not have
this bias.

**Proposed fix:** include `tool_calls` (and tool result content) length in
`source_chars`, or move to a token estimate that measures both sides
symmetrically.

---

## Notes checked and cleared

- **No double-wrap.** `native_agent::summarize_context` returns the trimmed raw
  summary (`native_agent.rs:711`); `conversation_agent` forwards it unchanged
  (`conversation_agent.rs:716-720`); `wrap` is applied exactly once in
  `session_commands.rs:247`, guarded by the non-empty check so the `expect`
  cannot panic.
- **Lease keying survives rotation.** All lease ops resolve
  `compression_lineage_root_on` (`session_db.rs:303`), which is cycle-guarded and
  capped at 100 hops, so acquire/refresh/release/holder all target the same root
  before and after a rotation; the compressor's drop deletes only its own holder
  row and cannot steal a lease another process re-acquired after expiry.
- **Publish aborts safely on a lost lease.** If the lease expires mid-summary,
  the ownership check at `session_db.rs:1417-1430` returns `Ok(false)` and the
  route/history are left unchanged; `compress_session` reports the neutral
  "conversation changed while its summary was being prepared" message.
- **Compressing an already-ended session is safe.** The child `INSERT ... SELECT`
  requires `ended_at IS NULL` and the parent-close CAS requires the same, so a
  session rotated out from under the compressor yields `Ok(false)` and a clean
  abort rather than a double rotation.
- **In-process route revalidation works.** For same-process rotations the
  in-memory index is updated, so the post-wait verify observes the change and
  retries (covered by `stale_waiter_re_resolves_after_route_rotation`). Only the
  cross-process case in Finding 1 escapes it.
- **Role alternation at the seam.** The compacted preamble is validated as
  `user, assistant`, and the cloned tail is validated to begin on `user` and
  alternate, aborting otherwise (`session_db.rs:1399-1488`).
