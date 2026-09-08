# Native manual compression review (Claude)

Scope: the uncommitted `/compress` checkpoint on `rust-rewrite`. Rust under
review: `session_commands.rs`, `session_db.rs`, `session_store.rs`,
`session_entry.rs`, `partial_compress.rs`, `compression_prompt.rs`,
`conversation_agent.rs`, `native_agent.rs`, `agent.rs`, `dispatch.rs`,
`message.rs`, `slash.rs`. Compared against the Python contract in
`gateway/slash_commands.py::_handle_compress_command_inner`,
`hermes_cli/partial_compress.py`, and `agent/context_compressor.py`.

The rotation core is solid. `publish_gateway_compression`
(`session_db.rs:1198`) is a single `IMMEDIATE` transaction that inserts the
child transcript, clones any concurrently appended tail rows, upserts the
route, and closes the parent last, all under WAL with a 5s busy timeout
(`session_db.rs:1839`). Child-first ordering, the parent-close CAS
(`UPDATE ... WHERE id = ? AND ended_at IS NULL`), the rollback test, and the
exactly-one-winner test all hold up. The route repoint is CAS-guarded against a
changed head (`session_store.rs:publish_compression`, `same_instance` +
`session_id` check), so I found no transaction-atomicity or stale-route
defect, and no in-process transcript loss (the head stays intact in the ended
parent; only rows above the snapshot watermark are cloned forward).

The findings below are the concrete issues that remain.

---

## 1. Cross-process compression has no lock, so a concurrent turn in another process can be orphaned (Medium)

**Where:** `session_commands.rs:93-108` (in-process `transcript_leases.acquire`),
`dispatch.rs:280-281` / `message.rs:114`, `conversation_agent.rs:36-37` /
`dispatch.rs:93-94` (the lease registries are per-instance `Arc`s).

`compress_session` serializes against a live turn only through the in-process
`SessionTurnLeaseRegistry`. That registry is created per gateway/AppState
instance, so it does not coordinate across processes that share the same
SQLite file. Hermes runs exactly this configuration: the `hermes` CLI and the
gateway open the same state DB. Python guards this with a DB-backed
compression lock and the live turn checking it
(`_compression_skipped_due_to_lock` / `describe_compression_lock_skip` in
`gateway/slash_commands.py:4812-4817`); that machinery has no Rust equivalent
here.

**Failure scenario:** CLI process runs a turn on session S (history already
loaded, model call in flight). Gateway `/compress S` acquires its in-process
transcript lease (does not block the CLI), snapshots at watermark W,
summarizes, then `publish_gateway_compression` commits: it clones rows with
`id > W`, closes S, and repoints the route to child C. The CLI turn finishes
after that commit and appends its user+assistant rows to S (the now-ended
parent). Those rows are never in C. The next gateway turn loads C and the CLI
exchange is gone from the active thread (still findable in the ended parent,
but dropped from the conversation the user sees).

**Proposed fix:** port the DB-level compression lock. Acquire it inside the
same connection/transaction scope used by `publish_gateway_compression`, and
have the normal turn-append path refuse or wait when the lock is held for that
`session_key`. Until that exists, document the limitation as
single-gateway-process-only and have the live-turn append path detect a parent
whose `ended_at` was set with `end_reason='compression'` and re-anchor onto the
child route instead of writing to the dead parent.

---

## 2. The anti-growth guard measures the wrapped summary against raw head chars, so short conversations are always refused (Low)

**Where:** `session_commands.rs:190-199`.

```rust
let source_chars = head.iter().map(|i| i.content.chars().count()).sum::<usize>();
if summary.chars().count() >= source_chars { /* refuse */ }
```

`summary` here is the value returned by `summarize_context`, which is already
wrapped (`native_agent.rs:706` returns `compression_prompt::wrap(...)`, and the
`ConversationAgent` path forwards that wrapped value). `wrap`
(`compression_prompt.rs:39`) prepends `SUMMARY_PREFIX` and appends
`SUMMARY_END`, roughly 530 characters of fixed boilerplate. So the guard
compares (boilerplate + body) against raw source content only. Any selected
head whose total content is under ~530 chars is refused even when the
compaction would genuinely reduce request pressure. Python instead compares
token estimates that include the system prompt and tools on both sides
(`estimate_request_tokens_rough(... system_prompt=..., tools=...)` at
`gateway/slash_commands.py:4779-4794,4892`), so the fixed overhead cancels.

This is a conservative bias (it refuses rather than losing data), so it is
Low severity, but it makes `/compress` a no-op on small-but-eligible threads
that pass the `history.len() < 4` gate.

**Proposed fix:** compare against the body only (strip the wrap before the
check, or compute `wrap` after the guard passes), or move to a token-based
estimate that counts the same fixed overhead on both sides as Python does.

---

## 3. Real (non-preview) compression failures report themselves as "preview" failures (Low)

**Where:** `session_commands.rs:268` (`bail!("session route kept changing during
compression preview")`), `dispatch.rs:419-420`
(`"push compression preview failed"` / `"Compression preview failed. Please try
again."`), `message.rs:249-252` (`"HTTP compression preview failed"` /
`"compression preview failed"`).

These messages fire on the live rotation path too, not just `--preview`. If the
32-iteration route-stability loop is exhausted during an actual `/compress`, or
the handler returns `Err`, the user is told a "preview" failed for what was a
real compaction attempt. Misleading, not a data issue.

**Proposed fix:** make the wording mode-aware, or use neutral text
("compression could not complete, the conversation was not changed").

---

## 4. Provider error is logged unredacted on summary failure (Low)

**Where:** `session_commands.rs:182` (`tracing::warn!(%error, session = %session_id,
"manual compression summary failed")`).

The user-facing message is correctly generic (`session_commands.rs:186-188`),
so nothing sensitive reaches the chat. But the provider error is logged raw.
Python force-redacts summary/provider error text before it leaves the boundary
(`redact_sensitive_text(_summary_err, force=True)`,
`gateway/slash_commands.py:4913-4915`). Depending on the backend, a provider
error can echo request-derived content. Risk is low (auth lives in headers, not
the error body in the common path), but the redaction parity is worth keeping.

**Proposed fix:** run the error through the redaction helper before logging, or
log a stable error code plus the redacted detail.

---

## 5. `partial_boundary` diverges from Python when fewer user turns exist than `keep_last` (Low, edge)

**Where:** `partial_compress.rs:83-94` vs
`hermes_cli/partial_compress.py:213-266`.

Rust returns `None` (fall back to full compression) whenever it never reaches
`keep_last` user messages. Python instead keeps a verbatim tail starting at the
earliest available user turn as long as the head is non-empty. This only
differs when a non-user message precedes the first/only user turn(s) and there
are fewer user turns than `keep_last`. Normal transcripts start on a user turn,
where both paths agree (head empty at index 0 -> full compression), so this is
an edge case, but the kept-verbatim behavior is not identical to the ported
contract.

**Proposed fix:** match Python: collect up to `keep_last` user starts, take the
earliest collected index as the boundary, and only fall back to full when the
resulting head is empty.

---

## Notes checked and cleared

- **Transaction atomicity / rollback:** one `IMMEDIATE` tx, child rows and
  route and parent-close all inside it; rollback verified
  (`session_db.rs` `compression_publish_is_child_first_atomic...`).
- **Concurrent double-publish:** parent-close CAS yields exactly one winner
  (`concurrent_compression_publications_have_exactly_one_winner`), enabled by
  WAL + 5s busy timeout.
- **Transcript durability under in-process append:** head is preserved in the
  ended parent; rows above the watermark (partial: `id >= tail_start_id`, full:
  `id > watermark`) are cloned into the child, so nothing in-process is lost.
- **Message-role ordering:** child is `[summary(user), ack(assistant)]` and the
  partial tail always begins on a `user` row (`partial_boundary` snaps to a
  user turn), so `assistant -> user` alternation at the seam holds. Verified by
  the HTTP continuation test in `message.rs`.
- **Prompt-cache invalidation:** `compression_candidate` mints a new
  `session_id` and resets `last_prompt_tokens` to 0 (`session_entry.rs`), and
  the frozen system prompt is carried onto the child via the `INSERT ... SELECT`,
  so the next turn rebuilds the cache prefix correctly.
- **Secret handling in the summary prompt:** `compression_prompt.rs` marks the
  transcript as data and instructs `[REDACTED]` substitution, matching the
  Python compressor prompt; the request drops `max_tokens` and streaming and
  sends no tools (`native_agent.rs:864-889`), verified by
  `compression_uses_one_non_streaming_tool_free_native_request`.
- **`checkpoint_required` gate:** reads `user_config["compression"]
  ["checkpoint_required"]` (`dispatch.rs:410`, `message.rs:243`), which is the
  same merged user config Python reads via `load_config()`; the gate honestly
  refuses instead of faking a checkpoint, and says so.
