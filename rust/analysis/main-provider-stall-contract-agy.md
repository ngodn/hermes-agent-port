# Main Provider Stall Detection and Timeout Contract Analysis

**Document Target**: `rust/analysis/main-provider-stall-contract-agy.md`
**Evidence Lane**: Python Main-Provider Timeout & Streaming Stale Detection Contract
**Primary Sources**:
- [`hermes_cli/timeouts.py`](file:///home/eins0fx/development/hermes-agent-port/hermes_cli/timeouts.py)
- [`run_agent.py`](file:///home/eins0fx/development/hermes-agent-port/run_agent.py)
- [`agent/chat_completion_helpers.py`](file:///home/eins0fx/development/hermes-agent-port/agent/chat_completion_helpers.py)
- [`agent/reasoning_timeouts.py`](file:///home/eins0fx/development/hermes-agent-port/agent/reasoning_timeouts.py)
- [`agent/model_metadata.py`](file:///home/eins0fx/development/hermes-agent-port/agent/model_metadata.py)
- [`agent/deadline.py`](file:///home/eins0fx/development/hermes-agent-port/agent/deadline.py)
- [`agent/conversation_loop.py`](file:///home/eins0fx/development/hermes-agent-port/agent/conversation_loop.py)
- [`agent/agent_runtime_helpers.py`](file:///home/eins0fx/development/hermes-agent-port/agent/agent_runtime_helpers.py)
- [`agent/process_bootstrap.py`](file:///home/eins0fx/development/hermes-agent-port/agent/process_bootstrap.py)
- [`cli-config.yaml.example`](file:///home/eins0fx/development/hermes-agent-port/cli-config.yaml.example)
- [`tests/hermes_cli/test_timeouts.py`](file:///home/eins0fx/development/hermes-agent-port/tests/hermes_cli/test_timeouts.py)
- [`tests/agent/test_non_stream_stale_timeout.py`](file:///home/eins0fx/development/hermes-agent-port/tests/agent/test_non_stream_stale_timeout.py)
- [`tests/agent/test_reasoning_stale_timeout_floor.py`](file:///home/eins0fx/development/hermes-agent-port/tests/agent/test_reasoning_stale_timeout_floor.py)
- [`tests/run_agent/test_stream_stale_breaker_reset.py`](file:///home/eins0fx/development/hermes-agent-port/tests/run_agent/test_stream_stale_breaker_reset.py)
- [`tests/cron/test_cron_direct_api_call_watchdog.py`](file:///home/eins0fx/development/hermes-agent-port/tests/cron/test_cron_direct_api_call_watchdog.py)
- [`rust/tools/gen_main_provider_stall_goldens.py`](file:///home/eins0fx/development/hermes-agent-port/rust/tools/gen_main_provider_stall_goldens.py)
- [`rust/tools/main-provider-stall-goldens.json`](file:///home/eins0fx/development/hermes-agent-port/rust/tools/main-provider-stall-goldens.json)

---

## 1. Executive Summary & Scope

This audit documents the complete behavioral contract for ordinary main-provider request timeouts, streaming stale detection, and buffered liveness watchdogs in the live Python implementation of Hermes Agent.

It covers:
1. `providers.<id>.request_timeout_seconds` and `providers.<id>.stale_timeout_seconds` configuration surfaces.
2. Per-model overrides (`providers.<id>.models.<model>.timeout_seconds` and `stale_timeout_seconds`).
3. Compatibility environment variables (`HERMES_API_TIMEOUT`, `HERMES_API_CALL_STALE_TIMEOUT`, `HERMES_STREAM_STALE_TIMEOUT`, `HERMES_LOCAL_STREAM_STALE_TIMEOUT`, `HERMES_STREAM_READ_TIMEOUT`, `HERMES_STREAM_RETRIES`, `HERMES_STREAM_STALE_GIVEUP`).
4. Local-endpoint recognition and scaling behaviors across streaming (900.0s) and non-streaming (`inf`).
5. Context-size token estimation and scaling tiers (streaming: 240s/300s; non-streaming: 150s/240s) along with wall-clock run budget interactions.
6. Reasoning-model floor matching and regex word-boundary slug resolution across all supported reasoning families.
7. First-byte versus inter-chunk single-deadline detection semantics, 30s heartbeat intervals, and operator-visible notices.
8. Two-layer retry budgets, pre-visible safe replay vs post-visible length-stub termination, and mid-tool reconnect gates.
9. Cross-turn consecutive stale streak accounting, give-up circuit breaker ceiling (default: 5), and exact reset transitions.
10. Buffered (non-streaming) worker polling and inline direct API call timer watchdogs.
11. Provider-specific boundary notes and exclusions (AWS Bedrock, OpenAI Codex, Anthropic Messages).

Every section explicitly distinguishes behavior proven by executed Python source code via the oracle generator [`rust/tools/gen_main_provider_stall_goldens.py`](file:///home/eins0fx/development/hermes-agent-port/rust/tools/gen_main_provider_stall_goldens.py) (producing 168 deterministic golden cases in [`rust/tools/main-provider-stall-goldens.json`](file:///home/eins0fx/development/hermes-agent-port/rust/tools/main-provider-stall-goldens.json)) from control-flow loop structures identified through source inspection.

---

## 2. Production Path Architecture & Two-Dimensional Timeout System

The Python implementation operates across two distinct, orthogonal timeout dimensions:

```
+---------------------------------------------------------------------------------------------------------+
|                                    MAIN PROVIDER TIMEOUT DIMENSIONS                                     |
+---------------------------------------------------------------------------------------------------------+
|  DIMENSION 1: TOTAL REQUEST TIMEOUT                                                                     |
|  - Controls the overall HTTP round-trip ceiling for a provider call.                                    |
|  - Source: providers.<id>.models.<model>.timeout_seconds -> providers.<id>.request_timeout_seconds       |
|            -> HERMES_API_TIMEOUT -> 1800.0s default.                                                    |
|  - Wired as per-call timeout kwarg and httpx write/total budget.                                        |
|  - Socket connect/pool phase capped at min(base, 60.0s) if configured, else 30.0s.                      |
+---------------------------------------------------------------------------------------------------------+
|  DIMENSION 2: STALE INACTIVITY WATCHDOG (LIVENESS DEADLINE)                                             |
|  - Controls maximum permitted silence (absence of bytes/chunks/events) before declaring a stall.       |
|  - Single-deadline model: clock resets on start AND on every real chunk received.                       |
|  - Streaming Base: providers.<id>.models.<model>.stale_timeout_seconds                                  |
|                    -> providers.<id>.stale_timeout_seconds -> HERMES_STREAM_STALE_TIMEOUT (180.0s)      |
|                    -> Local endpoint (900.0s) OR Context Scaling (240s/300s) + Reasoning Floor.        |
|  - Non-Streaming Base: providers.<id>.models.<model>.stale_timeout_seconds                              |
|                        -> providers.<id>.stale_timeout_seconds -> HERMES_API_CALL_STALE_TIMEOUT (90.0s) |
|                        -> Reasoning Floor -> Local endpoint (inf) -> Context Scaling (150s/240s).      |
+---------------------------------------------------------------------------------------------------------+
```

### 2.1 Execution Topology: Two-Layer Retry & Liveness Monitoring

```
+-----------------------------------------------------------------------------------------+
| Outer Conversation Loop (agent/conversation_loop.py)                                   |
| - Controls turn-level iteration and attempt counter retry_count (0..max_retries)        |
| - Catches TimeoutError and transport exceptions (classified as FailoverReason.timeout)  |
| - Transport ladder: attempt 1 & 2 retry same provider; attempt 3 activates fallback     |
| - Streak give-up check: _check_stale_giveup(agent) verifies streak < giveup before call |
|                                                                                         |
|   +---------------------------------------------------------------------------------+   |
|   | Inner Streaming Dispatcher (agent/chat_completion_helpers.py)                   |   |
|   | - Thread 1: Worker (_call) runs httpx stream read loop                          |   |
|   |   - Retries up to HERMES_STREAM_RETRIES (default 2 retries = 3 attempts total)  |   |
|   |   - Mid-tool error: silent retry permitted if transient; otherwise fail-closed  |   |
|   | - Thread 2: Monitor (_monitor_loop) polls every 0.3s                            |   |
|   |   - Tracks _stale_elapsed = time.time() - last_chunk_time["t"]                  |   |
|   |   - Checks 30s heartbeat -> emits waiting status notice                         |   |
|   |   - If _stale_elapsed > _stream_stale_timeout:                                  |   |
|   |       - Aborts worker client socket via stranger-thread helper                  |   |
|   |       - Increments agent._consecutive_stale_streams streak                      |   |
|   |       - Resets last_chunk_time["t"] to prevent rapid duplicate kills            |   |
|   +---------------------------------------------------------------------------------+   |
+-----------------------------------------------------------------------------------------+
```

---

## 3. Configuration Precedence Cascades

### 3.1 Total Request Timeout Cascade
Source: [`hermes_cli/timeouts.py:14-40`](file:///home/eins0fx/development/hermes-agent-port/hermes_cli/timeouts.py#L14-L40), [`run_agent.py:1522-1540`](file:///home/eins0fx/development/hermes-agent-port/run_agent.py#L1522-L1540)

The total request timeout resolver `AIAgent._resolved_api_call_timeout()` evaluates in strict order:
1. **Per-model override**: `config.yaml` -> `providers.<id>.models.<model>.timeout_seconds`.
2. **Per-provider setting**: `config.yaml` -> `providers.<id>.request_timeout_seconds`.
3. **Environment fallback**: `os.environ["HERMES_API_TIMEOUT"]` (parsed via `env_float`).
4. **Built-in default**: `1800.0` seconds (30 minutes).

### 3.2 Streaming Stale Timeout Cascade
Source: [`agent/chat_completion_helpers.py:5464-5523`](file:///home/eins0fx/development/hermes-agent-port/agent/chat_completion_helpers.py#L5464-L5523)

The streaming stale timeout `_stream_stale_timeout` evaluates in strict order:
1. **Per-model override**: `providers.<id>.models.<model>.stale_timeout_seconds`.
2. **Per-provider setting**: `providers.<id>.stale_timeout_seconds`.
3. **Environment fallback**: `os.environ["HERMES_STREAM_STALE_TIMEOUT"]` (default `180.0` seconds).
4. **Local-Endpoint Branch**:
   If the base resolved in steps 1-3 equals exactly `180.0` AND `agent.base_url` satisfies `is_local_endpoint(agent.base_url)`:
   - Sets timeout to `900.0` seconds (or `config.yaml` `agent.local_stream_stale_timeout`, overridden by `HERMES_LOCAL_STREAM_STALE_TIMEOUT`).
   - **Crucial Invariant**: When this local branch executes, the subsequent context scaling and reasoning floor branches are **bypassed**.
   - If the user explicitly configured a base other than `180.0` (e.g. `60.0` or `300.0`), the local escalation does not trigger, respecting the user choice.
5. **Context-Size Scaling Branch** (cloud models or non-default local base):
   - `estimate_request_context_tokens(api_kwargs) > 100_000`: `max(base, 300.0)` seconds.
   - `estimate_request_context_tokens(api_kwargs) > 50_000`: `max(base, 240.0)` seconds.
   - Otherwise: `base`.
6. **Reasoning-Model Floor**:
   `get_reasoning_stale_timeout_floor(model)`: if a floor exists, `_stream_stale_timeout = max(_stream_stale_timeout, floor)`.

### 3.3 Non-Streaming (Buffered) Stale Timeout Cascade
Source: [`run_agent.py:1542-1620`](file:///home/eins0fx/development/hermes-agent-port/run_agent.py#L1542-L1620)

`AIAgent._resolved_api_call_stale_timeout_base()` and `_compute_non_stream_stale_timeout()` evaluate in strict order:
1. **Per-model override**: `providers.<id>.models.<model>.stale_timeout_seconds` -> `(val, False)`.
2. **Per-provider setting**: `providers.<id>.stale_timeout_seconds` -> `(val, False)`.
3. **Environment fallback**: `os.environ["HERMES_API_CALL_STALE_TIMEOUT"]` -> `(float(val), False)`.
4. **Reasoning-Model Floor**: `get_reasoning_stale_timeout_floor(model)` -> `(floor, False)`.
5. **Built-in default**: `(90.0, True)` (flagged with `uses_implicit_default = True`).
6. **Local-Endpoint Short-Circuit**:
   If `uses_implicit_default` is `True` AND `agent.base_url` is a local endpoint:
   - Returns `float("inf")` (disabling non-stream watchdog for local models).
   - **Crucial Invariant**: If a reasoning floor matched in step 4, `uses_implicit_default` is `False`. Therefore, a reasoning model on a local endpoint does **not** return `inf`; it retains its reasoning floor (e.g. 600.0s).
7. **Context-Size Scaling**:
   - `> 100_000` tokens: `max(base, 240.0)` seconds.
   - `> 50_000` tokens: `max(base, 150.0)` seconds.
   - Otherwise: `base`.
8. **Wall-Clock Run Budget Cap**:
   If `agent.run_budget_seconds` is active and `not agent._stale_timeout_is_explicit()`:
   - Implicit timeout is capped at `max(60.0, remaining_budget * 0.5)`.
   - Explicit user-configured timeouts (`stale_timeout_seconds` or env var) are **never** capped.

---

## 4. Coercion and Normalization Rules

Source: [`hermes_cli/timeouts.py:4-11`](file:///home/eins0fx/development/hermes-agent-port/hermes_cli/timeouts.py#L4-L11), [`agent/deadline.py:189-216`](file:///home/eins0fx/development/hermes-agent-port/agent/deadline.py#L189-L216)

All timeout configuration values pass through `_coerce_timeout(raw)`:
- Values are parsed via `float(raw)`.
- Any `TypeError` or `ValueError` returns `None` (unset).
- Values `<= 0` (zero, negative numbers) return `None` (unset / unbounded).
- Strings with non-numeric units (e.g. `"120s"`, `"1m"`) fail `float()` parsing and return `None`.
- Booleans: In Python, `float(True) == 1.0` (returns `1.0`), while `float(False) == 0.0` (clamped to `None`).
- `agent.deadline.clamp_timeout` adds protection against macOS `time_t` overflow inside `threading.Lock.acquire`:
  - `NaN` values return `None`.
  - Values exceeding `MAX_SAFE_TIMEOUT_S = 31_536_000.0` (365 days) clamp to `31_536_000.0`.

---

## 5. Reasoning-Model Stale Timeout Floors

Source: [`agent/reasoning_timeouts.py`](file:///home/eins0fx/development/hermes-agent-port/agent/reasoning_timeouts.py)

Reasoning models routinely spend minutes in extended thinking before emitting their first content token. The floor prevents cloud gateways and local watchdogs from prematurely dropping connections during the thinking phase.

### 5.1 Matching Semantics & Regex Invariants
- **Aggregator Prefix Stripping**: Everything before and including the last `/` (e.g. `openai/`, `deepseek/`, `nvidia/`) is stripped before matching.
- **Start-of-Slug Anchor**: Matching regex anchors at the start of the remaining slug (`^`).
- **Right Delimiter Anchor**: The pattern requires end-of-string or one of the separator characters `[\-._:]` (`(?:$|[\-._:])`). The colon `:` is included to support OpenRouter SKU/routing suffixes (e.g. `:free`, `:nitro`, `:floor`).
- **Longest Slug Priority**: Sorted by descending slug length so `o3-mini` (300s) matches before `o3` (600s).
- **Fork / Derivative Safety**: A model named `llama-4-70b-o1-preview` does **not** match `o1` because `o1` appears at character offset 12, not at character offset 0.

### 5.2 Canonical Floor Table
All entries below are proven by executed source in `rust/tools/gen_main_provider_stall_goldens.py`:

| Model Family / Slug Prefix | Floor (s) | Representative Example Slugs |
| :--- | :---: | :--- |
| `nemotron-3-ultra` | 600.0 | `nvidia/nemotron-3-ultra-550b-a55b` |
| `nemotron-3-super` | 600.0 | `nvidia/nemotron-3-super-120b-a12b` |
| `nemotron-3-nano` | 300.0 | `nvidia/nemotron-3-nano-30b-a3b` |
| `nemotron-3.5-lightning` | 300.0 | `nvidia/nemotron-3.5-lightning-30b-a3b` |
| `deepseek-r1` | 600.0 | `deepseek/deepseek-r1`, `deepseek-r1-distill-llama-70b` |
| `deepseek-reasoner` | 600.0 | `deepseek/deepseek-reasoner` |
| `deepseek-v4-flash` | 600.0 | `deepseek/deepseek-v4-flash`, `deepseek-v4-flash-free` |
| `deepseek-v4-pro` | 600.0 | `deepseek/deepseek-v4-pro` |
| `qwq-32b` | 300.0 | `qwen/qwq-32b-preview` |
| `qwen3` | 180.0 | `qwen/qwen3-235b-a22b-thinking`, `qwen3-32b` |
| `o1` | 600.0 | `openai/o1` |
| `o1-mini` | 600.0 | `openai/o1-mini` |
| `o1-pro` | 600.0 | `openai/o1-pro` |
| `o1-preview` | 600.0 | `openai/o1-preview` |
| `o3` | 600.0 | `openai/o3` |
| `o3-pro` | 600.0 | `openai/o3-pro` |
| `o3-mini` | 300.0 | `openai/o3-mini` |
| `o4-mini` | 300.0 | `openai/o4-mini` |
| `claude-opus-4` | 240.0 | `anthropic/claude-opus-4-6`, `claude-opus-4-20250514` |
| `claude-opus-5` | 240.0 | `anthropic/claude-opus-5` |
| `claude-sonnet-5` | 180.0 | `anthropic/claude-sonnet-5` |
| `claude-sonnet-4.5` | 180.0 | `anthropic/claude-sonnet-4.5` |
| `claude-sonnet-4.6` | 180.0 | `anthropic/claude-sonnet-4.6` |
| `claude-fable` | 600.0 | `anthropic/claude-fable-5`, `claude-fable` |
| `grok-4-fast-reasoning` | 300.0 | `x-ai/grok-4-fast-reasoning` |
| `grok-4.20-reasoning` | 300.0 | `x-ai/grok-4.20-reasoning` |
| `grok-4.5` | 300.0 | `x-ai/grok-4.5` |
| `grok-4.6` | 300.0 | `x-ai/grok-4.6` |
| `grok-4-fast-non-reasoning` | 180.0 | `x-ai/grok-4-fast-non-reasoning` |
| `ox-alpha` | 300.0 | `stealth/ox-alpha` |
| `x-preview-f-free` | 300.0 | `x-preview-f-free` |
| `inkling` | 300.0 | `thinkingmachines/inkling`, `inkling:free`, `inkling-small:free` |

---

## 6. Context-Size Estimation and Scaling Tiers

### 6.1 Token Estimation
Source: [`agent/chat_completion_helpers.py:463-514`](file:///home/eins0fx/development/hermes-agent-port/agent/chat_completion_helpers.py#L463-L514)

The estimation function `estimate_request_context_tokens(api_payload)` handles four polymorphic input forms using a 4-chars-per-token heuristic:
1. `list` (bare messages): Sum of string length of all message elements `// 4`.
2. `dict` with `"messages"` (Chat Completions): Sum of string length of messages plus `"tools"` (if present) `// 4`.
3. `dict` with `"input"` (Responses API): Sum of string lengths of `"input"` + `"instructions"` + `"tools"` `// 4`.
4. Generic `dict`: Sum of string lengths of all dictionary values `// 4`.
5. Empty inputs (`None`, `[]`, `{}`): Return `0`.

### 6.2 Scaling Tier Differences: Streaming vs Non-Streaming
A critical architectural divergence exists between streaming and non-streaming context scaling:

| Token Threshold | Streaming Path Timeout | Non-Streaming Path Timeout |
| :--- | :---: | :---: |
| `<= 50,000` tokens | `base` (default `180.0s`) | `base` (default `90.0s`) |
| `50,001` to `100,000` tokens | `max(base, 240.0s)` | `max(base, 150.0s)` |
| `> 100,000` tokens | `max(base, 300.0s)` | `max(base, 240.0s)` |

---

## 7. Local-Endpoint Detection and Scaling Mechanics

Source: [`agent/model_metadata.py:993-1040`](file:///home/eins0fx/development/hermes-agent-port/agent/model_metadata.py#L993-L1040)

### 7.1 Address Class Recognition
`is_local_endpoint(base_url)` parses the hostname and returns `True` for:
1. **Loopback hostnames and IPs**: `localhost`, `127.0.0.0/8`, `::1`.
2. **Container-internal DNS**: hostnames ending in `.docker.internal`, `.lima.internal`, `.local`.
3. **Unqualified hostnames** (no dots): Docker Compose service names (e.g. `http://ollama:11434`, `http://vllm:8000`).
4. **RFC 1918 Private Ranges**: `10.0.0.0/8`, `172.16.0.0/12`, `192.168.0.0/16`.
5. **Link-Local & Tailscale CGNAT**: `100.64.0.0/10` (permitting remote Tailscale Ollama nodes to receive local timeout treatment).

### 7.2 Streaming vs Non-Streaming Local Behavior
- **Streaming**: Default base `180.0s` escalates to `900.0s` (via `HERMES_LOCAL_STREAM_STALE_TIMEOUT` or config `agent.local_stream_stale_timeout`). An explicit user base (e.g. `60.0s` or `300.0s`) remains untouched.
- **Non-Streaming**: Implicit default `90.0s` escalates to `float("inf")` (watchdog disarmed). An explicit user timeout or reasoning model floor is preserved and remains finite.
- **Socket Read Timeout**: In streaming mode, httpx read timeout escalates from `120.0s` to `_base_timeout` (`1800.0s`).

---

## 8. Streaming Socket Read Timeout & Coordination

Source: [`agent/chat_completion_helpers.py:4216-4260`](file:///home/eins0fx/development/hermes-agent-port/agent/chat_completion_helpers.py#L4216-L4260)

When constructing the httpx client inside `_call_chat_completions`:
- **Base Request Timeout (`_base_timeout`)**: `get_provider_request_timeout()` if set, else `HERMES_API_TIMEOUT` (default `1800.0s`).
- **Read Timeout Coordination**:
  - If provider config is set: read timeout matches `_provider_timeout_cfg`.
  - Otherwise, read timeout defaults to `HERMES_STREAM_READ_TIMEOUT` (`120.0s`).
  - If read timeout is `120.0s` and endpoint is local: read timeout raises to `_base_timeout` (`1800.0s`).
  - If read timeout is `120.0s` and `_stream_stale_timeout > 120.0s`: read timeout raises to match `_stream_stale_timeout` (e.g. 240s, 300s, 600s). This prevents the raw socket read timeout from preempting the stale watchdog, preserving structured error logging and recovery.
- **Connect & Pool Caps**:
  - If provider config is set: `min(_base_timeout, 60.0s)`.
  - Otherwise: `30.0s`.

---

## 9. Single-Deadline Stale Detection, Heartbeats, and Visibility

Source: [`agent/chat_completion_helpers.py:5551-5705`](file:///home/eins0fx/development/hermes-agent-port/agent/chat_completion_helpers.py#L5551-L5705)

### 9.1 Single Deadline for First-Byte and Inter-Chunk Silence
The Python streaming implementation does not maintain separate time-to-first-byte and inter-chunk timers on the main OpenAI/Anthropic path:
- `last_chunk_time["t"]` is stamped at request start (`time.time()`).
- On every SDK-visible provider chunk, `last_chunk_time["t"]` updates to `time.time()`.
- Raw SSE comments and empty separators are not SDK-visible chunks. OpenAI SDK 2.24.0's `SSEDecoder.decode()` returns `None` for comment lines and empty events, so keepalive pings do not refresh Hermes' timestamp.
- The stale condition `time.time() - last_chunk_time["t"] > _stream_stale_timeout` governs both silence before the first meaningful event and silence between any two subsequent SDK-visible chunks.

### 9.2 The 30s Heartbeat Notice
- The background monitor thread ticks every `0.3s`.
- Every `30.0s` (`_HEARTBEAT_INTERVAL = 30.0`), it checks silence duration:
  - If silence >= 30s: emits operator notice:
    `⏳ waiting on {model} -- {waiting_secs}s with no output yet (provider may be slow or overloaded, or the model is thinking; auto-reconnect at {timeout}s)`
  - Touches agent activity tracker so gateway inactivity watchdogs do not prematurely kill the session.

### 9.3 Stale Kill Sequence & Side Effects
When `_stale_elapsed > _stream_stale_timeout`:
1. Logs structured warning: `Stream stale for {elapsed}s (threshold {threshold}s) -- no chunks received. model={model} context=~{tokens} tokens. Killing connection.`
2. Buffers status message: `⚠️ No response from provider for {int(elapsed)}s (model: {model}, context: ~{tokens} tokens). Reconnecting...`
3. Cancels attempt and aborts the request-local socket via `_close_request_client_once("stale_stream_kill")`.
4. Bumps consecutive stale streak: `_bump_stale_streak(agent)`.
5. Resets `last_chunk_time["t"] = time.time()` to prevent cascading re-kills during socket unwinding.
6. Emits wait notice: `⚠ no output from provider for {int(elapsed)}s -- reconnecting...`
7. Touches activity tracker: `stale stream detected after {int(elapsed)}s, reconnecting`.

---

## 10. Stream Failure Boundaries: Pre-Visible vs Post-Visible

Source: [`agent/chat_completion_helpers.py:5180-5295, 5865-5874`](file:///home/eins0fx/development/hermes-agent-port/agent/chat_completion_helpers.py#L5180-L5295)

### 10.1 Pre-Visible Stalls (`deltas_were_sent == False`)
- **Safe Replay**: Because no visible deltas or user-facing text have been emitted, replaying the request produces no duplicated text.
- **Inner Streaming Worker Replays**:
  - The worker loop retries up to `HERMES_STREAM_RETRIES` (default `2` retries, meaning `3` total attempts).
  - Stale kills abort the socket, allowing the loop to reopen a fresh connection.
  - After attempt 3 fails, the worker logs `❌ Connection to provider failed after 3 attempts...` and re-raises the exception.
- **Outer Conversation Loop Handling**:
  - The conversation loop catches the re-raised exception.
  - Exception classifies as `FailoverReason.timeout`.
  - `_is_transport_failure = classified.reason in {FailoverReason.timeout, FailoverReason.overloaded}`.
  - `_should_fallback = ... or (_is_transport_failure and retry_count >= 2)`.
  - Attempt 1 and attempt 2 retry on the same provider; attempt 3 (`retry_count >= 2`) triggers provider fallback.

### 10.2 Post-Visible Stalls (`deltas_were_sent == True`)
- **Pure Text Output (No Tool In Flight)**:
  - Silent replay is strictly **forbidden** (`_can_silent_retry == False`) because replay would duplicate already-visible preamble text.
  - The worker aborts the stream and returns a partial stream stub:
    - ID: `PARTIAL_STREAM_STUB_ID` (`"partial_stream_recovered_output"`).
    - Finish Reason: `FINISH_REASON_LENGTH` (`"length"`).
    - Content: Preserves text accumulated up to the failure point.
  - **Streak Reset**: The partial stub resets the stale streak (`_reset_stale_streak`) because the provider demonstrably responded with tokens.
- **Mid-Tool Call Reconnect Gate**:
  - If a tool call was in flight when the stream dropped and the error is transient (`_is_timeout`, `_is_conn_err`, `_is_sse_conn_err`), silent retry is permitted.
  - Emits marker: `\n\n⚠ Connection dropped mid tool-call; reconnecting…\n\n`.
  - Resets streamed assistant text tracking and argument accumulators so the retry starts clean.

---

## 11. Stale Streak Lifecycle & Give-Up Circuit Breaker

Source: [`agent/chat_completion_helpers.py:700-756, 804-816`](file:///home/eins0fx/development/hermes-agent-port/agent/chat_completion_helpers.py#L700-L756)

### 11.1 The Consecutive Stale Counter (`_consecutive_stale_streams`)
- Stored directly on `agent._consecutive_stale_streams`.
- Persists across turns within the same agent session.
- **Increment Triggers**:
  1. Streaming monitor stale kill (`_bump_stale_streak`).
  2. Non-streaming monitor stale kill (`_bump_stale_streak`).
  3. Direct inline API call watchdog expiry (`_bump_stale_streak`).
  4. Bedrock streaming monitor stale kill (`_bump_stale_streak`).
  5. User interrupt during silence >= 30s (`_record_interrupted_provider_wait`).
- **Reset Triggers (`_reset_stale_streak`)**:
  1. Successful streaming response (`result["response"] is not None`).
  2. Partial stream stub returned after tokens were delivered.
  3. Successful non-streaming response.
  4. Successful direct inline API call.
  5. Provider fallback activation (`try_activate_fallback`): the streak belonged to the failing provider and must not wedge the fallback.
  6. Model switch (`switch_model`): user-selected provider receives a clean slate.
  7. Primary runtime restoration at new turn (`restore_primary_runtime`).
- **Preservation Invariants**:
  - Failed model switch (rolls back) preserves the streak.
  - Fallback exhaustion (no candidate left) preserves the streak.
  - In-turn retries without provider swap preserve the streak.

### 11.2 The Give-Up Circuit Breaker (`HERMES_STREAM_STALE_GIVEUP`)
- Checked at entry to `interruptible_streaming_api_call`, `interruptible_api_call`, and `direct_api_call` via `_check_stale_giveup(agent)`.
- Default threshold: `5` consecutive stale attempts.
- If `_giveup > 0` and `_streak >= _giveup`:
  Raises `RuntimeError`:
  `Provider has been unresponsive (no response received) for {_streak} consecutive stale attempts -- aborting this call to avoid an indefinite stall. Switch models or start a new session, then retry.`
- Setting `HERMES_STREAM_STALE_GIVEUP=0` or negative disables the breaker.

---

## 12. Buffered (Non-Streaming) Timeout Behavior

Source: [`agent/chat_completion_helpers.py:1112-1380, 1393-1932`](file:///home/eins0fx/development/hermes-agent-port/agent/chat_completion_helpers.py#L1112-L1380)

### 12.1 Interactive Background Worker (`interruptible_api_call`)
- Spawns background daemon thread running `_dispatch_nonstreaming_api_request`.
- Main thread polls every `0.3s`.
- If `_elapsed > _stale_timeout`:
  - Reports non-streaming stale kill.
  - Buffers status: `⚠️ No response from provider for {int(elapsed)}s (non-streaming, model: {model}). Aborting call.`
  - Aborts request client socket, bumps streak, touches activity, and sets `TimeoutError`.

### 12.2 Inline Execution (`direct_api_call`)
- Used for cron turns and delegated subagent calls to prevent nested thread-pool deadlocks (`should_use_direct_api_call`).
- Uses `_resolve_direct_stale_timeout(agent, api_kwargs)`.
- Implements hard socket backstop `_inline_nonstream_hard_timeout(stale_timeout)` which injects `httpx.Timeout(connect=min(stale, 60), read=stale, write=min(stale, 60), pool=min(stale, 60))`.
- Uses `threading.Timer(stale_timeout, _on_stale)`:
  - Transition protected by `request_client_lock`: stamps `request_state["stale"] = True`.
  - Aborts client socket.
  - Bumps stale streak under the lock.
  - Caller catches abort and raises `TimeoutError`.

---

## 13. Provider-Specific Exclusions and Boundary Notes

1. **AWS Bedrock**:
   - `providers.<id>.request_timeout_seconds` and `stale_timeout_seconds` are **not** wired in Python; boto3 manages its own client timeouts (`cli-config.yaml.example:242`).
   - Bedrock streaming watchdog uses `_derive_stream_stale_timeout`: enforces cloud base (180.0s), context scaling, and normalizes inference profile IDs (e.g. `us.anthropic.claude-opus-4-6-v1:0` -> `claude-opus-4-6` floor 240s).
   - Shares the cross-turn circuit breaker (`_bump_stale_streak`, `_reset_stale_streak`, `_check_stale_giveup`).
2. **OpenAI Codex (`api_mode == "codex_responses"`)**:
   - Out of scope for native chat completions except for boundary definitions.
   - Features dedicated TTFB watchdog (`HERMES_CODEX_TTFB_TIMEOUT_SECONDS`, default 120s), event idle watchdog (`HERMES_CODEX_EVENT_STALE_TIMEOUT_SECONDS`, default 12s), gateway scale floor (`openai_codex_stale_timeout_floor`, up to 1200s), and hard ceiling (`HERMES_CODEX_HARD_TIMEOUT_SECONDS`, default 1500s).
   - Features silent-hang hint (`_codex_silent_hang_hint`) for `gpt-5.5` family.
3. **Anthropic Messages (`api_mode == "anthropic_messages"`)**:
   - Uses request-local client whose socket is aborted from the watchdog thread (`_abort_request_anthropic_client`), preventing FD recycling corruption on shared clients.
   - `build_anthropic_client` defaults to `read=900.0s`, `connect=10.0s`.

---

## 14. Verification, Exact Commands, Invariants, and Deferrals

### 14.1 Verification Commands Run
- Test Generator Execution:
  `python3 /home/eins0fx/development/hermes-agent-port/rust/tools/gen_main_provider_stall_goldens.py`
- Test Generator Parity Check:
  `python3 /home/eins0fx/development/hermes-agent-port/rust/tools/gen_main_provider_stall_goldens.py --check`
- Smallest Relevant Focused Python Test Suites:
  `pytest tests/hermes_cli/test_timeouts.py tests/run_agent/test_stream_stale_breaker_reset.py`
  `pytest tests/agent/test_non_stream_stale_timeout.py tests/agent/test_reasoning_stale_timeout_floor.py tests/cron/test_cron_direct_api_call_watchdog.py`

### 14.2 Case Counts
- Total Goldens Sections: **13**
- Total Golden Test Cases: **168**
  - `timeout_coercion_and_normalization`: 26 cases
  - `request_timeout_precedence`: 8 cases
  - `stale_timeout_precedence`: 7 cases
  - `reasoning_model_floors`: 43 cases
  - `context_token_estimation`: 5 cases
  - `context_size_scaling`: 15 cases
  - `local_endpoint_scaling`: 23 cases
  - `socket_read_and_connect_bounds`: 8 cases
  - `stale_detection_and_operator_visibility`: 3 cases
  - `pre_vs_post_visible_effects`: 3 cases
  - `stale_streak_circuit_breaker`: 10 cases
  - `buffered_timeout_behavior`: 4 cases
  - `provider_exclusions_and_boundaries`: 5 cases
- Focused Python Unit Test Count: **68 passed tests** across 5 test suites in under 8 seconds.

### 14.3 Parity Invariants for Rust Implementation
1. **Single Re-Armed Deadline**: In `forward_sse`, `tokio::time::timeout(stale_deadline, stream.next())` must re-arm on every returned chunk. There must not be a separate pre-first-byte and inter-chunk deadline.
2. **Replay Barrier**: Pre-visible stream stalls (0 chunks delivered) may safely replay up to the retry ceiling (matching `HERMES_STREAM_RETRIES` = 2 retries / 3 attempts) and then trigger fallback. Post-visible stream stalls (>= 1 chunk delivered) must fail closed without replay to prevent duplicate output.
3. **Cross-Turn Persistent Streak**: The consecutive stale counter must survive turn boundaries on the client/agent instance, increment on each stale failure, and reset to zero on stream completion, partial stub delivery, fallback activation, or model switch.
4. **Give-Up Ceiling**: When consecutive stale attempts reach `5` (or `HERMES_STREAM_STALE_GIVEUP`), calls must abort immediately without issuing an HTTP request.
5. **Reasoning Floor Priority**: Known reasoning models must raise the stale detection threshold to their floor (e.g. 600s for o1/DeepSeek-R1/Nemotron, 240s for Opus 4), preventing false stale kills during the model's thinking phase.
6. **Local Endpoint Patience**: Local endpoints must receive extended patience (900s for streaming, `inf` for default non-streaming).

### 14.4 Uncertainties and Deferrals
- **Typed Config Struct Port**: Per-provider and per-model YAML config keys are deferred until the full typed configuration schema port lands. In the interim, reading scalars via untyped user config or environment variables matches the existing pattern.
- **Wall-Clock Run Budget in Rust**: The run budget cap (`max(60.0, remaining * 0.5)`) requires a session run-budget tracker not yet present in Rust; deferred to the evaluation runner port.
- **Direct Inline Cron Mode**: Rust utilizes an async runtime (Tokio) where thread-pool deadlocks from nested sync calls do not exist; `direct_api_call` watchdog specifics are relevant only if synchronous blocking bridges are ported.
