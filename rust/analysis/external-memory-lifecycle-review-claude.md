# External-memory turn-lifecycle checkpoint review (claude)

Scope: the uncommitted diff on branch `rust-rewrite` that wires the real Python
external-memory turn lifecycle into native Rust conversations through the
persistent extension host. Files audited:

- `hermes_cli/rust_extension_host.py` (`turn_start` / `turn_complete` / `session_end` / `flush_pending`)
- `rust/crates/hermes-gateway/src/extension_host.rs` (new client methods + tests)
- `rust/crates/hermes-gateway/src/native_agent.rs` (`run_native_turn`, `run_model_turn`)
- `rust/crates/hermes-gateway/src/conversation_agent.rs` (context threading)
- `rust/crates/hermes-gateway/src/session_db.rs` (api_content column, migration, replay, turn count)
- `rust/crates/hermes-gateway/src/main.rs` (startup integration test)

Compared against the live Python paths: `agent/turn_context.py`
(`build_turn_context`), `agent/turn_finalizer.py` (`finalize_turn`),
`run_agent.py` (`_sync_external_memory_for_turn`), `agent/memory_manager.py`,
`agent/memory_provider.py`, `agent/conversation_loop.py`.

Bottom line: the core turn path (turn_number, api_content compose/persist/replay,
prefetch gating, fail-open) is a faithful port and the DB migration is safe. The
real problems are in what gets handed to `sync_turn` and when, plus a set of
methods that are implemented but never driven in production. No async deadlock,
no lost/duplicated stream events, no FTS corruption found.

---

## HIGH

### H1. `sync_turn` receives a stripped 2-message array, not the turn transcript
`native_agent.rs:603-616` builds the sync payload as exactly two messages:

```rust
let messages = [
    json!({"role":"user", "content": clean_content}),
    json!({"role":"assistant", "content": response}),
];
host.turn_complete(&clean_content, &response, &messages)
```

Python forwards the full working conversation list. `finalize_turn(..., messages=messages)`
(`conversation_loop.py:9310`, `turn_finalizer.py:803-808`) passes the entire
OpenAI-style `messages` array (all prior turns plus this turn's assistant
tool-call messages and tool-result messages) into
`_sync_external_memory_for_turn`, which forwards it as `sync_kwargs["messages"]`
to `sync_all` (`run_agent.py:4982-4988`). Providers that opt into it via
`_provider_sync_accepts_messages` (`memory_manager.py:732-742`) get the whole
transcript.

Failure scenario: a tool-aware memory provider that mines tool results or
multi-turn structure (the documented reason `messages` is forwarded at all) sees
none of it on the Rust path. On every tool-using turn it receives only the final
user text and final assistant text, so any fact that lived in a tool result is
never persisted to long-term memory. The fixture test at
`extension_host.rs:947-953` locks this stripped shape in as "correct", so the
regression is baked into the test suite.

This is a turn-lifecycle defect, in scope for this checkpoint.

### H2. Synced assistant text diverges from the persisted transcript reply
`run_native_turn` accumulates the assistant `response` from `MessageChunk` events
only (`native_agent.rs:582-586`). The gateway's own transcript reply, built in
`dispatch.rs:419-435`, additionally folds in `Commentary` text and inserts
`"\n\n"` separators on non-final `MessageStop`. So the string handed to
`sync_turn` (`response`) is not the same string written to the transcript
(`reply`), and neither is guaranteed to equal Python's `final_response`.

Failure scenario: a turn whose visible answer includes a Commentary block, or a
multi-part answer separated by a non-final `MessageStop`, is stored in external
memory as a shorter/differently-joined string than what the user saw and what the
transcript holds. Memory and transcript drift apart on exactly the turns with the
richest content. Python derives both the transcript and the sync text from the
same `final_response`, so they never disagree.

In scope for this checkpoint.

---

## MEDIUM

### M1. Sync fires before the assistant reply is persisted (ordering reversed)
Python persists the assistant row first, then syncs: `_persist_session(...)` runs
at `turn_finalizer.py:473`, and `_sync_external_memory_for_turn` runs later at
`turn_finalizer.py:803`. In Rust the order is inverted: `run_native_turn` calls
`host.turn_complete` (which enqueues `sync_all`) at `native_agent.rs:603-616`,
and the assistant row is only written afterward by `end_turn` back in the
dispatcher at `dispatch.rs:446`.

Consequences:
- If a provider's `sync_turn` reads the session DB (rather than the passed
  `messages`), it sees a transcript that is missing the just-completed assistant
  turn.
- A crash between `turn_complete` and `end_turn` leaves external memory ahead of
  the durable transcript (the "sync-before-persistence" case). Python's ordering
  guarantees the transcript is the durable source of truth first.

Low probability but a genuine ordering divergence. In scope for this checkpoint.

### M2. `recall_indicator` is emitted but silently dropped by the gateway
`run_native_turn` sends the recall indicator as
`StreamEvent::GatewayNotice { notice_kind: "memory_recall", .. }`
(`native_agent.rs:544-553`). The dispatcher's reply loop explicitly ignores
`GatewayNotice` (`dispatch.rs:431-434`), so the indicator never reaches the user.
Python renders it deterministically through `_emit_status`
(`turn_context.py:1592-1598`) precisely so it can't be dropped. Right now the
Rust user never learns memory was injected.

Whether this is acceptable depends on the presentation roadmap. If notice
rendering is deliberately deferred, say so; as written the parity claim
("tell the user, don't rely on the model") is not met. In scope for this
checkpoint (the event is produced here).

### M3. `session_end` and `flush_pending` are implemented but never driven in production
Both host methods and both Rust client wrappers exist and are exercised only by
unit tests (`extension_host.rs:955-971`). No production path calls them: the only
callers in the crate are tests (grep for `session_end` / `flush_pending` outside
`#[cfg(test)]` finds nothing). In Python, `on_session_end` runs at session
teardown (`run_agent.py:4893`) and at session-id rotation such as `/new`
(`run_agent.py:4917`, `commit_memory_session`), before `shutdown_all`. On the
Rust side only `shutdown()` (→ `shutdown_all`) ever fires, when the cached client
is dropped.

Net effect today: end-of-session and reset-time provider commits
(`on_session_end`) never happen for native Rust conversations. Queued background
writes are not lost on shutdown, because Python's `shutdown_all` drains the
single-worker executor first (`memory_manager.py:1339-1356`,
`_drain_sync_executor`, 5s), and the prior checkpoint's client-drop path does
call `shutdown`.

Checkpoint boundary: wiring `session_end`/`flush_pending` requires a
conversation-end and a reset/rotation hook in the Rust gateway. There is no such
hook yet, and minting a new session id on reset plus tearing the cached client
down is the **upcoming TTL/LRU/reset teardown checkpoint**. So the *wiring* of
`session_end` belongs to that teardown checkpoint, not this one. What belongs to
*this* checkpoint is only the observation that the plumbing added here is dormant
until then; flag it so it isn't assumed live.

---

## LOW

### L1. `interrupted` is hard-coded false; no interruption path exists yet
`turn_complete` always sends `"interrupted": false` (`native_agent.rs:571`,
`extension_host.rs:59`). Model errors are safe today because `outcome?`
(`native_agent.rs:598`) early-returns before `turn_complete`, so an errored turn
never syncs. But there is no path that can report a *completed-with-partial*
interrupted turn. Python explicitly skips sync on `interrupted`
(`run_agent.py:4969`) because partial output "is not durable conversational
truth". When streaming interruption is added, partial output would be
incorrectly synced unless this flag is wired. Deferred item; note it so it isn't
forgotten when interruption lands.

### L2. `ensure_message_schema` takes an IMMEDIATE write lock on every DB open
`session_db.rs:1150-1160` opens a `TransactionBehavior::Immediate` transaction on
every `SessionDb::open`, even when `api_content` already exists (the common case
after the first migration). This grabs the SQLite write lock briefly on every
open just to run `PRAGMA table_info`. On a DB shared with the live Python process
this adds avoidable write-lock contention. A read-only PRAGMA check first, taking
the write transaction only when the column is actually missing, would avoid it.
Minor. In scope for this checkpoint (added here).

### L3. `turn_start` 10s timeout vs 8s external prefetch is tight
`TURN_START_TIMEOUT = 10s` (`extension_host.rs:9`). `turn_start` runs
`manager.prefetch_all` synchronously in the host's single-threaded request loop,
and external prefetch blocks up to `_EXTERNAL_PREFETCH_TIMEOUT_S = 8.0` per
provider (`memory_manager.py:81,654`). One slow external provider plus JSONL
overhead can approach the 10s budget; more than one external provider can exceed
it. On timeout the Rust side fails open (drops recall, `native_agent.rs:588-590`)
while the Python worker keeps running the abandoned prefetch, which then blocks
the next request on that pipe. Turns are lease-serialized per session and the
host is per-session, so this only delays the same conversation's next request
rather than deadlocking. Fail-open is correct; the tightness is worth a comment.

---

## Verified correct (checked, not defects)

- **api_content replay is faithful.** Python persists the composed api_content on
  the current user row and replays it byte-for-byte on later turns
  (`turn_context.py:1624-1628`, `conversation_loop.py:2565-2579`), regenerating
  recall only for the current turn. Rust matches: `set_latest_user_api_content`
  stores it on the matching current row (`session_db.rs:1454-1471`) and
  `load_history` replays via `COALESCE(NULLIF(api_content,''), content)`
  (`session_db.rs:1564-1573`). No replay drift, and the clean `content` column is
  never mutated. This directly preserves prompt-cache prefix stability.
- **api_content is attached to the right row.** `begin_turn` appends the current
  user row before the agent runs (`session_db.rs:205-206`,
  `dispatch.rs:395`), and `set_latest_user_api_content` matches the newest active
  user row whose clean content equals the freshly re-encoded `model_content`
  (same `encode_message_content` on both sides). A stale/mismatched preparation
  returns `false` and leaves the row clean. The "must not attach" test at
  `session_db.rs:3371-3373` confirms it.
- **turn_number semantics match.** Python passes `agent._user_turn_count`,
  incremented before `on_turn_start` and therefore including the current message
  (`turn_context.py:852,1572`, hydrated from history at 815-820). Rust's
  `user_turn_count` counts active user rows including the just-appended current
  row (`session_db.rs:1476-1483`), with a history-based `+1` fallback when no DB
  is present (`native_agent.rs:539-543`). First turn = 1 on both.
- **Prefetch / queue-prefetch gating matches.** `on_turn_start` runs always,
  `prefetch` only when not trivial, `sync` always, `queue_prefetch` only when not
  trivial, identical to Python (`run_agent.py:4993`, `turn_context.py:1581-1585`).
  Multimodal (non-str) turns use query `""` on both sides, so both skip prefetch
  identically; this is *not* a divergence.
- **Multimodal flatten matches.** `_summarize_user_message_for_log` produces
  `"[1 image] User text"` and the fixture asserts it (`extension_host.rs:969`).
- **Empty and errored turns skip sync.** `run_native_turn` skips `turn_complete`
  when `response` is empty, and `outcome?` early-returns on model error before
  reaching sync. Matches Python's empty/interrupted guards.
- **Fail-open parity.** Host wraps `on_turn_start`, `prefetch`, `describe_recall`
  in try/except; Rust logs and continues on `turn_start`/`turn_complete` errors,
  on api_content persist failure, and on row mismatch (`native_agent.rs:557-566,
  588-590, 606-611`).
- **Migration is safe.** `ensure_message_schema` adds `api_content` only when
  absent, inside one short transaction, no external work under the write lock
  (`session_db.rs:1147-1160`). Fresh tables already declare the column
  (`session_db.rs:1331`). The legacy-table test covers it (`session_db.rs:3392`).
- **No FTS corruption.** `messages_fts` indexes `content, tool_name, tool_calls`
  only (`session_db.rs:1351-1353`); `api_content` is not indexed. The
  `messages_fts_update` trigger fires on the `api_content` UPDATE but deletes and
  re-inserts the *unchanged* `content` (`session_db.rs:1369-1373`), a net no-op.
  `ADD COLUMN` does not disturb the external-content FTS.
- **No async deadlock, no lost/duplicated events.** `run_model_turn` and the
  forwarding future run under one `tokio::join!` (`native_agent.rs:587-597`); the
  inner channel closes when `run_model_turn` drops its sender, ending the forward
  loop cleanly. Host requests are id-keyed and serialized; DB locks
  (`user_turn_count`, `set_latest_user_api_content`) are std mutexes released
  before any `.await`, never held across host I/O.
- **Task-local secret scope preserved.** Output is intercepted in the same task
  (`native_agent.rs:578-597`), matching the stated reason that task-local profile
  state stays visible to native tools.

---

## Test gaps (this checkpoint)

- No test that `turn_complete` is **skipped** on an empty response or on a model
  error. The interrupted/empty path is asserted only in Python.
- No test exercising a **tool-using** turn through `run_native_turn` to show what
  `messages` reaches the provider; the only sync test uses a hand-built 2-message
  array (`extension_host.rs:947-953`), which encodes the H1 divergence as
  expected behavior instead of catching it.
- No test for the **response-vs-reply** text divergence (Commentary / non-final
  MessageStop) from H2.
- No test that the `memory_recall` `GatewayNotice` is delivered or is
  intentionally suppressed (M2).
- No end-to-end test that any production lifecycle event drives `session_end` /
  `flush_pending` (M3). They are only unit-tested in isolation.
- `user_turn_count` parity is tested only against a hand-seeded table
  (`session_db.rs:3382`), not through the real `begin_turn` → `run_native_turn`
  dispatch path.

---

## Checkpoint boundary summary

Belongs to **this turn-lifecycle checkpoint**: H1, H2, M1, M2, L2, L3, and all
the test gaps above.

Belongs to the **upcoming TTL/LRU/reset teardown checkpoint**: the *wiring* of
`session_end` (session teardown) and reset/rotation commit + client teardown from
M3. L1 (interruption) is a separate, later streaming-interruption concern. The
plumbing for session_end/flush is present now but dormant; that is the only part
of M3 that is this checkpoint's to note.
