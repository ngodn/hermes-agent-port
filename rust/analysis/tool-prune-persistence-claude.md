# Tool-result prune: minimal atomic persistence contract (Claude lane)

Scope of this note: the durable-store contract a deterministic, no-LLM tool-result
prune must satisfy when it commits. It does NOT cover the pruning algorithm
itself (that is the pure-algorithm lane), provider usage parsing (AGY lane), or
the Rust implementation/wiring (primary agent). It is a read-only investigation.
No Rust source was edited; no cargo or git was run.

All line references are to the tree at commit `546767be10` on branch
`rust-rewrite`.

---

## 1. What the Python prune actually asks of the store

The proactive prune is a single call site. `ContextCompressor.prune_tool_results_only`
(`agent/context_compressor.py:4519`) does all its gating in memory and then, and only
then, persists through exactly one store method:

```python
# agent/context_compressor.py:4634
session_db.archive_and_compact(
    session_id,
    pruned_msgs,
    model_config_patch={
        PROACTIVE_PRUNE_REARM_MODEL_CONFIG_KEY: next_rearm_tokens,
    },
)
```

Three things are notable and they define the contract:

1. It passes NO `watermark`, NO `lock_holder`, NO `tail_count`
   (`agent/context_compressor.py:4634-4640`). It uses the plainest
   `archive_and_compact` shape: soft-archive every active row, insert the pruned
   list as the new active set, merge one model_config key.
2. The ONLY model_config mutation is the rearm watermark key
   `_proactive_prune_rearm_tokens` (`agent/context_compressor.py:317`).
3. On ANY exception from the store the compressor returns the INPUT list object
   unchanged and does NOT advance its in-memory rearm counter
   (`agent/context_compressor.py:4641-4647`). The caller then treats it as a no-op
   because `result is messages` (`agent/conversation_loop.py:8368`).

The capability gate at `agent/context_compressor.py:4593-4599` refuses to even run
the scan if the bound store has no callable `archive_and_compact`, so "the store
cannot persist atomically" is a designed, logged no-op, not a crash.

Post-commit, the compressor stamps `_DB_PERSISTED_MARKER` on the exact dict
instances it returns (`agent/context_compressor.py:4650`,
`stamp_db_persisted_markers` at `agent/context_compressor.py:423`). That marker is
the append-only flush's dedup key: without it the next `_persist_session` walk
re-INSERTs the whole pruned transcript on top of the rows it just replaced. This
is a Python in-process concern (the flush and the prune share the live dict list);
it is called out here because the Rust equivalent must guarantee the same "do not
double-write" property by some means, even though Rust does not share Python's
mutable dict identity trick.

### Why prune does NOT need the watermark/lock_holder machinery

`archive_and_compact`'s `watermark` (`hermes_state.py:13430-13441`) and
`lock_holder` (`hermes_state.py:13443-13447`) exist because a full compression
makes a SLOW external provider summary call, during which concurrent turns can
append rows and a competing writer can steal the compression lease. The prune has
no external call: it runs synchronously inside the post-tool gate of the turn that
already owns the loop (`agent/conversation_loop.py:8354-8380`). There is no summary
window, so there is no concurrent-append tail to protect and no lease to re-verify.
That is why the Python call site deliberately omits all three. The Rust contract
should preserve that simplicity and NOT require the caller to compute a watermark
for a prune. See section 4 for the one residual race that still needs a guard.

---

## 2. What `archive_and_compact` guarantees (the reference contract)

`SessionDB.archive_and_compact` (`hermes_state.py:13402-13612`) runs its whole body
inside one `self._execute_write(_do)` transaction (`hermes_state.py:13612`). The
guarantees, in commit order, are:

1. Optional lease fence (skipped by prune) at `hermes_state.py:13468-13482`.
2. model_config merge at `hermes_state.py:13484-13492` via `_merge_model_config_json`
   with `on_missing="raise"` (`hermes_state.py:9705-9751`). Merge semantics: parse
   existing JSON tolerantly, overlay patch keys, a `None` value deletes a key, an
   empty result serializes to NULL. Prune relies on this to add the rearm key while
   keeping every other lineage key (`_branched_from`, `_delegate_from`, arbitrary
   user keys) alive. Test proof: `keep=value` survives alongside the rearm key
   (`tests/agent/test_proactive_prune_restart_safety.py:98-99`).
   `on_missing="raise"` is what makes a prune against a vanished session row fail
   the whole transaction instead of silently committing a half-state
   (`hermes_state.py:13486-13492`).
3. Soft-archive of the live set. Plain prune path (no watermark, no tail_count)
   hits the else branch at `hermes_state.py:13569-13573`:
   `UPDATE messages SET active = 0, compacted = 1 WHERE session_id = ? AND active = 1`.
   `active = 0` hides the rows from the live load; `compacted = 1` keeps them
   discoverable by `search_messages` (the archive-vs-rewind distinction,
   `hermes_state.py:13515-13523`). This is a content-preserving UPDATE, so FTS
   entries survive (`hermes_state.py:13424-13428`).
4. Insert of the pruned list as fresh active rows via `_insert_message_rows`
   (`hermes_state.py:13574-13576`, body at `hermes_state.py:13175-13267`). CRITICAL:
   this insert carries the FULL column set, not just role/content. See section 3.
5. Counter reconciliation. `message_count` and `tool_call_count` are set to the
   ACTIVE (post-insert) counts, and model_config is written in the same UPDATE
   (`hermes_state.py:13599-13609`). For prune, `model_config_patch is not None`, so
   the branch at `hermes_state.py:13604-13609` runs and writes counts + merged
   model_config together.

Atomicity is the whole point. Test `test_archive_model_config_patch_rolls_back_with_transcript`
(`tests/agent/test_proactive_prune_restart_safety.py:189-211`) forces a failure
mid-commit and asserts BOTH the transcript AND the model_config revert. Test
`test_prune_persistence_failure_is_a_noop`
(`tests/agent/test_proactive_prune_restart_safety.py:164-186`) asserts that a raising
`archive_and_compact` leaves the transcript, the model_config, and the in-memory
rearm counter all untouched, and that the compressor returns the input object.

---

## 3. Which columns must be cloned / rewritten (the central finding)

This is where the current Rust compression path is NOT reusable for a prune as-is.

Python `_insert_message_rows` (`hermes_state.py:13228-13258`) inserts 22 columns:

```
session_id, role, content, tool_call_id, tool_calls, tool_name,
effect_disposition, timestamp, token_count, finish_reason,
reasoning, reasoning_content, reasoning_details, codex_reasoning_items,
codex_message_items, platform_message_id, observed, _compressed_summary,
active, api_content, display_kind, display_metadata
```

The pruned message list is NOT plain text. A prune rewrites tool-result BODIES but
keeps the surrounding tool structure: assistant rows still carry `tool_calls`, tool
rows still carry `tool_call_id` and `tool_name`, and reasoning sidecars still ride
on assistant rows. Proof from the test fixtures: `_assistant_call` produces a row
with `tool_calls`, `_tool_result`/`_tool_msg` produce rows with `tool_call_id`
(`tests/agent/test_proactive_prune_restart_safety.py:18-32`,
`tests/agent/test_proactive_tool_result_pruning.py:44-54`). The committed-prune
wiring test asserts the tool ROWS survive as tool rows in the final transcript
(`tests/run_agent/test_proactive_prune_loop_wiring.py:185-190`).

Now compare the Rust compacted-insert. Both `publish_gateway_compression`
(`rust/crates/hermes-gateway/src/session_db.rs:1850-1864`) and
`publish_gateway_in_place_compression`
(`rust/crates/hermes-gateway/src/session_db.rs:2052-2066`) insert compacted rows with
ONLY:

```
session_id, role, content, api_content(=NULL), timestamp,
active(=1), _compressed_summary(=index==0), compacted(=0)
```

driven by `HistoryMessage { role, content, api_content }`
(`rust/crates/hermes-gateway/src/session_db.rs:71-77`). That shape is correct for a
compression SUMMARY (a bare user/assistant text pair) but wrong for a prune:

- `tool_calls`, `tool_call_id`, `tool_name` are dropped to NULL. A reloaded
  transcript then has assistant rows that advertise no tool calls followed by tool
  rows that answer no id. That violates role/tool pairing and, on strict templates,
  makes the request unsendable. `complete_turn_sequence`
  (`rust/crates/hermes-gateway/src/session_db.rs:448-477`) would even reject such a
  list at validation time because a `tool` row with no `tool_call_id` fails the
  `TailPhase::Tools` check (`session_db.rs:459-465`).
- `api_content` is hard-set NULL (`session_db.rs:1855`, `session_db.rs:2057`). A prune
  that rewrote a body must persist the rewritten api_content sidecar or the
  prompt-cache divergence the sidecar exists to close reopens.
- `_compressed_summary` is set to `index == 0` (`session_db.rs:1861`,
  `session_db.rs:2063`). For a prune NONE of the rows is a summary; the first pruned
  row is just the first real message. Setting `_compressed_summary=1` on it would
  mislabel it.

Conclusion: a prune commit needs an insert path that writes the SAME wide column
set Python's `_insert_message_rows` does, at minimum: `role, content, api_content,
tool_call_id, tool_calls, tool_name, timestamp` plus whatever reasoning/display
sidecars the Rust `messages` table actually carries. The Rust `messages` table
already has `tool_call_id, tool_calls, tool_name, display_kind, display_metadata`
(`rust/crates/hermes-gateway/src/session_db.rs:2681-2694`) and the wide append path
`append_message_with` already writes them (`session_db.rs:2886-2902`), so the
columns exist; only the compaction insert is narrow.

Note on Rust's tail clone: `clone_messages_by_id`
(`rust/crates/hermes-gateway/src/session_db.rs:498-543`) already does a pure-SQL
column clone that carries EVERY column except `id`/`session_id`/`active`/`compacted`,
so a retained/concurrent tail keeps its metadata byte-exact. The gap is strictly the
NEW rows (the compacted/pruned bodies), which come from the caller and today only
carry three fields.

---

## 4. CAS / watermark / lease guards required for a prune

Reference behavior (Python) vs what a prune needs:

- Watermark: NOT required for prune. Python omits it (`context_compressor.py:4634`).
  Rationale in section 1: no slow summary window. The plain archive branch archives
  all active rows and reinserts the pruned list, which is safe precisely because the
  prune list already IS the full active transcript rewritten in place. The Rust
  in-place path takes `watermark`/`tail_start_id` (`session_db.rs:674-683`); for a
  prune the caller can leave the tail set empty (no concurrent tail to re-sequence)
  and simply pass the whole rewritten list as the compacted set.

- Compression lease / lock_holder: NOT required for prune. Python omits it. But note
  the Rust in-place path ALSO enforces two guards the Python prune does not:
  (a) a `turn_lease_holder` check against `session_turn_leases`
  (`session_db.rs:1922-1935`), and (b) a durable-route CAS: the row is only rewritten
  if `gateway_routing` still points at this session id
  (`session_db.rs:1936-1951`), plus a "session still live" check
  (`session_db.rs:1952-1962`). These are Rust-gateway invariants that have no Python
  `archive_and_compact` equivalent (Python's routing lives elsewhere). For a prune
  they are HARMLESS-to-keep and actually desirable: they close the one residual race.

- The one residual race ("racing an admitted turn"): although the prune has no
  external call, it runs after tool results are posted and before the next provider
  call (`agent/conversation_loop.py:8354-8395`). If another process adopted the same
  session id (the concurrent-fork incident,
  `tests/agent/test_compression_concurrent_fork.py:1-30`) it could append or compress
  between the prune's read and its commit. Python leans on the caller owning the turn;
  Rust already has a stronger, cheaper fence in the same transaction: the
  `turn_lease_holder` + durable-route CAS above. Recommendation: a Rust prune commit
  SHOULD reuse those guards (verify the turn lease and that routing still names this
  session) so a prune cannot clobber a transcript another admitted turn is rewriting.
  This is a superset of Python's safety, obtained for free from the existing seam.

- CAS on the rearm watermark itself: NONE needed. The rearm value is advisory
  hysteresis, last-writer-wins, merged into model_config in the same transaction as
  the transcript. There is no compare-and-set requirement; the durability guarantee is
  simply "counts, model_config, and transcript commit together or not at all"
  (`hermes_state.py:13604-13609`, proven by
  `tests/agent/test_proactive_prune_restart_safety.py:189-211`).

---

## 5. Archive semantics, model_config / rearm behavior, rollback

Archive semantics for the plain prune path:

- Every currently-active row: `active = 0, compacted = 1` (archived-but-discoverable).
  Rust already does exactly this in the in-place path
  (`rust/crates/hermes-gateway/src/session_db.rs:2033-2037`), then flips
  `compacted = 0` back on the ids it re-clones (`session_db.rs:2038-2050`). For a prune
  with no retained-original clones (the pruned bodies are fresh inserts, not clones)
  the `compacted = 0` re-flip set is empty, which is correct: all originals stay
  `compacted = 1` and the fresh pruned rows are inserted with `compacted = 0`.
- Archived rows must stay in the FTS index. Rust's archive is a content-preserving
  `UPDATE ... SET active/compacted`, so the FTS triggers
  (`session_db.rs:2713-2729`) do not drop them. Good. A DELETE-based approach would be
  wrong (it evicts from FTS); do not use `replace_messages`-style DELETE semantics
  (Python `replace_messages` at `hermes_state.py:13350-13354` is the DESTRUCTIVE path
  and is explicitly NOT what a prune uses).

model_config / rearm behavior:

- Merge one key `_proactive_prune_rearm_tokens` into model_config, preserving all
  other keys, in the same transaction (`hermes_state.py:9746-9750`,
  `agent/context_compressor.py:4636-4640`).
- `next_rearm_tokens = after + max(reclaimed, proactive_prune_tokens,
  proactive_prune_min_reclaim_tokens)` (`agent/context_compressor.py:4625-4630`). This
  value is computed by the algorithm lane; the persistence contract only has to store
  it verbatim as a REAL/number under that key and read it back on resume
  (`agent/context_compressor.py:2791-2792` reads it, restart test at
  `tests/agent/test_proactive_prune_restart_safety.py:106,161`).
- A model switch clears the key (`agent/context_compressor.py:2814`, test at
  `tests/agent/test_proactive_prune_restart_safety.py:214-231`). That is a SEPARATE
  path (`patch_session_model_config` with `{key: None}`), not part of the prune commit,
  but the Rust store needs a "delete key" model_config merge to support it. Rust has no
  `patch_session_model_config`/`_proactive_prune_rearm_tokens` support today (grep
  shows the key only under Python), so this is net-new.

Rollback / failure behavior:

- The commit is all-or-nothing. On failure NOTHING is archived and NOTHING inserted;
  every pre-prune row stays `active = 1`
  (`tests/run_agent/test_in_place_commit_rollback_99477.py:1-19`,
  `tests/agent/test_proactive_prune_restart_safety.py:164-186`). Rust gets this from
  the single `tx.commit()` at the end of the in-place function
  (`rust/crates/hermes-gateway/src/session_db.rs:2074`): any earlier `?` return drops
  the transaction and rolls back.
- The store method must signal failure to the caller distinctly from "committed
  nothing on purpose." Python raises; Rust returns `Ok(false)` for the guard-rejections
  (bad routing, lost lease, empty input) and `Err` for real SQL failure. The caller
  must treat BOTH `Ok(false)` and `Err` as "keep the original transcript, do not
  advance the in-memory rearm counter, do not stamp persisted markers." That mirrors
  Python's `except Exception: return messages, 0`
  (`agent/context_compressor.py:4641-4647`).

---

## 6. Concrete Rust API / type suggestions

The cleanest fit reuses the existing in-place seam rather than inventing a parallel
one. Two shapes are viable:

Option A (recommended): a dedicated `GatewayToolPrunePublish` + method, so the intent
and the wide-column insert are explicit and the compression-summary path keeps its
narrow `HistoryMessage`.

```rust
/// One row of a rewritten (pruned) transcript. Unlike `HistoryMessage`
/// (summary text only), a prune keeps the tool scaffolding, so the full
/// metadata set rides through and is inserted verbatim.
pub struct PrunedMessage {
    pub role: String,
    pub content: String,
    pub api_content: Option<String>,
    pub tool_call_id: Option<String>,
    pub tool_calls: Option<String>,   // JSON text, already serialized
    pub tool_name: Option<String>,
    // add display_kind / display_metadata if the prune ever carries them
}

pub struct GatewayToolPrunePublish<'a> {
    pub scope: &'a str,
    pub session_key: &'a str,
    pub session_id: &'a str,
    pub pruned_messages: &'a [PrunedMessage],
    /// Verbatim rearm watermark to merge under
    /// `_proactive_prune_rearm_tokens`; None leaves model_config untouched.
    pub rearm_tokens: Option<f64>,
    pub turn_lease_holder: Option<&'a str>,
}

impl SessionDb {
    /// Atomically archive the live transcript (active=0, compacted=1) and
    /// insert `pruned_messages` as the new active set, merging the rearm
    /// watermark into model_config in the same transaction. Returns Ok(true)
    /// on commit, Ok(false) on a guard rejection (lost lease / stale route /
    /// empty or role-invalid input), Err on SQL failure. Both non-true
    /// outcomes mean "keep the original transcript."
    pub fn publish_tool_result_prune(
        &self,
        change: &GatewayToolPrunePublish<'_>,
    ) -> rusqlite::Result<bool>;
}
```

Implementation notes, all reusing existing helpers:
- Validate role/tool pairing on the pruned list with `complete_turn_sequence`
  (`session_db.rs:448`) built from `(role, tool_calls, tool_call_id)` BEFORE any write.
- Reuse the turn-lease + durable-route CAS block verbatim from
  `publish_gateway_in_place_compression` (`session_db.rs:1922-1962`).
- Archive with the existing `UPDATE ... SET active = 0, compacted = 1`
  (`session_db.rs:2033-2037`). Do NOT re-flip `compacted = 0` on anything (no clones).
- INSERT each pruned row with the WIDE column set (mirror `append_message_with`
  at `session_db.rs:2886-2902`, plus `api_content` and `_compressed_summary = 0`,
  `compacted = 0`, `active = 1`). This is the one line of net-new SQL.
- Merge model_config with a `json_set`/`json_remove` helper (Rust already uses
  `json_set` for `_reset_from` at `session_db.rs:1582-1585`), preserving other keys.
  Store `rearm_tokens` as a REAL. If `None`, skip the model_config UPDATE.
- Recompute counts with `active_transcript_counts` (`session_db.rs:479`) and write
  `message_count`/`tool_call_count`/`last_activity_at` (mirror `session_db.rs:2068-2073`).
- Single `tx.commit()` at the end for all-or-nothing.

Option B: extend `HistoryMessage` with the optional tool/api fields and widen the
existing compaction insert. Rejected: it entangles the summary path (which must set
`_compressed_summary = index==0`) with the prune path (which must set it to 0), and
risks a future summary regression. Keep the two intents separate.

A separate small method for the model-switch clear is also needed (Python
`patch_session_model_config` with `{key: None}`):

```rust
pub fn clear_proactive_prune_rearm(&self, session_id: &str) -> rusqlite::Result<bool>;
```

Both new methods should fail closed on empty `session_id` and return `false` when no
row matches, matching the convention already established for the compression-guard
setters in this file (`session_db.rs:1209-1278`).

---

## 7. Test matrix (for the primary agent to implement)

Persistence-contract tests, phrased against the Rust store directly (two `SessionDb`
handles on one temp file where cross-connection durability matters, as the existing
`session_db` tests do):

1. Happy path, tool scaffolding preserved. Seed a transcript with an
   assistant(tool_calls)+tool(tool_call_id,tool_name) pair, publish a prune that
   rewrites the tool body. Assert the reloaded active set has the rewritten body AND
   still carries `tool_calls`/`tool_call_id`/`tool_name` byte-exact. (Guards against
   the section-3 metadata loss.)
2. api_content survives. Prune a row that carries an api_content sidecar; assert the
   sidecar is persisted, not NULLed.
3. Role/tool pairing rejected. Publish a pruned list whose tool row has no
   `tool_call_id`; assert `Ok(false)` and the ORIGINAL transcript is untouched
   (no archive, no insert).
4. Archive semantics. After a prune, assert originals are `active = 0, compacted = 1`
   (still discoverable / in FTS), fresh rows are `active = 1, compacted = 0`, and
   `_compressed_summary = 0` on every fresh row.
5. Counts reconciled. `message_count`/`tool_call_count` equal the ACTIVE post-prune
   counts (via `active_transcript_counts`), not the pre-prune totals.
6. model_config merge preserves siblings. Seed `model_config={"keep":"value"}`,
   prune with `rearm_tokens=Some(N)`; assert reloaded model_config has BOTH `keep`
   and the rearm key = N. (Mirror
   `tests/agent/test_proactive_prune_restart_safety.py:98-99`.)
7. Atomic rollback with transcript. Force a mid-commit SQL failure (e.g. a poisoned
   row) and assert BOTH the transcript and model_config revert, and the method returns
   `Err`. (Mirror
   `tests/agent/test_proactive_prune_restart_safety.py:189-211` and
   `tests/run_agent/test_in_place_commit_rollback_99477.py`.)
8. Missing session fails closed. Prune a non-existent session id; assert `Ok(false)`
   / no partial write (Python's `on_missing="raise"`,
   `hermes_state.py:13486-13492`).
9. Lost turn lease. Set `turn_lease_holder` to a holder that no longer owns the lease;
   assert `Ok(false)`, no write. (Reuse the in-place lease-fence test pattern.)
10. Stale route CAS. Point `gateway_routing` at a different session id; assert
    `Ok(false)`, no write.
11. Rearm survives reopen. Prune, drop the handle, reopen, assert the rearm key reads
    back exactly. (Mirror
    `tests/agent/test_proactive_prune_restart_safety.py:106`.)
12. Model-switch clear. `clear_proactive_prune_rearm` removes only the rearm key and
    leaves other model_config keys intact; no-op on missing session / empty id.
    (Mirror `tests/agent/test_proactive_prune_restart_safety.py:214-231,234-246`.)
13. Empty / no-op inputs. Empty `pruned_messages` returns `Ok(false)` and writes
    nothing (matches the caller's "commit only a NEW non-empty list" contract,
    `agent/conversation_loop.py:8368`).
14. FTS discoverability. After a prune, a full-text search still finds the archived
    original bodies (they remained `compacted = 1` and indexed).

---

## 8. Exact file:line index

Python:
- `agent/context_compressor.py:4519` prune entry; `:4593-4599` capability gate;
  `:4634-4640` the archive_and_compact call; `:4641-4647` failure no-op;
  `:4650` post-commit stamp; `:317` rearm key; `:423` `stamp_db_persisted_markers`;
  `:2791-2814` rearm load / model-switch clear.
- `agent/context_engine.py:194` default no-op hook.
- `agent/conversation_loop.py:8354-8395` post-tool prune wiring and no-op contract.
- `hermes_state.py:13402-13612` `archive_and_compact` (whole body);
  `:13175-13267` `_insert_message_rows` (the 22-column insert);
  `:13384-13400` `get_active_message_watermark`;
  `:13269-13367` `replace_messages` (DESTRUCTIVE, the path a prune must NOT use);
  `:9705-9751` `_merge_model_config_json`; `:9753-` `patch_session_model_config`.
- Tests: `tests/agent/test_proactive_prune_restart_safety.py` (rearm durability,
  rollback, model-config merge, incapable-store, model-switch clear);
  `tests/agent/test_proactive_tool_result_pruning.py` (fixture shapes);
  `tests/run_agent/test_proactive_prune_loop_wiring.py` (caller no-op contract,
  tool rows survive); `tests/test_compression_watermark_commit.py` (watermark/tail,
  not used by prune but the reference for concurrent-tail semantics);
  `tests/run_agent/test_in_place_commit_rollback_99477.py` (atomic rollback);
  `tests/agent/test_compression_concurrent_fork.py` (the race the lease/route CAS
  closes).

Rust (`rust/crates/hermes-gateway/src/session_db.rs`):
- `:71-77` `HistoryMessage` (narrow, summary-only);
- `:448-477` `complete_turn_sequence` (role/tool validation);
- `:479-496` `active_transcript_counts`;
- `:498-543` `clone_messages_by_id` (wide column clone, used for tails);
- `:674-683` `GatewayInPlaceCompressionPublish`;
- `:1895-2076` `publish_gateway_in_place_compression` (the reusable seam: lease fence
  `:1922-1935`, route CAS `:1936-1962`, archive `:2033-2037`, narrow compacted insert
  `:2052-2066`, counts `:2068-2073`, commit `:2074`);
- `:1684-1889` `publish_gateway_compression` (rotation variant, same narrow insert
  `:1850-1864`);
- `:2680-2696` `messages` schema; `:2705-2729` FTS table + triggers;
- `:2435-2477` `ensure_message_schema` (api_content/_compressed_summary/compacted
  migration); `:2886-2902` `append_message_with` (the existing wide insert to mirror);
- `:1163-1278` compression-guard load/setters (the convention new methods should
  follow: fail-closed on empty id, `false` on no-match).

No `_proactive_prune_rearm_tokens` key, no `patch_session_model_config` equivalent,
and no wide-column compaction insert exist in Rust today. Those three are the net-new
surface a prune commit requires.
