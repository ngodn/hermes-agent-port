# Native Manual Compression Review (AGY)

**Target Checkout:** `/home/eins0fx/development/hermes-agent-port`
**Git Branch:** `rust-rewrite` (uncommitted `/compress` checkpoint)
**Rust Sources Audited:**
- [`rust/crates/hermes-gateway/src/session_commands.rs`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/session_commands.rs)
- [`rust/crates/hermes-gateway/src/session_db.rs`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/session_db.rs)
- [`rust/crates/hermes-gateway/src/session_store.rs`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/session_store.rs)
- [`rust/crates/hermes-gateway/src/session_entry.rs`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/session_entry.rs)
- [`rust/crates/hermes-gateway/src/partial_compress.rs`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/partial_compress.rs)
- [`rust/crates/hermes-gateway/src/compression_prompt.rs`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/compression_prompt.rs)
- [`rust/crates/hermes-gateway/src/conversation_agent.rs`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/conversation_agent.rs)
- [`rust/crates/hermes-gateway/src/native_agent.rs`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/native_agent.rs)
- [`rust/crates/hermes-gateway/src/agent.rs`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/agent.rs)
- [`rust/crates/hermes-gateway/src/dispatch.rs`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/dispatch.rs)
- [`rust/crates/hermes-gateway/src/message.rs`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/message.rs)
- [`rust/crates/hermes-gateway/src/prompt_cache.rs`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/prompt_cache.rs)

**Python Reference Baseline:**
- [`gateway/slash_commands.py`](file:///home/eins0fx/development/hermes-agent-port/gateway/slash_commands.py#L4564-L4950)
- [`agent/context_compressor.py`](file:///home/eins0fx/development/hermes-agent-port/agent/context_compressor.py)
- [`agent/conversation_compression.py`](file:///home/eins0fx/development/hermes-agent-port/agent/conversation_compression.py#L5307-L5353)
- [`agent/prompt_cache_scope.py`](file:///home/eins0fx/development/hermes-agent-port/agent/prompt_cache_scope.py)
- [`hermes_cli/partial_compress.py`](file:///home/eins0fx/development/hermes-agent-port/hermes_cli/partial_compress.py)
- [`hermes_state.py`](file:///home/eins0fx/development/hermes-agent-port/hermes_state.py#L9141-L9250)

---

## Executive Summary

The transaction core and rotation publication in [`publish_gateway_compression`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/session_db.rs#L1215-L1375) are structurally solid:
- Executed in a single SQLite `IMMEDIATE` transaction under WAL mode with a 5-second busy timeout.
- The child session and compacted transcript rows are inserted prior to closing the parent.
- Parent closure uses a compare-and-swap (`UPDATE sessions SET ended_at = ?, end_reason = 'compression' WHERE id = ? AND ended_at IS NULL`), ensuring exactly one winner in concurrent publish attempts.
- Route repointing verifies head integrity against concurrent modifications.
- Role alternation at the seam is preserved by validating that partial tails begin on a `user` message.

However, eight concrete correctness defects and unsupported parity claims were identified across transcript durability, secret sanitization, prompt-cache scoping, title persistence, cross-process concurrency, and failure handling.

---

## Findings

### Finding 1: Transcript Data Loss & Tool Call Erasure in Summarization Input
- **Severity:** High
- **Category:** Transcript Data Loss / Context Degradation
- **Exact References:**
  - [`rust/crates/hermes-gateway/src/session_db.rs:2184-2199`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/session_db.rs#L2184-L2199) (`load_compression_snapshot`)
  - [`rust/crates/hermes-gateway/src/compression_prompt.rs:11-20`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/compression_prompt.rs#L11-L20) (`format_history_turns`)
- **Mechanics & Failure Mode:**
  `load_compression_snapshot` executes:
  ```sql
  SELECT id, role, content, api_content FROM messages
  WHERE session_id = ? AND active = 1 ORDER BY id ASC
  ```
  Unlike `load_lifecycle_messages` ([`session_db.rs:2213-2222`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/session_db.rs#L2213-L2222)), it does not select `tool_calls`, `tool_call_id`, or `tool_name`.
  Then, `compression_prompt::format_history_turns` serializes history entries purely as:
  ```rust
  let body = message.api_content.as_deref().unwrap_or(&message.content);
  formatted.push_str(&format!("{}: {}\n\n", message.role, body));
  ```
  In agent transcripts, assistant turns that invoke tools (executing shell commands, editing/reading files, web scraping) routinely have empty string text (`content: ""`) with all operational payload residing in `tool_calls`.
  Because tool calls are completely omitted:
  1. The summarizer receives blank assistant turns (`"assistant: \n\n"`).
  2. The summarizer receives tool response turns (`"tool: { ... }\n\n"`) with no indication of what tool was executed, what parameters were supplied, or which assistant action prompted it.
  3. Crucial state-such as files inspected, commands executed, codebase facts discovered, and active user instructions handled via tools-is erased from the summarization prompt. The resulting summary cannot preserve operational context it was never given.
- **Proposed Fix:**
  Expand `load_compression_snapshot` and `HistoryMessage` (or introduce a dedicated snapshot struct) to query and retain `tool_calls`, `tool_call_id`, and `tool_name`. Update `format_history_turns` in [`compression_prompt.rs`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/compression_prompt.rs) to format tool calls and their results into the transcript prompt, matching Python's serialization in [`agent/context_compressor.py`](file:///home/eins0fx/development/hermes-agent-port/agent/context_compressor.py).

---

### Finding 2: Secret Exposure & Missing Strict Redaction Boundary
- **Severity:** High
- **Category:** Secret Exposure / Security Boundary
- **Exact References:**
  - [`rust/crates/hermes-gateway/src/native_agent.rs:691-706`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/native_agent.rs#L691-L706) (`summarize_context`)
  - [`rust/crates/hermes-gateway/src/session_commands.rs:201-212`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/session_commands.rs#L201-L212) (`compacted` history message construction)
  - [`rust/crates/hermes-gateway/src/compression_prompt.rs:11-37`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/compression_prompt.rs#L11-L37) (`build_compression_prompt`)
  - [`rust/crates/hermes-gateway/src/session_commands.rs:182`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/session_commands.rs#L182) (raw provider error logging)
- **Mechanics & Failure Mode:**
  In Python, context compression enforces programmatic defense-in-depth sanitization at two non-negotiable boundaries ([`agent/context_compressor.py:1405-1424`](file:///home/eins0fx/development/hermes-agent-port/agent/context_compressor.py#L1405-L1424)):
  1. Input turns are scrubbed of secrets before prompt assembly.
  2. The LLM-generated summary is strictly filtered via `redact_sensitive_text(summary, force=True, redact_url_credentials=True)` before being stored into SQLite or returned.

  The Rust implementation violates this security contract in three places:
  1. **Unredacted input:** [`compression_prompt.rs`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/compression_prompt.rs#L11-L37) concatenates raw message content into the prompt without programmatic secret redaction. It relies exclusively on model compliance with a system instruction (`"substitute [REDACTED]"`).
  2. **Unredacted output persistence:** When the model outputs a summary, [`native_agent.rs:706`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/native_agent.rs#L706) wraps it and [`session_commands.rs:201-212`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/session_commands.rs#L201-L212) writes it directly to SQLite as the root `user` message of the child session without running any redaction filter. If the model echoes API keys, bearer tokens, or database URLs, those secrets are committed unredacted into durable storage and replayed into all future context windows.
  3. **Unredacted provider error log:** [`session_commands.rs:182`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/session_commands.rs#L182) emits `tracing::warn!(%error, session = %session_id, "manual compression summary failed")`. Provider errors can echo request headers or payloads containing authentication credentials. Python explicitly redacts this error before logging ([`gateway/slash_commands.py:4913-4915`](file:///home/eins0fx/development/hermes-agent-port/gateway/slash_commands.py#L4913-L4915)).
- **Proposed Fix:**
  - In [`session_commands.rs`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/session_commands.rs), pass the generated `summary` through the gateway secret redaction pipeline with strict/force mode enabled prior to inserting into `compacted`.
  - Pass `error` through the secret redaction helper before logging in `session_commands.rs:182`.
  - Scrub candidate history messages through the redaction helper before formatting in `compression_prompt.rs`.

---

### Finding 3: Prompt-Cache Invalidation and Lineage Root Loss on Post-Compression Turn
- **Severity:** Medium
- **Category:** Prompt-Cache Invalidation / Parity Claims
- **Exact References:**
  - [`rust/crates/hermes-gateway/src/native_agent.rs:580`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/native_agent.rs#L580) (`run_native_turn`)
  - [`rust/crates/hermes-gateway/src/prompt_cache.rs:94-104`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/prompt_cache.rs#L94-L104) (`inject_prompt_cache_key`)
  - [`rust/crates/hermes-gateway/src/agent.rs:139-145`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/agent.rs#L139-L145) (`release_conversation` trait definition)
- **Mechanics & Failure Mode:**
  When manual compression rotates session $S$ into child session $C$, the active route is repointed to $C$. On the subsequent user turn:
  1. [`native_agent.rs:580`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/native_agent.rs#L580) sets `turn_client.cache_scope = Some(session_id.clone())`, where `session_id` is the newly minted child ID $C$.
  2. [`prompt_cache.rs:96-103`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/prompt_cache.rs#L96-L103) uses `scope_src = Some(C)` and hashes it into `prompt_cache_key`.
  3. In Python ([`agent/prompt_cache_scope.py:1-20, 246-300`](file:///home/eins0fx/development/hermes-agent-port/agent/prompt_cache_scope.py#L1-L20)), `resolve_prompt_cache_scope()` walks the compression lineage back to the root session ID (or derives from `gateway_session_key`). This guarantees that prompt cache keys remain identical across rotations so the provider cache (e.g. Anthropic, OpenAI, OpenRouter sticky session routing) is preserved.
  4. In Rust, because the physical child ID $C$ is used directly as `cache_scope`, rotating physical session IDs immediately causes a 100% prompt cache miss on the turn following compression.
  5. Furthermore, the docstring in [`agent.rs:139-141`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/agent.rs#L139-L141) claims that `release_conversation` is used so that "the next turn rebuilds the allowed prompt-cache prefix". `NativeAgent` does not implement `release_conversation` at all (it defaults to `false`), and even if implemented, cache scope would still be broken due to using child session $C$.
- **Proposed Fix:**
  Port compression lineage resolution from `agent/prompt_cache_scope.py` into Rust. When `native_agent.rs:580` establishes `turn_client.cache_scope`, resolve the logical lineage root via SQLite `parent_session_id` traversal (or hash the stable `gateway_session_key`), ensuring the cache key survives session rotation.

---

### Finding 4: Title & Title-Source Provenance Lost on Child Session Insertion
- **Severity:** Medium
- **Category:** Parity Claims / Transcript Metadata Durability
- **Exact References:**
  - [`rust/crates/hermes-gateway/src/session_db.rs:1270-1283`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/session_db.rs#L1270-L1283) (`publish_gateway_compression`)
  - [`rust/crates/hermes-gateway/src/session_db.rs:1984-2010`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/session_db.rs#L1984-L2010) (`ensure_title_index`)
  - [`agent/conversation_compression.py:5307-5353`](file:///home/eins0fx/development/hermes-agent-port/agent/conversation_compression.py#L5307-L5353) (Python title transfer contract)
- **Mechanics & Failure Mode:**
  In [`session_db.rs:1270-1283`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/session_db.rs#L1270-L1283), `publish_gateway_compression` clones parent attributes into the child session:
  ```sql
  INSERT INTO sessions (
      id, source, user_id, session_key, chat_id, chat_type, thread_id,
      model, model_config, system_prompt, system_prompt_hash,
      parent_session_id, cwd, profile_name, git_repo_root, git_branch,
      origin_json, display_name, started_at, message_count,
      last_activity_at, tool_names, hidden
  )
  SELECT ?1, source, user_id, session_key, chat_id, chat_type, thread_id,
         model, model_config, system_prompt, system_prompt_hash,
         id, cwd, profile_name, git_repo_root, git_branch,
         origin_json, display_name, ?2, 0, ?2, tool_names, 0
  FROM sessions WHERE id = ?3 AND ended_at IS NULL
  ```
  Neither `title` nor `title_source` is copied into the child session; both become `NULL`.
  In Python ([`agent/conversation_compression.py:5307-5353`](file:///home/eins0fx/development/hermes-agent-port/agent/conversation_compression.py#L5307-L5353)), the title and its provenance are explicitly transferred to the child session, while being cleared from the parent to satisfy the unique constraint `idx_sessions_title_unique` ([`session_db.rs:1986-1987`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/session_db.rs#L1986-L1987)).

  In Rust:
  1. The newly compressed active session has no title. Commands such as `/sessions` (which filter unnamed sessions unless `include_unnamed` is specified) drop the active session.
  2. The `/title` command on the active thread reports `"No title set"`.
  3. The parent session (now hidden and ended) retains the title, preventing the child from ever taking that name unless manually reassigned.
- **Proposed Fix:**
  Inside the atomic transaction in `publish_gateway_compression`:
  1. Query `title` and `title_source` of the parent session.
  2. If present, clear `title` and `title_source` on the parent session row (`UPDATE sessions SET title = NULL, title_source = NULL WHERE id = ?`).
  3. Set `title` and `title_source` on the newly inserted child session row.

---

### Finding 5: Cross-Process Mutual Exclusion Bypassed (Missing DB Compression Lock)
- **Severity:** Medium
- **Category:** Cross-Process Races / Concurrency
- **Exact References:**
  - [`rust/crates/hermes-gateway/src/session_commands.rs:68-110`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/session_commands.rs#L68-L110) (`compress_session` lease acquisition)
  - [`rust/crates/hermes-gateway/src/session_commands.rs:114-189`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/session_commands.rs#L114-L189) (asynchronous summarization call)
  - [`gateway/slash_commands.py:4812-4817`](file:///home/eins0fx/development/hermes-agent-port/gateway/slash_commands.py#L4812-L4817) (Python DB compression lock check)
  - [`hermes_state.py:9141-9250`](file:///home/eins0fx/development/hermes-agent-port/hermes_state.py#L9141-L9250) (`compression_locks` table schema and acquisition)
- **Mechanics & Failure Mode:**
  `compress_session` acquires in-process `Arc<SessionTurnLeaseRegistry>` leases (`route_leases` and `transcript_leases`). These leases coordinate only between tasks within the current gateway process.
  Hermes is designed to operate concurrently with external processes sharing the SQLite state file (such as the Hermes CLI or external agents).
  In Python, cross-process mutual exclusion is enforced by acquiring a lock row in the `compression_locks` table during the slow LLM summarization window.

  In Rust:
  1. No SQLite lock row is acquired in `compression_locks`.
  2. If an external process (CLI or another gateway) executes a turn on session $S$ while Rust is waiting for the LLM summary (a 10-30+ second window), the external process is not blocked.
  3. When `publish_gateway_compression` runs, it only clones rows where `id > snapshot.watermark`. If the external turn commits *after* `publish_gateway_compression` finishes and closes $S$, its newly appended messages are written to the closed parent $S$ and never transferred to child $C$.
  4. The active conversation route now points to $C$, causing the user's turn in the other process to be silently dropped from active visibility.
- **Proposed Fix:**
  Port the SQLite `compression_locks` acquisition and release into [`session_db.rs`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/session_db.rs) and acquire it in [`session_commands.rs`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/session_commands.rs) around the summarization phase.

---

### Finding 6: Flawed Anti-Growth ("Shrinkage") Guard Rejecting Valid Compactions
- **Severity:** Low
- **Category:** Failure Behavior / Heuristic Flaw
- **Exact References:**
  - [`rust/crates/hermes-gateway/src/session_commands.rs:190-200`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/session_commands.rs#L190-L200) (`source_chars` vs `summary.chars().count()`)
  - [`rust/crates/hermes-gateway/src/native_agent.rs:706`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/native_agent.rs#L706) (`compression_prompt::wrap`)
  - [`rust/crates/hermes-gateway/src/compression_prompt.rs:39-44`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/compression_prompt.rs#L39-L44) (`wrap` boilerplate constants)
- **Mechanics & Failure Mode:**
  [`session_commands.rs:190-200`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/session_commands.rs#L190-L200) computes:
  ```rust
  let source_chars = head.iter().map(|item| item.content.chars().count()).sum::<usize>();
  if summary.chars().count() >= source_chars {
      return Ok(CompressResult {
          reply: "⚠️ Compression refused because the generated checkpoint would not shrink the selected history. The conversation was not changed.".into(),
      });
  }
  ```
  The variable `summary` has already been formatted by [`compression_prompt::wrap(summary)`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/compression_prompt.rs#L39-L44), which adds `SUMMARY_PREFIX` and `SUMMARY_END` (~530 characters of static Markdown instructions and XML wrappers).
  The guard compares:
  $$\text{summary\_body\_chars} + 530 \ge \text{source\_chars}$$
  If the head history contains concise turns (e.g. totaling 400 characters of user/assistant text), the comparison compares ~700 characters (including boilerplate) against 400 characters of source text, and incorrectly aborts compression.
  In Python ([`gateway/slash_commands.py:4779-4794`](file:///home/eins0fx/development/hermes-agent-port/gateway/slash_commands.py#L4779-L4794)), `estimate_request_tokens_rough` accounts for system prompt and tools on both sides symmetrically, canceling out fixed overhead.
- **Proposed Fix:**
  Perform the shrinkage check on the raw generated summary before calling `wrap`, or strip the boilerplate before comparing character lengths. Alternatively, adopt token-based estimation that includes fixed overhead symmetrically.

---

### Finding 7: Misleading "Preview" Failure Messaging on Live Executions
- **Severity:** Low
- **Category:** Failure Behavior / Diagnostics
- **Exact References:**
  - [`rust/crates/hermes-gateway/src/session_commands.rs:268`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/session_commands.rs#L268) (`bail!("session route kept changing during compression preview")`)
  - [`rust/crates/hermes-gateway/src/dispatch.rs:418-422`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/dispatch.rs#L418-L422) (`push compression preview failed`)
  - [`rust/crates/hermes-gateway/src/message.rs:248-254`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/message.rs#L248-L254) (`HTTP compression preview failed`)
- **Mechanics & Failure Mode:**
  If route CAS retries are exhausted or an unhandled error occurs during a live `/compress` invocation (without `--preview`), the error message and logging state:
  - `dispatch.rs`: logs `"push compression preview failed"` and replies `"Compression preview failed. Please try again."`
  - `message.rs`: logs `"HTTP compression preview failed"` and replies `{"error":"compression preview failed"}`.
  - `session_commands.rs:268`: errors with `"session route kept changing during compression preview"`.

  Users and operators investigating live compression failures are misled into believing a preview simulation failed, obscuring actual rotation aborts.
- **Proposed Fix:**
  Make the error messages and log entries condition on `args.preview`: emit `"compression failed"` when performing live rotation and `"compression preview failed"` when `--preview` is passed.

---

### Finding 8: `partial_boundary` Algorithm Divergence on Short Transcripts
- **Severity:** Low
- **Category:** Parity Claims / Boundary Logic
- **Exact References:**
  - [`rust/crates/hermes-gateway/src/partial_compress.rs:83-94`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/partial_compress.rs#L83-L94) (`partial_boundary`)
  - [`hermes_cli/partial_compress.py:213-266`](file:///home/eins0fx/development/hermes-agent-port/hermes_cli/partial_compress.py#L213-L266) (Python reference)
- **Mechanics & Failure Mode:**
  In [`partial_compress.rs:83-94`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/partial_compress.rs#L83-L94), if the backward search over history discovers fewer user turns than `keep_last`, the function returns `None`, forcing a fallback to full compression.
  In Python, if fewer user turns than `keep_last` exist, the algorithm retains all available user turns up to that point as the tail as long as the head remains non-empty.
- **Proposed Fix:**
  Align `partial_boundary` with Python by selecting the earliest available user turn index when user turns $< \text{keep\_last}$, only returning `None` if the resulting head would be empty.

---

## Verified Invariants (Cleared / Non-Issues)

1. **Transaction Atomicity & Child-First Rollback:**
   [`session_db.rs:1215-1375`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/session_db.rs#L1215-L1375) executes within a single `rusqlite::TransactionBehavior::Immediate`. Child session and compacted rows are committed before parent closing. Rollback behavior on conflict is properly covered by integration tests.
2. **Stale Route Publication Protection:**
   Route publication verifies CAS conditions against durable routing (`WHERE scope = ? AND session_key = ?`) and memory caches (`session_store.rs`), repointing only if the instance and parent session remain unchanged.
3. **Message Role Alternation:**
   The compacted preamble consists of `[user: summary, assistant: ack]`. [`session_db.rs:1240-1267`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/session_db.rs#L1240-L1267) validates that cloned tail rows begin with a `user` turn and alternate consistently, preventing adjacent identical roles.
4. **Checkpoint Gate Integrity:**
   The gate inspecting `user_config["compression"]["checkpoint_required"]` ([`dispatch.rs:410-412`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/dispatch.rs#L410-L412), [`message.rs:243-245`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/message.rs#L243-L245)) accurately refuses execution when checkpointing is unavailable rather than proceeding without safety checkpoints.
