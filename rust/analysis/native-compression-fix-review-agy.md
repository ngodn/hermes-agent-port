# Native Manual Compression Fix Review (AGY)

**Target Checkout:** `/home/eins0fx/development/hermes-agent-port`
**Git Branch:** `rust-rewrite` (uncommitted checkpoint after reviews)
**Rust Sources Audited:**
- [`rust/crates/hermes-gateway/src/session_commands.rs`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/session_commands.rs)
- [`rust/crates/hermes-gateway/src/session_db.rs`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/session_db.rs)
- [`rust/crates/hermes-gateway/src/session_admission.rs`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/session_admission.rs)
- [`rust/crates/hermes-gateway/src/session_store.rs`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/session_store.rs)
- [`rust/crates/hermes-gateway/src/session_entry.rs`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/session_entry.rs)
- [`rust/crates/hermes-gateway/src/durable_turn_lease.rs`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/durable_turn_lease.rs)
- [`rust/crates/hermes-gateway/src/partial_compress.rs`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/partial_compress.rs)
- [`rust/crates/hermes-gateway/src/compression_prompt.rs`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/compression_prompt.rs)
- [`rust/crates/hermes-gateway/src/compression_redact.rs`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/compression_redact.rs)
- [`rust/crates/hermes-gateway/src/conversation_agent.rs`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/conversation_agent.rs)
- [`rust/crates/hermes-gateway/src/native_agent.rs`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/native_agent.rs)
- [`rust/crates/hermes-gateway/src/dispatch.rs`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/dispatch.rs)
- [`rust/crates/hermes-gateway/src/message.rs`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/message.rs)
- [`rust/crates/hermes-gateway/src/turn_lease.rs`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/turn_lease.rs)

---

## Executive Summary

The fixes applied in response to `native-compression-review-agy.md` and `native-compression-review-claude.md` successfully address several previous defects:
- Programmatic secret scrubbing is enforced across prompt inputs, tool calls, LLM summaries, and provider error logs via `compression_redact`.
- Title and title provenance transfer to the newly inserted child session while nulling the parent to respect `idx_sessions_title_unique`.
- Rotation-stable prompt cache scoping is achieved by traversing the lineage root in `native_agent` and evicting conversation clients with `RetirementKind::Release`.
- `partial_compress::partial_boundary` now retains available user turns when fewer than `keep_last` exist, matching Python parity.
- Neutral error wording replaces misleading "preview" failure text.

However, five concrete correctness defects and operational regressions were identified in the new implementation, including tool-role rejection in tail publication, lost turns during LLM summarization, anti-growth accounting undercounting, async drop races in lease acquisition, and table name divergence with the Python CLI.

---

## Findings

### Finding 1: Tail Role Alternation Check Rejects Any Transcript Containing Tool Invocations
- **Severity:** High
- **Category:** Invalid Role Ordering / Transcript Degradation / Feature Failure
- **Exact References:**
  - [`rust/crates/hermes-gateway/src/session_db.rs:1472-1488`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/session_db.rs#L1472-L1488) (`publish_gateway_compression` role validation loop)
  - [`rust/crates/hermes-gateway/src/session_commands.rs:277-283`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/session_commands.rs#L277-L283) (`published` None handler)
  - [`rust/crates/hermes-gateway/src/partial_compress.rs:83-96`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/partial_compress.rs#L83-L96) (`partial_boundary`)
- **Mechanics & Failure Mode:**
  In [`session_db.rs:1472-1488`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/session_db.rs#L1472-L1488), `publish_gateway_compression` validates the sequence of message roles for rows to be cloned into the child session (`clone_roles`):
  ```rust
  let mut expect_user = true;
  for role in &clone_roles {
      let valid = if expect_user {
          role == "user"
      } else {
          role == "assistant"
      };
      if !valid {
          return Ok(false);
      }
      expect_user = !expect_user;
  }
  if !expect_user {
      return Ok(false);
  }
  ```
  This validation loop assumes transcripts consist solely of strictly alternating `user` and `assistant` messages.
  However, in Hermes, any agent turn that invokes tools records assistant turns with `tool_calls` followed by one or more `role = "tool"` messages, followed by the assistant response.
  When partial compression (`/compress here <N>`) runs on a conversation where the kept tail contains any tool execution, `clone_roles` contains `["user", "assistant", "tool", ...]`.
  When `role == "tool"`, `valid` evaluates to `false` (since `"tool"` is neither `"user"` nor `"assistant"`).
  `publish_gateway_compression` immediately aborts and returns `Ok(false)`.
  In `session_commands.rs:277-283`, this causes `/compress` to abort with:
  `"⚠️ The conversation changed while its summary was being prepared, so this compression result was not applied. Inspect the current session and retry if it still needs compression."`
  Similarly, if a full compression is running and a concurrent turn arrives during summarization that uses a tool, `id > watermark` contains tool messages, and `publish_gateway_compression` aborts.
  Consequently, manual partial compression is broken for any conversation that actually uses tools in the retained tail.
- **Proposed Fix:**
  Update the validation loop to accommodate tool interaction semantics: after an `assistant` turn, allow zero or more `tool` messages followed by an `assistant` message, while ensuring that the cloned sequence begins with a `user` turn and ends on a completed `assistant` turn (with no dangling unfulfilled tool calls).

---

### Finding 2: Inbound Platform Messages Dropped During Summarization Due to Hardcoded 5-Second Lease Timeout
- **Severity:** High
- **Category:** Lost Turns / Turn Admission Timeout
- **Exact References:**
  - [`rust/crates/hermes-gateway/src/session_admission.rs:81-89`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/session_admission.rs#L81-L89) (`durable_turn_lease::acquire` in `admit_turn`)
  - [`rust/crates/hermes-gateway/src/dispatch.rs:509-531`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/dispatch.rs#L509-L531) (`admit_turn` error handling on push ingress)
  - [`rust/crates/hermes-gateway/src/session_commands.rs:126-147`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/session_commands.rs#L126-L147) (`compress_session` durable lease acquisition)
  - [`rust/crates/hermes-gateway/src/turn_lease.rs:36`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/turn_lease.rs#L36) (`DEFAULT_LEASE_WAIT = Duration::from_secs(5)`)
- **Mechanics & Failure Mode:**
  In [`session_commands.rs:127-133`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/session_commands.rs#L127-L133), `compress_session` acquires the cross-process `durable_turn_lease` before initiating the out-of-band LLM summary (`agent.summarize_context`). This durable lease is held throughout the slow network LLM summarization call (typically 10-30+ seconds).
  In [`session_admission.rs:81-87`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/session_admission.rs#L81-L87), every inbound turn passing through `admit_turn` must acquire this same `durable_turn_lease` with a hardcoded timeout of `DEFAULT_LEASE_WAIT = 5s`.
  If a user sends a message on any push channel (Telegram, Discord, Slack) while compression summarization is in flight:
  1. `admit_turn` waits for 5 seconds and times out (`anyhow::bail!("durable turn lease wait timed out...")`).
  2. In `session_admission.rs:87`, the `?` operator aborts `admit_turn`.
  3. In `dispatch.rs:527-530`:
     ```rust
     Err(error) => {
         warn!(%error, "could not resolve session for inbound turn");
         return;
     }
     ```
     The dispatcher logs a warning and returns immediately without sending any message, confirmation, or rejection to the chat platform.
  4. The user's inbound turn is permanently lost with no user feedback.
  Furthermore, on the HTTP path ([`message.rs:317-323`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/message.rs#L317-L323)), any turn submitted during summarization fails with a 503 `{"error": "session unavailable"}` after 5 seconds.
- **Proposed Fix:**
  Align with the original transaction-fence architecture: compression should snapshot at a watermark, summarize without holding exclusive execution locks on live turns, and acquire the durable fence only during `publish_gateway_compression` to commit the handoff and clone post-watermark turns. Alternatively, push ingress in `dispatch.rs` must deliver a clear busy notification or queue the message rather than silently dropping it.

---

### Finding 3: Anti-Growth Accounting Undercounts Tool Calls & API Content, Triggering False Compaction Refusals
- **Severity:** Medium
- **Category:** Anti-Growth Accounting / Heuristic Flaw
- **Exact References:**
  - [`rust/crates/hermes-gateway/src/session_commands.rs:236-246`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/session_commands.rs#L236-L246) (`source_chars` calculation)
  - [`rust/crates/hermes-gateway/src/session_db.rs:2481-2491`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/session_db.rs#L2481-L2491) (`CompressionHistoryMessage` struct fields)
  - [`rust/crates/hermes-gateway/src/compression_prompt.rs:15-36`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/compression_prompt.rs#L15-L36) (`build` including `tool_calls` in prompt)
- **Mechanics & Failure Mode:**
  [`session_commands.rs:236-240`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/session_commands.rs#L236-L240) computes:
  ```rust
  let source_chars = head
      .iter()
      .map(|item| item.message.content.chars().count())
      .sum::<usize>();
  if summary_body.chars().count() >= source_chars {
      return Ok(CompressResult {
          reply: "⚠️ Compression refused because the generated checkpoint would not shrink the selected history. The conversation was not changed.".into(),
      });
  }
  ```
  In agent transcripts, assistant turns executing tool calls commonly have `content: ""` with hundreds or thousands of characters of operations residing in `item.tool_calls` or `item.message.api_content`.
  While `compression_prompt::build` faithfully includes `item.tool_calls` in the summarization prompt so the LLM can summarize those actions, `source_chars` counts only `item.message.content`.
  If a transcript has assistant turns whose content is empty or concise while performing significant tool calls, `source_chars` will count only the user questions and tool outputs, severely undercounting the actual context size.
  When the LLM outputs a comprehensive structured summary (`summary_body`), its character count often exceeds this artificially deflated `source_chars`, triggering a false refusal:
  `"⚠️ Compression refused because the generated checkpoint would not shrink the selected history."`
- **Proposed Fix:**
  Calculate `source_chars` over the effective content of the turns being compressed, including `item.tool_calls.as_ref().map_or(0, |t| t.chars().count())` and `item.message.model_content()`, or adopt token estimation consistent with `partial_compress::estimate_tokens`.

---

### Finding 4: Asynchronous Lease Release in Drop Races with 1ms Acquire in `/compress` and Can Wedge Same-Process Sessions
- **Severity:** Medium
- **Category:** Durable Lease Concurrency / Wedge Risk
- **Exact References:**
  - [`rust/crates/hermes-gateway/src/durable_turn_lease.rs:22-36`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/durable_turn_lease.rs#L22-L36) (`impl Drop for DurableTurnLease`)
  - [`rust/crates/hermes-gateway/src/session_commands.rs:131`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/session_commands.rs#L131) (`durable_turn_lease::acquire(..., Duration::from_millis(1))`)
  - [`rust/crates/hermes-gateway/src/session_db.rs:363-378`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/session_db.rs#L363-L378) (`structured_holder_process_is_dead`)
- **Mechanics & Failure Mode:**
  In [`durable_turn_lease.rs:22-36`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/durable_turn_lease.rs#L22-L36):
  ```rust
  impl Drop for DurableTurnLease {
      fn drop(&mut self) {
          ...
          std::thread::spawn(move || {
              if let Err(error) = database.release_session_turn_lease(&session_id, &holder) {
                  tracing::warn!(%error, session = %session_id, "durable turn lease release failed");
              }
          });
      }
  }
  ```
  When a turn finishes and drops `DurableTurnLease`, the database deletion is dispatched to an un-awaited OS thread.
  If the user or an automation script issues `/compress` immediately after a turn completes:
  1. The in-process `transcript_leases` token is dropped immediately.
  2. `/compress` acquires `transcript_leases` successfully.
  3. `/compress` attempts to acquire `durable_turn_lease` with a 1-millisecond timeout (`Duration::from_millis(1)`).
  4. Because thread creation and SQLite transaction startup take longer than 1 ms, the lease row from the just-finished turn is still in `session_turn_leases`.
  5. The 1 ms deadline expires, and `/compress` immediately aborts with:
     `"Agent is running in another Hermes process - /compress did not change the conversation. Wait for that turn to finish, then try again."`
  Additionally, if the thread pool or OS fails to spawn a thread in `Drop` or if a panic occurs, the lease row remains in SQLite. Because [`session_db.rs:368`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/session_db.rs#L368) explicitly excludes the current PID (`*pid != std::process::id()`), the gateway process cannot reclaim its own orphaned lease, wedging the session until the 300-second TTL expires.
- **Proposed Fix:**
  Allow synchronous release or provide a small retry grace period (e.g. 200-500 ms) in `/compress` before declaring a cross-process turn conflict. Provide explicit release handling rather than relying on fire-and-forget OS thread spawning in `Drop`.

---

### Finding 5: Table Name Discrepancy Prevents Cross-Process Coordination with Python CLI (`session_turn_leases` vs `compression_locks`)
- **Severity:** Medium
- **Category:** Cross-Process Concurrency / Schema Divergence
- **Exact References:**
  - [`rust/crates/hermes-gateway/src/session_db.rs:2144-2151`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/session_db.rs#L2144-L2151) (`CREATE TABLE session_turn_leases`)
  - [`hermes_state.py:9141-9250`](file:///home/eins0fx/development/hermes-agent-port/hermes_state.py#L9141-L9250) (`try_acquire_compression_lock` on `compression_locks`)
  - [`agent/conversation_compression.py:3646`](file:///home/eins0fx/development/hermes-agent-port/agent/conversation_compression.py#L3646) (`_try_acquire_lock`)
- **Mechanics & Failure Mode:**
  The review in `native-compression-review-agy.md` and `native-compression-review-claude.md` highlighted that Python CLI and gateway share the SQLite state database and that Python coordinates compression exclusion using the `compression_locks` table.
  The Rust fix created an entirely new table, `session_turn_leases`, rather than interacting with `compression_locks`.
  Because the Python CLI does not know about or query `session_turn_leases`, and the Rust gateway does not check `compression_locks`:
  1. If the Python CLI is running a turn on session S, Rust `/compress` will not observe any lock in `session_turn_leases` and will proceed.
  2. If Rust `/compress` is running, the Python CLI will not observe any lock in `compression_locks` and will execute turns concurrently on the parent session.
  Cross-process mutual exclusion between the Python CLI and Rust gateway remains unenforced.
- **Proposed Fix:**
  Unify the table schema and name with Python (`compression_locks`) or have the gateway dual-check/write to `compression_locks` so that CLI and gateway processes mutually respect each other's locks.

---

## Verified Invariants (Cleared / Non-Issues)

1. **Strict Programmatic Redaction:**
   Input turns and structured content are scrubbed before prompt assembly (`compression_prompt.rs:17-38`, `compression_redact.rs`), focus topics are scrubbed (`compression_prompt.rs:51`), generated summaries are sanitized prior to child transcript insertion (`session_commands.rs:235`), and provider errors are scrubbed before logging (`session_commands.rs:226-227`).
2. **Title and Provenance Transfer:**
   `publish_gateway_compression` queries `title` and `title_source` from the parent session (`session_db.rs:1490-1501`), clears them from the parent to satisfy `idx_sessions_title_unique` (`session_db.rs:1503-1507`), and sets them on the child session row (`session_db.rs:1519-1521`).
3. **Rotation-Stable Prompt-Cache Scope:**
   `NativeAgentClient::run_native_turn` derives `cache_scope` by resolving the logical lineage root via SQLite parent traversal (`native_agent.rs:580-586`). In addition, `release_conversation` is invoked upon publication to cleanly evict the cached client using `RetirementKind::Release` without triggering end-of-session hooks (`conversation_agent.rs:237, 767`).
4. **Partial-Boundary Parity:**
   `partial_compress::partial_boundary` collects user turn boundaries backwards and returns the earliest available user turn index when user turns $< \text{keep\_last}$, falling back to full compression only when the resulting head would be empty (`partial_compress.rs:83-96`).
5. **Neutral Failure Text:**
   Mode-neutral failure messages and logs now replace misleading "preview" errors on live executions in `dispatch.rs:424`, `message.rs:252`, and `session_commands.rs:317`.
6. **Route Revalidation After Waiting:**
   Both `admit_turn` (`session_admission.rs:94-122`) and `compress_session` (`session_commands.rs:148-169`) re-verify `current_entry_for_source(&source)` after acquiring locks to ensure the session was not rotated or repointed while waiting.
7. **Atomic Publication Fence:**
   `publish_gateway_compression` executes inside a single `TransactionBehavior::Immediate` transaction under WAL mode (`session_db.rs:1415`). It verifies the turn lease holder under the transaction lock (`session_db.rs:1417-1430`) and ensures exactly one winner via CAS on `ended_at IS NULL` (`session_db.rs:1591-1595`).
