# External-memory lifecycle review resolution

This records the disposition of the Gemini and Claude implementation audits.
The reviews were run against an earlier uncommitted revision, so their line
numbers and several findings predate the fixes below.

## Fixed in this checkpoint

- Tool-aware providers now receive the actual current-turn model transcript,
  including assistant tool calls and tool results. A small model wrapper
  captures the final request prefix without changing the tool loop. The clean
  user content and exact `api_content` sidecar remain distinct, and the durable
  assistant reply is appended only after the gateway writes it.
- External-memory completion moved behind `end_turn` through the agent
  `finalize_turn_after_persist` hook. Both HTTP and push dispatch paths call it,
  and a dispatcher test verifies the assistant row is visible first. The real
  Python-provider fixture independently opens SQLite during `sync_turn` and
  observes the durable assistant row.
- The finalizer receives the gateway's assembled reply, not a private
  MessageChunk-only reconstruction. This also covers future Commentary or
  segmented-message support without transcript drift.
- `HistoryMessage` now retains clean `content` and optional `api_content` as
  separate fields. Model request construction carries the sidecar until the
  existing wire projection substitutes it. Non-model history consumers keep a
  clean transcript.
- Sidecar attachment now checks the newest active user row first, then requires
  its clean content to match. It cannot search backward and modify an older
  identical message when the current row differs.
- The typed completion RPC accepts the interruption bit. Model errors and empty
  replies still skip pending memory completion entirely.
- Turn-start IPC now allows 30 seconds around Python's bounded 8-second external
  prefetch. A truly wedged lifecycle hook still terminates the compatibility
  child by design; mid-conversation host reconstruction remains explicit future
  work.
- The protocol fixture exercises session-end IPC with a 15-second budget. The
  Rust production method is intentionally withheld until the eviction
  checkpoint supplies its first real caller.
- Existing databases inspect the message schema read-only. They acquire an
  immediate write transaction only when the `api_content` migration is needed,
  then recheck the column under that transaction.

## Rejected findings

- Whitespace-only sync was left unchanged. The live Python reference tests
  truthiness without trimming, so non-empty whitespace is intentionally truthy
  on this compatibility path. Changing only Rust would introduce a divergence.
- Python imports inside the persistent host's turn methods use the interpreter's
  module cache. Moving them to module import time would weaken the current
  profile-environment-before-import boundary for no practical hot-path gain.
- The history-window fallback affects database-less direct callers only. Live
  gateway turns use `SessionDb::user_turn_count`, including the current row, so
  long production conversations do not drift.

## Explicit follow-on work

- TTL, LRU, pressure, reset, expiry and shutdown eviction must drive
  `session_end`, queue drain and child teardown. This is the next checkpoint,
  and it will add the typed Rust methods alongside those production callers.
- The `memory_recall` notice is emitted as structured transport state, but the
  current Rust dispatcher has no status-update delivery lane and drops all
  gateway notices. Rendering lifecycle notices belongs with that presentation
  lane rather than inserting them into assistant transcript text.
- A genuinely hung compatibility host is not reconstructed in place. Cache
  eviction will bound its lifetime; transparent host respawn remains separate.
- Native interruption producers, compression checkpoints and built-in memory
  tool write mirroring remain later agent-core milestones.
