# Session Store Transition Review

## Maintainer verification, 2026-09-07

- Confirmed a specific snapshot gap: activity updated during the process probe
  was invisible to Rust's reset calculation. Executing Python's actual
  `_should_reset` method with a probe that touches the entry preserves the
  session. The Rust regression failed before the fix and passes after refreshing
  policy fields from the same entry instance following unlocked I/O. This does
  not claim to eliminate the decision-to-publication race also present in Python.
- Rejected the null-token claim below. `SessionEntry::from_dict` always supplies
  the missing-field default of zero; explicit null is retained. Python's
  `entry.last_prompt_tokens > 0` raises TypeError for null, so Rust's error is
  intentional parity, not a coercion bug.
- Compression healing and the reset ID guard follow Python's phase 2 ordering.
- Peer repair is an integration requirement, not a missing unconditional call
  in Python's transition. Python refreshes after successful creation and in
  `update_session`; its recovered/healthy transition paths do not independently
  refresh every peer. The optional Rust dispatcher now calls `update_session`
  after turns. Startup/HTTP integration remains pending.

The original helper report below is retained for traceability; its claims are
superseded by these source checks where they differ.

Analysis of `session_store.rs` (`get_or_create_session`/`transition`) against Python reference `gateway/session.py` (`get_or_create_session`/`_get_or_create_session_impl`).

## 1. Ordering and Stale vs Compression Decisions
- Compression Healing Coupling: In `transition` (lines 191-197), `heal_compression_tip` mutates `current.session_id` to `tip` before evaluating `same_id`. Because `observed` was captured prior to healing, `current.session_id == entry.session_id` is unconditionally false when healed. This bypasses the reset/stale branch and forces the healthy branch, matching Python line 3079 (`entry.session_id != _stale_session_id`). However, Rust couples local mutation with external concurrency detection rather than checking distinct flags.
- Stale Self-Heal Drop: When an entry is ended in SQLite without an active reset reason, Rust correctly evicts it from `index.entries` and evaluates DB recovery (`query_recoverable`), matching Python #54878. Rust omits Python's warning log on stale drop.

## 2. Races and Concurrency
- Flight Coordination: `SessionFlights` correctly serializes transitions per routing key. Overlapping callers join as waiters and share the owner's outcome without duplicate DB creations.
- Detached Snapshot Window: `observed` is a cloned value snapshot held across unlocked SQLite I/O. If an external caller executes `update_session` during this window, `current.updated_at` is bumped in the index, but `observed` retains stale timestamps. Because `same_id` only compares `session_id`, `transition` may execute an idle reset against outdated timestamps.
- Waiter Synchronization: Waiters that unblock with `touch_activity=true` invoke `update_session`, refreshing `updated_at`, persisting via single-entry UPSERT, and repairing peer rows. Waiters re-lock the index and verify instance identity before returning, mirroring Python.

## 3. Reset Lineage
- Predecessor Promotion: Rust correctly calls `promote_to_session_reset` before creating the new session row, preventing accidental-close rows from winning restart recovery.
- Lineage Persistence: The successor session records `prev_session_id`, `parent_session_id`, and `model_config: {"_reset_from": parent_id}`, adhering to Python #12857.
- Map Indexing Panic and Coercion Bug: Line 200 uses direct indexing `&current.fields["last_prompt_tokens"]` and `.ok_or_else(|| anyhow!("last_prompt_tokens must be numeric"))`. In Python, `entry.last_prompt_tokens > 0` defaults safely to false if null or absent. In Rust, missing keys trigger a runtime panic, while null values fail the transition with an error.

## 4. Profile Isolation
- Namespace and Key Routing: Session keys encode the profile namespace (`agent:<profile>:...` or `agent:main`). Named profiles map to isolated databases at `profiles/<canonical>/state.db`, with unprovisioned or deleted profiles returning `None` to prevent falling back to ambient root storage.
- Routing vs History Separation: Routing index persistence (`persist_metadata`/`persist_full`) correctly targets the dedicated routing store (`self.databases.routing()`), matching Python `_routing_db`.
- Recovery Profile Filtering: `query_recoverable` enforces `recovered_profile_allowed`, preventing foreign profile sessions from being adopted.

## 5. Missing Per-Turn Peer Repair
- Deferred Creation Deficit: When `create_session` fails (line 309), Rust logs that creation is deferred to peer repair, but `refresh_peer` is skipped.
- Healthy Path Omission: In Python, every normal turn refreshes peer metadata via `update_session` or peer repair. In Rust, the single-flight Owner that reuses an existing healthy session never calls `refresh_peer` during `transition`. Only waiters and explicit `update_session` callers invoke `refresh_peer`. If dispatch has not yet wired `update_session`, session rows that failed initial creation or were delayed by profile provisioning are never self-healed.
- Recovered Reopen Omission: Reopening a recovered session (line 246) issues `db.reopen_session` but omits `refresh_peer`, leaving peer records stale in SQLite.
