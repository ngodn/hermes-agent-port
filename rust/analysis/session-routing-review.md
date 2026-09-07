# Session Routing and Storage Migration Review

Reviewer: Gemini 3.8 Flash high assisting Codex.
Scope: `session_routing.rs`, `session_entry.rs`, and `session_db.rs` (gateway routing methods).
Audited against: `gateway/session.py`, `hermes_state.py`, and `hermes_state_schema.py`.

## Correctness Findings

### 1. Data Loss on Candidate Fallback During Outage Recovery
- Reference: `rust/crates/hermes-gateway/src/session_routing.rs:88-93`, `104-108`, `155-166`
- Priority: High
- Scenario:
  1. Start `RoutingIndex` with DB unavailable (`ensure_loaded(None)`). Store loads `k1` from mirror.
  2. Database becomes available containing durable entries `k1` and `k2`.
  3. Call `save_entry("k1", Some(candidate), Some(db), false)`.
  4. Upsert fails in `persist_entry` (e.g. transient lock or SQL trigger failure).
  5. `save_entry` falls back to `writer.persist` using `self.serialized()`. Because `save_entry`
     omitted `ensure_loaded` and `reconcile`, `self.entries` never loaded `k2`.
  6. `writer.persist` executes `replace_gateway_routing_entries`, which deletes all scope rows
     in SQLite and writes only `k1`. Durable entry `k2` is permanently lost.
- Fix: Call `self.ensure_loaded(database)` (or `self.reconcile(database)`) at the start of `save_entry`.

### 2. Cold save_entry Silently Drops Updates and Skips Primary Reconciliation
- Reference: `rust/crates/hermes-gateway/src/session_routing.rs:80-82`, `gateway/session.py:1671-1673`, `3201`
- Priority: Medium
- Scenario:
  1. Construct `index = RoutingIndex::new(dir)` without calling `ensure_loaded` or `snapshot`.
  2. Call `index.save_entry("k1", None, Some(db), false)`.
  3. `self.entries.get("k1")` returns `None` because entries were never loaded; `save_entry`
     returns `Ok(())` silently without saving to DB or mirror.
  4. On a store loaded via fallback, successive successful `save_entry` calls never trigger
     `reconcile(database)`, leaving `self.database_loaded = false` until an unrelated `save()` occurs.
- Fix: Ensure `self.ensure_loaded(database)` executes before checking `self.entries.get(key)`.

### 3. Redundant Generation Allocation on Upsert Fallback Without Candidate
- Reference: `rust/crates/hermes-gateway/src/session_routing.rs:84`, `94`, `gateway/session.py:2166`
- Priority: Low
- Scenario:
  1. `save_entry("k1", None, Some(db), false)` allocates `revision = self.next_revision()` (e.g. 1).
  2. `persist_entry` returns false. The `else` branch calls `self.save(database, mirror)`.
  3. `self.save` calls `self.snapshot`, which calls `self.next_revision()` again (revision 2).
  4. Revision 1 is skipped, burning two generation numbers for one full write.

## Test Oracle and Golden Gaps

1. `rust/tools/gen_session_entry_goldens.py:55-58`: Value permutation sweeps omit 8 fields:
   `cache_read_tokens`, `cache_write_tokens`, `last_prompt_tokens`, `cost_status`, `resume_reason`,
   `auto_reset_reason`, `reset_had_activity`, and `display_name`.
2. `rust/tools/gen_session_entry_goldens.py:211-222`: Schema repair oracle only tests tables that
   already have a `scope` column; it omits the pre-migration legacy schema where `scope` is absent.
3. `rust/tools/gen_session_entry_goldens.py:172-198`: Replays only full `persist()` writes.
   `persist_entry()`, single-entry candidate persistence, and fallback triggers lack golden coverage.
4. `rust/crates/hermes-gateway/src/session_routing.rs:tests`: Missing unit tests for cold
   `save_entry` invocations and candidate fallback recovery under transient DB errors.

## Migration and Boundary Notes

- Schema heal: `SessionDb::heal_routing_schema` in `session_db.rs:231-264` correctly adds `scope`,
  rebuilds composite `(scope, session_key)` PK, and resolves duplicates by `updated_at ASC`.
- Monotonic ordering: Writer serialization correctly drops obsolete snapshots and folds fast writes.
