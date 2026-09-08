# Automatic Context-Compression & Pruning/Micro-Compaction Source Map

## Executive Summary

Commit [`bab208de9d`](file:///home/eins0fx/development/hermes-agent-port/rust/PORT.md#L3-L47) ported native manual compression (`/compress` and `/compact`) to the Rust gateway, introducing rotation-mode child publication ([`publish_compression`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/session_store.rs#L274-L327) / [`publish_gateway_compression`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/session_db.rs#L508-L620)), a tool-free summary generation path on [`NativeAgentClient::summarize_context`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/native_agent.rs#L700-L716), secret redaction ([`compression_redact::redact`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/compression_redact.rs#L10-L65)), and cross-process turn leases ([`durable_turn_lease`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/durable_turn_lease.rs)). Subsequent work on [`SessionDb`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/session_db.rs#L1671-L1780) added [`publish_gateway_in_place_compression`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/session_db.rs#L1671) as the database primitive for in-place message soft-archiving and tail cloning.

This report maps the **automatic trigger**, **pruning policy**, and **micro-compaction semantics** from the authoritative Python implementation to the Rust gateway after commit `bab208de9d`. In accordance with project boundaries, auxiliary model selection, external memory checkpoint hooks, context-engine hooks, and plugin hooks are excluded as they are owned by other helpers.

---

## 1. Exact Trigger Inputs

The automatic context-compaction and pruning subsystems in Python evaluate distinct configuration, runtime, and telemetry inputs across turn boundaries.

### 1.1 Configuration Inputs (`compression.*`)
Defined in [`agent/agent_init.py:2145-2380`](file:///home/eins0fx/development/hermes-agent-port/agent/agent_init.py#L2145-L2380):

| Parameter | Type & Default | Description / Parser Constraints |
|---|---|---|
| `compression.enabled` | `bool` (default `true`) | Master switch. When `false`, automatic compression never triggers. |
| `compression.threshold` | `float` (default `0.50`) | Fractional fill of context window at which compaction triggers. |
| `compression.threshold_tokens` | `Option<int>` (default `None`) | Hard absolute token ceiling. When set, triggers at `min(context_length * threshold, threshold_tokens)`. |
| `compression.model_thresholds` | `dict[str, float]` (default `{}`) | Substring match against model name; longest match overrides global threshold. |
| `compression.protect_last_n` | `int` (default `20`) | Number of most recent messages unconditionally spared from summarization. |
| `compression.protect_first_n` | `int` (default `3`) | Number of initial messages (excluding system prompt) preserved at transcript head. |
| `compression.max_attempts` | `int` (default `3`) | Per-turn cap on compression retry passes. Clamped to `[1, 10]`; rejects booleans and fractional floats. |
| `compression.proactive_prune_tokens` | `int` (default `0`) | Token threshold for cheap deterministic tool output pruning (`0` = disabled). |
| `compression.proactive_prune_min_result_chars` | `int` (default `8000`) | Minimum character length for tool output summarization; floor clamped to `_PRUNE_MIN_CHARS` (`200`). |
| `compression.proactive_prune_min_reclaim_tokens` | `int` (default `4096`) | Minimum tokens a prune must save to commit; prevents prompt-cache churn. |
| `compression.micro_compact` | `bool` (default `false`) | Opt-in switch for per-turn single-exchange rolling compaction during post-turn idle. |
| `compression.micro_compact_every_n_turns` | `int` (default `1`) | Cadence gate: run micro-compaction every N completed turns (clamped >= 1). |
| `compression.micro_compact_defrag_threshold_tokens` | `int` (default `2000`) | Rolling summary size threshold triggering summary defragmentation. |

### 1.2 Runtime & Token Inputs
Evaluated in [`agent/turn_context.py:1130-1300`](file:///home/eins0fx/development/hermes-agent-port/agent/turn_context.py#L1130-L1300) and [`agent/context_compressor.py:3947-3953`](file:///home/eins0fx/development/hermes-agent-port/agent/context_compressor.py#L3947-L3953):

1. **Context Window Ceiling (`context_length`)**: Resolved from model catalog metadata (e.g. 128K, 200K, 1M).
2. **Current Request Token Pressure (`tokens`)**:
   - **Preflight Phase**: Rough estimate of active transcript (`system_prompt` + `history` + inbound `user_message` + active `tool_specs`). Uses [`estimate_request_tokens_rough`](file:///home/eins0fx/development/hermes-agent-port/agent/model_metadata.py#L3965-L4001).
   - **Mid-Turn Tool Loop**: Overhead-aware [`_midturn_request_pressure_tokens`](file:///home/eins0fx/development/hermes-agent-port/agent/conversation_loop.py#L8251-L8258) accounting for newly appended tool results and schemas.
   - **Billed Prompt Usage Anchor (`last_real_prompt_tokens`)**: Exact `usage.prompt_tokens` from provider response, plus delta of unbilled messages appended since.

---

## 2. Thresholds & Trigger Logic

### 2.1 Automatic Compaction Trigger
Evaluated by [`ContextCompressor.should_compress_info`](file:///home/eins0fx/development/hermes-agent-port/agent/context_compressor.py#L3921-L3953):
1. **Effective Threshold**:
   `threshold_tokens = min(int(context_length * threshold), threshold_tokens_config)`
2. **Primary Condition**:
   `current_tokens >= threshold_tokens`
3. **Block Condition**:
   If `current_tokens >= threshold_tokens`, the trigger fires unless [`_automatic_compression_blocked()`](file:///home/eins0fx/development/hermes-agent-port/agent/context_compressor.py#L4009-L4030) returns `true` (see Section 3).

### 2.2 Proactive Tool Result Pruning Trigger
Evaluated by [`ContextCompressor.prune_tool_results_only`](file:///home/eins0fx/development/hermes-agent-port/agent/context_compressor.py#L4519-L4655):
- **Enablement & Token Floor**: `proactive_prune_tokens > 0` and `current_tokens >= proactive_prune_tokens`.
- **Structural Floor**: Transcript length must exceed protected bounds:
  `len(messages) > protect_last_n + protect_head_size + 1`
- **Rearm Gate (Prompt-Cache Hysteresis)**:
  `estimated_message_tokens >= _proactive_prune_rearm_tokens`
  *Exception*: If provider-billed tokens `>= threshold_tokens`, the rearm gate is bypassed to prevent silent lockout ([#101889](file:///home/eins0fx/development/hermes-agent-port/agent/context_compressor.py#L4580-L4586)).
- **Reclaim Gate**: Prune is discarded unless tokens saved satisfy:
  `reclaimed = tokens_before - tokens_after >= proactive_prune_min_reclaim_tokens` (default 4096).

### 2.3 Micro-Compaction Trigger
Evaluated by [`ContextCompressor._micro_compact`](file:///home/eins0fx/development/hermes-agent-port/agent/context_compressor.py#L7589-L7680) in [`agent/turn_finalizer.py:415-465`](file:///home/eins0fx/development/hermes-agent-port/agent/turn_finalizer.py#L415-L465):
- **Turn State**: Turn succeeded (`not interrupted and not failed` and `final_response` present).
- **Flag**: `_micro_compact_enabled is True`.
- **Cadence**: `_micro_compact_turns_since_pass + 1 >= _micro_compact_every_n_turns`.
- **Structural Bounds**: `len(messages) >= 4`, with at least one unabsorbed exchange outside `protect_head` and `find_tail_cut_by_tokens`.
- **Defrag Sub-Trigger**: If tokens in `_micro_compact_rolling_summary` `> micro_compact_defrag_threshold_tokens` (`2000`), a summary defrag runs instead of exchange absorption.

---

## 3. Retry, Rearm & Breaker State

State machine invariants prevent compression failure loops, freeze wedging, and prompt-cache degradation:

```
[Normal State]
      │
      ├─► Compaction Fails (429/Transient) ──► Cooldown Arm (60s monotonic)
      │                                                │
      ├─► History All Protected ──────────────► Structural Backoff (60s monotonic)
      │                                                │
      ├─► Savings < 10% / Anti-Growth ───────► Ineffective Strike += 1
      │                                                │
      │                                    Strike >= 2?
      │                                    ├── No: stay in probation
      │                                    └── Yes: Tripped Breaker (blocked)
      │                                                │
      │                                      Recovery Timer (300s wall-clock)
      │                                                │
      │                                      Probe Allowed: Strike = 1
      │
      └─► Real Provider Usage < Threshold ────► Ineffective Strikes = 0 (Cleared)
```

### 3.1 Per-Turn Attempt Budget
- `compression_attempts` is tracked per agent turn, initialized to `0`, capped at `compression.max_attempts` (default `3`).
- If compression no-ops due to lock contention (`compression_skipped_due_to_lock`), the attempt is refunded.
- Once attempts reach the cap, further automatic compression is skipped with reason `attempts_exhausted:<N>`.

### 3.2 Transient Cooldowns
1. **Summary LLM Failure Cooldown (`_summary_failure_cooldown_until`)**:
   - Monotonic deadline set when summarization encounters HTTP 429 or network timeout (default `60.0s`).
   - Blocks automatic compression (`cooldown:<seconds>`) to eliminate CLI freezing loops ([#11529](file:///home/eins0fx/development/hermes-agent-port/agent/context_compressor.py#L4038-L4042)).
   - Cleared immediately if user executes manual `/compress` with `force=True`.
2. **Structural No-Op Backoff (`_structural_no_op_backoff_until`)**:
   - Monotonic deadline (default `60.0s`) set when history is over threshold but all content lies inside the protected head/tail window ([#93022](file:///home/eins0fx/development/hermes-agent-port/agent/context_compressor.py#L4050-L4056)).
   - Cleared as soon as transcript outgrows the window or a compaction successfully completes.

### 3.3 Anti-Thrashing Circuit Breaker
- **Strikes**: [`_ineffective_compression_count`](file:///home/eins0fx/development/hermes-agent-port/agent/context_compressor.py#L2963-L2972) increments whenever:
  - Compaction candidate is rejected because it fails to shrink history ([`record_rejected_compaction`](file:///home/eins0fx/development/hermes-agent-port/agent/context_compressor.py#L2948-L2972)).
  - Completed compaction saves `< 10%` tokens or real provider usage remains `>= threshold_tokens` ([`_verify_compaction_cleared_threshold`](file:///home/eins0fx/development/hermes-agent-port/agent/context_compressor.py#L3768-L3784)).
- **Trip Condition**: `_ineffective_compression_count >= 2`. Blocks further auto-compaction with reason `"ineffective"`.
- **Probation Probe**: Wall-clock deadline `_anti_thrash_recovery_deadline = now + 300.0s` (`_ANTI_THRASH_RECOVERY_SECONDS`). When elapsed, drops count to `1`, permitting exactly ONE trial probe compaction.
- **Reset**: Any provider response reporting real `prompt_tokens < threshold_tokens` resets `_ineffective_compression_count` to `0`.

### 3.4 Proactive Prune Rearm
- Upon committing a tool result prune, [`_proactive_prune_rearm_tokens`](file:///home/eins0fx/development/hermes-agent-port/agent/context_compressor.py#L4630-L4651) is set to:
  `next_rearm = tokens_after + max(reclaimed, proactive_prune_tokens, proactive_prune_min_reclaim_tokens)`
- Persisted in session database `model_config` under `_proactive_prune_rearm_tokens`.
- Reset to `0` upon session rotation or full conversation compaction.

---

## 4. Pruning & Micro-Compaction Invariant Rules

### 4.1 Deterministic Tool Output Pruning (`_prune_old_tool_results`)
Implemented in [`agent/context_compressor.py:4146-4405`](file:///home/eins0fx/development/hermes-agent-port/agent/context_compressor.py#L4146-L4405):
1. **Pass 1: Deduplication (Tail-Agnostic, Lossless)**:
   - Scans backward across all messages.
   - For `role == "tool"` with text `>= _PRUNE_MIN_CHARS` (`200`), hashes content (`md5(content)[:12]`).
   - Older duplicates are replaced with `"[Duplicate tool output - same content as a more recent call]"`. Runs across the entire transcript including protected tail.
2. **Pass 2: Informative 1-Line Summarization (Outside Protected Tail)**:
   - Replaces tool results larger than `min_prune_chars` with formatted single-line stubs ([`_summarize_tool_result`](file:///home/eins0fx/development/hermes-agent-port/agent/context_compressor.py#L2100-L2126)):
     - `terminal`: `[terminal] ran <cmd> -> exit <code>, <N> lines output`
     - `read_file`: `[read_file] read <path> from line <N> (<chars> chars)`
     - `search_files`: `[search_files] <target> search for '<pattern>' in <path> -> <N> matches`
     - `write_file` / `patch`: `[write_file] wrote to <path> (<N> lines)`
     - Generic fallback: `[<tool_name>] (<chars> chars result)`
   - Skips already-summarized lines, placeholders, and protected active skills.
3. **Pass 3: Tool Call Argument Truncation**:
   - In assistant messages outside protected tail, argument payloads `> 500` chars are truncated within valid JSON structure.

### 4.2 Micro-Compaction Policy (`_micro_compact`)
Implemented in [`agent/context_compressor.py:7589-7680`](file:///home/eins0fx/development/hermes-agent-port/agent/context_compressor.py#L7589-L7680):
- Takes the single oldest uncompacted exchange (user turn + assistant response + tool results).
- Micro-summarizes it using auxiliary call.
- Rolls text into `_micro_compact_rolling_summary`.
- Splices an assistant-role summary marker carrying `_compressed_summary` metadata.
- **Persistence Primitive**: In Python, this relies on [`archive_and_compact`](file:///home/eins0fx/development/hermes-agent-port/hermes_state.py#L13402-L13440) for in-place soft-archiving. In Rust, [`SessionDb::publish_gateway_in_place_compression`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/session_db.rs#L1671) provides this database transaction, but it is not yet exposed via [`SessionStore`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/session_store.rs#L274-L327) or wired into post-turn finalization.

---

## 5. Safe Turn-Loop Insertion Points in Rust

Examining the post-`bab208de9d` gateway implementation reveals three distinct lifecycle phases and their exact wiring locations:

```
[Inbound Message]
       │
       ▼
1. Admission & Leases (session_admission.rs)
       │
       ▼
2. PREFLIGHT COMPRESSION INSERTION POINT  ◄── [Point A: Safe Rotation Gateway]
   • History loaded
   • Estimate tokens vs threshold
   • If over threshold: publish_compression -> child session -> rebind leases
       │
       ▼
3. Agent Turn Starts (dispatch.rs / message.rs -> NativeAgentClient::run_native_turn)
       │
       ▼
4. Model Turn & Tool Loop (native_agent.rs / native_tools.rs)
   • For each tool iteration:
     • Tool executes -> tool_result appended
     • TOOL PRUNING INSERTION POINT       ◄── [Point B: In-Memory Tool Loop Prune]
       • Dedup identical tool outputs
       • Summarize bulky tool results (> min_chars)
       • Truncate large tool_call args (> 500 chars)
       │
       ▼
5. Model Turn Completes
       │
       ▼
6. Persist Assistant Reply (session_db::end_turn)
       │
       ▼
7. TURN FINALIZATION INSERTION POINT      ◄── [Point C: Post-Turn Micro-Compaction]
   • finalize_turn_after_persist
   • Gated off until in-place SessionStore wiring lands
```

### Point A: Preflight Automatic Compression (Gateway Admission)
- **Files & Locations**:
  - Push Ingress: [`rust/crates/hermes-gateway/src/dispatch.rs:580-610`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/dispatch.rs#L580-L610) in [`run_admitted_turn`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/dispatch.rs#L579-L640).
  - HTTP Ingress: [`rust/crates/hermes-gateway/src/message.rs:371-392`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/message.rs#L371-L392) in the admitted turn spawn.
- **Why Safe**:
  1. Leases are already held: `route_lease`, `transcript_lease`, and `_durable_turn_lease` from [`admit_turn`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/session_admission.rs#L29-L120).
  2. The parent session is idle: no model query is in flight.
  3. Uses the proven [`publish_compression`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/session_store.rs#L274-L327) rotation transaction: child session is published atomically, route is updated, `transcript_lease` is rebound via `rebind()`, and old client is evicted via [`agent.release_conversation`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/session_commands.rs#L318).
  4. The subsequent `run_turn_with_context` runs directly against the child session with clean history.

### Point B: Mid-Turn In-Memory Pruning (Tool Loop)
- **File & Location**:
  - [`rust/crates/hermes-gateway/src/native_tools.rs:735-748`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/native_tools.rs#L735-L748) in [`run_tool_loop_with_content`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/native_tools.rs#L513-L757).
- **Why Safe**:
  1. In Rust, intermediate tool call and result turns are accumulated in-memory in `messages: Vec<Value>` during `run_tool_loop_with_content`.
  2. Applying deterministic Phase-1 pruning ([`_prune_old_tool_results`](file:///home/eins0fx/development/hermes-agent-port/agent/context_compressor.py#L4146-L4405)) directly to `messages` after tool execution requires zero network calls and zero database mutations.
  3. Prevents large tool outputs (e.g. multi-megabyte file reads, npm dumps) from blowing past the model context limit before the turn finishes.

### Point C: Post-Turn Micro-Compaction (Turn Finalization)
- **File & Location**:
  - [`rust/crates/hermes-gateway/src/agent.rs:110-119`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/agent.rs#L110-L119) and [`rust/crates/hermes-gateway/src/dispatch.rs:655-666`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/dispatch.rs#L655-L666) in `finalize_turn_after_persist`.
- **Constraint / Status**:
  - While [`SessionDb::publish_gateway_in_place_compression`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/session_db.rs#L1671) provides the raw SQLite transaction, it is not yet wrapped in [`SessionStore`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/session_store.rs#L274-L327).
  - Micro-compaction must remain opt-in disabled (`compression.micro_compact: false`) until in-place session store publication and background summarizer pipelines are completed.

---

## 6. Behavior Tests Matrix

The following test suites from the Python reference codebase define the behavior contracts to replicate in Rust:

| Test Group | Python Source Reference | Core Invariant Verified |
|---|---|---|
| **Preflight Trigger** | [`test_context_compressor.py`](file:///home/eins0fx/development/hermes-agent-port/tests/agent/test_context_compressor.py) | Over-threshold history triggers auto-compaction before model turn; under-threshold passes through untouched. |
| **Max Attempts Ceiling** | [`test_compression_max_attempts_config.py`](file:///home/eins0fx/development/hermes-agent-port/tests/agent/test_compression_max_attempts_config.py) | Halts auto-compression after `max_attempts` passes (default 3) when history remains above threshold. |
| **Anti-Thrash Persistence** | [`test_compression_anti_thrash_persistence.py`](file:///home/eins0fx/development/hermes-agent-port/tests/agent/test_compression_anti_thrash_persistence.py) | Two consecutive ineffective compactions (<10% savings or failure) trip breaker; state persists across restart. |
| **Anti-Thrash Recovery** | [`test_compression_anti_thrash_recovery.py`](file:///home/eins0fx/development/hermes-agent-port/tests/agent/test_compression_anti_thrash_recovery.py) | After 300s window lapses, allows exactly one probation probe by resetting strikes from 2 to 1. |
| **Breaker Usage Reset** | [`test_context_compressor.py`](file:///home/eins0fx/development/hermes-agent-port/tests/agent/test_context_compressor.py) | Provider response reporting real `prompt_tokens < threshold_tokens` resets strike counter to 0. |
| **Tool Result Pruning** | [`test_proactive_tool_result_pruning.py`](file:///home/eins0fx/development/hermes-agent-port/tests/agent/test_proactive_tool_result_pruning.py#L75-L100) | Identical tool outputs deduplicated (MD5); bulky outputs outside `protect_last_n` replaced with 1-line stubs. |
| **Prune Idempotence** | [`test_proactive_tool_result_pruning.py`](file:///home/eins0fx/development/hermes-agent-port/tests/agent/test_proactive_tool_result_pruning.py#L100) | Running pruning multiple times on already-pruned messages produces identical output. |
| **Prune Rearm Runway** | [`test_proactive_prune_rearm_threshold.py`](file:///home/eins0fx/development/hermes-agent-port/tests/agent/test_proactive_prune_rearm_threshold.py#L83-L120) | Committed prune sets rearm runway; skips until tokens regrow; provider-billed over-threshold overrides lockout. |

---

## 7. Recommended Smallest Production-Wired Rust Slice

To deliver immediate value without introducing destabilizing surface area, the next Rust checkpoint should implement the following focused slice:

### 1. What to Implement in the Slice
1. **Preflight Threshold Trigger (`auto_compress_preflight`)**:
   - Location: Integrated into [`dispatch.rs:run_admitted_turn`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/dispatch.rs#L580-L610) and [`message.rs:tokio::spawn`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/message.rs#L371-L392).
   - Logic: Estimate tokens on `history` via [`partial_compress::estimate_tokens`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/partial_compress.rs#L101-L107). If `>= threshold_tokens` and not in cooldown/breaker:
     - Execute rotation compaction reusing [`session_store::publish_compression`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/session_store.rs#L274-L327) and [`NativeAgentClient::summarize_context`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/native_agent.rs#L700-L716).
     - Rebind `transcript_leases` token to child session.
     - Release old client via `agent.release_conversation`.
     - Continue turn execution on child session.
2. **Breaker & Cooldown Tracking**:
   - Track summary failure cooldown (60s monotonic) and ineffective compaction count in memory (and persist `_ineffective_compression_count` in session `model_config`).
   - Latch at `>= 2` strikes; allow probe after 300s.
3. **In-Memory Tool Result Pruning (`prune_tool_results`)**:
   - Location: Inside [`native_tools.rs:run_tool_loop_with_content`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/native_tools.rs#L513-L757).
   - Logic: Deterministic pass over `messages: Vec<Value>`:
     - Byte-identical tool result deduplication using MD5 hash back-reference.
     - 1-line tool output summarization for non-tail outputs exceeding `min_prune_chars`.
     - JSON argument truncation for arguments `> 500` chars.

### 2. What to Keep Explicitly Deferred
1. **In-Place Compaction Wiring (`publish_in_place_compression`)**:
   - Keep manual and automatic compression on the proven rotation path (`publish_compression`) until the store/command layer exposes in-place publication.
2. **Micro-Compaction (`compression.micro_compact`)**:
   - Keep opt-in disabled / fail-closed until in-place store publication and background aux-worker pipelines are ready.
3. **Auxiliary Compression Model Selection**:
   - Summary generation continues using the primary model tool-free step.
