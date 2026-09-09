# Main Provider Retry and Fallback Contract Analysis

**Document Target**: `rust/analysis/main-provider-retry-contract-agy.md`
**Evidence Lane**: Live Python Main-Turn Retry & Provider-Fallback Contract
**Primary Sources**:
- [`agent/conversation_loop.py`](file:///home/eins0fx/development/hermes-agent-port/agent/conversation_loop.py)
- [`agent/error_classifier.py`](file:///home/eins0fx/development/hermes-agent-port/agent/error_classifier.py)
- [`agent/chat_completion_helpers.py`](file:///home/eins0fx/development/hermes-agent-port/agent/chat_completion_helpers.py)
- [`agent/agent_runtime_helpers.py`](file:///home/eins0fx/development/hermes-agent-port/agent/agent_runtime_helpers.py)
- [`agent/turn_retry_state.py`](file:///home/eins0fx/development/hermes-agent-port/agent/turn_retry_state.py)
- [`agent/empty_response_guard.py`](file:///home/eins0fx/development/hermes-agent-port/agent/empty_response_guard.py)
- [`agent/retry_utils.py`](file:///home/eins0fx/development/hermes-agent-port/agent/retry_utils.py)
- [`agent/transports/base.py`](file:///home/eins0fx/development/hermes-agent-port/agent/transports/base.py)
- [`agent/transports/chat_completions.py`](file:///home/eins0fx/development/hermes-agent-port/agent/transports/chat_completions.py)
- [`agent/transports/responses_api.py`](file:///home/eins0fx/development/hermes-agent-port/agent/transports/responses_api.py)
- [`agent/transports/anthropic.py`](file:///home/eins0fx/development/hermes-agent-port/agent/transports/anthropic.py)
- [`agent/transports/bedrock.py`](file:///home/eins0fx/development/hermes-agent-port/agent/transports/bedrock.py)
- [`run_agent.py`](file:///home/eins0fx/development/hermes-agent-port/run_agent.py)
- [`hermes_cli/config.py`](file:///home/eins0fx/development/hermes-agent-port/hermes_cli/config.py)
- [`rust/tools/gen_main_provider_retry_goldens.py`](file:///home/eins0fx/development/hermes-agent-port/rust/tools/gen_main_provider_retry_goldens.py)
- [`rust/tools/main-provider-retry-goldens.json`](file:///home/eins0fx/development/hermes-agent-port/rust/tools/main-provider-retry-goldens.json)

---

## 1. Executive Summary & Scope

This audit documents the complete, deterministic retry, failover, credential rotation, and provider fallback contract in the live Python implementation of Hermes Agent. It covers ordinary main-turn execution inside [`agent/conversation_loop.py`](file:///home/eins0fx/development/hermes-agent-port/agent/conversation_loop.py) (primarily lines 3300 through 7500), the underlying classification system in [`agent/error_classifier.py`](file:///home/eins0fx/development/hermes-agent-port/agent/error_classifier.py), streaming worker failure boundary handling in [`agent/chat_completion_helpers.py`](file:///home/eins0fx/development/hermes-agent-port/agent/chat_completion_helpers.py), transport layer response validation across all four transport classes in [`agent/transports/`](file:///home/eins0fx/development/hermes-agent-port/agent/transports/), and runtime management in [`agent/agent_runtime_helpers.py`](file:///home/eins0fx/development/hermes-agent-port/agent/agent_runtime_helpers.py).

Every section explicitly distinguishes behavior **proven by executed Python source code** via the oracle generator [`rust/tools/gen_main_provider_retry_goldens.py`](file:///home/eins0fx/development/hermes-agent-port/rust/tools/gen_main_provider_retry_goldens.py) (producing 157 deterministic golden cases in [`rust/tools/main-provider-retry-goldens.json`](file:///home/eins0fx/development/hermes-agent-port/rust/tools/main-provider-retry-goldens.json)) from behavior **inferred from source inspection and execution traces** of outer control-flow loops.

---

## 2. Production Path Architecture & Two-Layer Retry Topology

The live Python architecture implements a **two-layer nested retry loop**:

```
+-----------------------------------------------------------------------------------------+
| Outer Conversation Loop (agent/conversation_loop.py)                                   |
| - Controls turn-level iteration, retry_count accounting (0..max_retries)                |
| - Manages TurnRetryState (22 recovery guards)                                           |
| - Dispatches classified failover: credential rotation vs fallback vs primary recovery   |
|                                                                                         |
|   +---------------------------------------------------------------------------------+   |
|   | Inner Streaming Worker (agent/chat_completion_helpers.py)                       |   |
|   | - Controls HTTP transport connection & SSE chunk reading                        |   |
|   | - Retries up to HERMES_STREAM_RETRIES (default 2 retries = 3 attempts total)     |   |
|   | - Catches transient network errors before deltas or during mid-tool execution   |   |
|   | - On unrecoverable or exhausted failure: re-raises exception to outer loop      |   |
|   +---------------------------------------------------------------------------------+   |
+-----------------------------------------------------------------------------------------+
```

### 2.1 The Outer Conversation Loop (`conversation_loop.py`)
- **Loop Lifecycle**: Enters `while iteration < max_iterations:`, then `while retry_count < max_retries:`.
- **Attempt Index Accounting**:
  - `retry_count` starts at `0` for the initial request.
  - When an attempt fails with an exception or malformed response, `retry_count` increments (`retry_count += 1`).
  - When fallback activates (or primary transport is rebuilt), `retry_count` is explicitly reset to `0`.
- **TurnRetryState**: Instantiated once per user turn (`state = TurnRetryState()`). Houses 22 distinct one-shot boolean guard flags to prevent recursive failover loops and enforce strict recovery sequencing.

### 2.2 The Inner Streaming Worker (`interruptible_streaming_api_call`)
- **Worker Invocation**: Spawns an asynchronous background thread executing `_stream_worker()` while the main thread coordinates cancellation, user interrupts, and timeouts via `_run_with_stale_detection()`.
- **Internal Reconnect Budget**: Defined by `HERMES_STREAM_RETRIES` environment variable (default: `2`). This grants up to `3` physical connection attempts inside the worker before delegating to the outer conversation loop.
- **Mid-stream Reconnect**: If an error occurs *after* tokens have been sent, the inner worker handles recovery differently depending on whether tool calls or pure text are in flight (detailed in Section 7).

---

## 3. Retry Budgets & Configuration Defaults

All budget figures and configuration parsing behaviors below are **proven by executed source** in `section_retry_budgets_and_config_defaults` of [`rust/tools/gen_main_provider_retry_goldens.py`](file:///home/eins0fx/development/hermes-agent-port/rust/tools/gen_main_provider_retry_goldens.py).

| Configuration Parameter | Location | Default Value | Parsing & Value Constraints |
| :--- | :--- | :--- | :--- |
| `agent._api_max_retries` | `config.yaml` (`agent.api_max_retries`) -> `agent/agent_init.py:2138` | `3` | Parsed as `int(val)`; clamped to `max(val, 1)`. Non-integer or `None` falls back to `3`. Zero or negative integers clamp to `1`. |
| `HERMES_STREAM_RETRIES` | `os.environ` -> `agent/chat_completion_helpers.py:5128` | `2` | Inner stream reconnect retries. Default `2` allows 3 total stream attempts before re-raising. |
| `DEFAULT_EMPTY_RETRY_BUDGET` | `agent/empty_response_guard.py:43` | `3` | Budget for retrying empty responses. Reduced dynamically on high-cost models (`>= $5.00/M tokens` drops budget to 1). Skipped on deterministic empty. |
| `zai_coding_overload_retry_ceiling` | `agent/retry_utils.py:208` | `8` | Ceiling for Z.AI Coding 429 overload errors. Sizes retry loop for 3 short attempts + 4 long backoff tiers (30s, 60s, 90s, 120s) + 1. Loop dynamically elevates `max_retries = max(max_retries, 8)`. |
| `HERMES_STREAM_STALE_GIVEUP` | `os.environ` -> `agent/chat_completion_helpers.py:807` | `5` | Maximum consecutive stream attempts that make no token progress before tripping the circuit breaker. |
| `MAX_COMPRESSION_ATTEMPTS` | `agent/conversation_loop.py:6880` | `3` | Hard ceiling on consecutive context compression attempts per turn before aborting. |
| `MAX_THINKING_PREFILL_RETRIES` | `agent/conversation_loop.py:3490` | `2` | Hard ceiling on thinking prefill rejection retries before disabling thinking prefill. |

---

## 4. Response Validation & Malformed Shapes

Before accepting an HTTP 200 payload as valid, Hermes runs transport-specific response validation via `transport.validate_response(response)`.

### 4.1 Transport Validation Matrix
The table below is **proven by executed source** in `section_response_validation_and_malformed_shapes`:

| Transport Class | Tested Response Shape | Valid? | Fallback / Retry Consequence |
| :--- | :--- | :---: | :--- |
| `ChatCompletionsTransport` | `response is None` | `False` | Triggers malformed response eager fallback (attempt 1). |
| `ChatCompletionsTransport` | `SimpleNamespace(id="resp-1")` (no `choices`) | `False` | Triggers malformed response eager fallback (attempt 1). |
| `ChatCompletionsTransport` | `SimpleNamespace(choices=None)` | `False` | Triggers malformed response eager fallback (attempt 1). |
| `ChatCompletionsTransport` | `SimpleNamespace(choices=[])` (empty list) | `False` | Triggers malformed response eager fallback (attempt 1). |
| `ChatCompletionsTransport` | Valid choice with content or tool calls | `True` | Accepted; proceeds to message extraction. |
| `ResponsesApiTransport` (Codex) | `response is None` | `False` | Eager fallback on attempt 1. |
| `ResponsesApiTransport` (Codex) | `status="failed"`, `output=[]` | `False` | Eager fallback on attempt 1. |
| `ResponsesApiTransport` (Codex) | `status="cancelled"`, `output=[]` | `False` | Eager fallback on attempt 1. |
| `ResponsesApiTransport` (Codex) | `status="incomplete"`, reason="length", `output=[]` | `False` | Eager fallback on attempt 1. |
| `ResponsesApiTransport` (Codex) | `status="incomplete"`, reason="content_filter" | `True` | **Special-cased as Valid**: allows downstream safety refusal handler to capture refusal cleanly instead of treating as malformed wire response. |
| `AnthropicTransport` | `response is None` | `False` | Eager fallback on attempt 1. |
| `AnthropicTransport` | `content=[]`, `stop_reason=None` | `False` | Empty content without stop reason is malformed. |
| `AnthropicTransport` | `content=[]`, `stop_reason="max_tokens"` | `False` | Empty content cut off by length is malformed. |
| `AnthropicTransport` | `content=[]`, `stop_reason="end_turn"` | `True` | Legitimate empty completion from Anthropic. |
| `AnthropicTransport` | `content=[]`, `stop_reason="refusal"` | `True` | Legitimate refusal block; routed to safety refusal handler. |
| `BedrockTransport` | `response is None` or `{}` (empty dict) | `False` | Eager fallback on attempt 1. |
| `BedrockTransport` | `{"output": {"message": {"content": [...]}}}` | `True` | Valid Bedrock native dictionary structure. |

### 4.2 Malformed Response Handling in Conversation Loop
- When `validate_response()` returns `False` on HTTP 200:
  - Outer conversation loop immediately tests `_try_activate_fallback(reason=FailoverReason.format_error)`.
  - If a fallback provider is configured, **fallback activates eagerly on attempt 1**, bypassing any same-provider retries. `retry_count` is reset to `0`.
  - Operator buffer status: `"⚠️ Empty/malformed response -- switching to fallback..."` (punctuation normalized here for repository style).
  - If no fallback is available, it enters same-provider retry with backoff (`5s` base, up to `120s`), terminating after `max_retries` with:
    `"❌ Max retries (3) exceeded for invalid responses. Giving up."`

---

## 5. Error Classification Taxonomy & Reason Matrix

The table below documents how exceptions and HTTP response statuses map into `FailoverReason`, `retryable`, `should_fallback`, and `should_rotate_credential`. All rows are **proven by executed source** in `section_error_classification_taxonomy_matrix`.

| Error Trigger / Exception / Status | Classified `FailoverReason` | `retryable` | `should_fallback` | `should_rotate_credential` | Loop Failover Route |
| :--- | :--- | :---: | :---: | :---: | :--- |
| `httpx.ConnectError` ("Connection refused") | `timeout` | `True` | `True` | `False` | Transport Ladder (Attempt 2) |
| `ConnectionResetError` ("Connection reset by peer") | `timeout` | `True` | `True` | `False` | Transport Ladder (Attempt 2) |
| `BrokenPipeError` ("Broken pipe") | `timeout` | `True` | `True` | `False` | Transport Ladder (Attempt 2) |
| `httpx.ConnectError` ("Name or service not known" / DNS) | `timeout` | `True` | `True` | `False` | Transport Ladder (Attempt 2) |
| `httpx.ReadTimeout` ("Read timed out") | `timeout` | `True` | `True` | `False` | Transport Ladder (Attempt 2) |
| `httpx.ConnectTimeout` ("Connect timed out") | `timeout` | `True` | `True` | `False` | Transport Ladder (Attempt 2) |
| `openai.APITimeoutError` ("Request timed out") | `timeout` | `True` | `True` | `False` | Transport Ladder (Attempt 2) |
| `TimeoutError` ("Operation timed out") | `timeout` | `True` | `True` | `False` | Transport Ladder (Attempt 2) |
| **HTTP 408 Request Timeout** | `timeout` | `True` | `True` | `False` | **Transport Ladder (Attempt 2)**. Explicitly mapped to timeout (RFC 9110 §15.5.9 safe-to-retry reverse proxy timeout), NOT generic 4xx. |
| **HTTP 503 Service Unavailable** | `overloaded` | `True` | `False` | `False` | Transport Ladder (Attempt 2) |
| **HTTP 529 Site Overloaded** (Anthropic) | `overloaded` | `True` | `False` | `False` | Transport Ladder (Attempt 2) |
| **HTTP 429 with Overload Body** ("service is temporarily overloaded") | `overloaded` | `True` | `False` | `False` | Overload Disambiguation. Bypasses credential rate-limit exhaustion, retries on same provider with overload schedule. |
| **HTTP 500 Internal Server Error** | `server_error` | `True` | `False` | `False` | Max-Retry Fallback (Attempt 3) |
| **HTTP 502 Bad Gateway** | `server_error` | `True` | `False` | `False` | Max-Retry Fallback (Attempt 3) |
| **HTTP 504 Gateway Timeout** | `server_error` | `True` | `False` | `False` | Max-Retry Fallback (Attempt 3) |
| **HTTP 520 / 521 / 524** (Cloudflare error hops) | `server_error` | `True` | `False` | `False` | Max-Retry Fallback (Attempt 3) |
| **HTTP 500/502 with Request-Validation pattern** ("unsupported parameter") | `format_error` | `False` | `True` | `False` | Reclassified to Client Error; Eager Fallback (Attempt 1). Deterministic rejection. |
| **HTTP 500/503 with Context Overflow pattern** ("maximum context length") | `context_overflow` | `True` | `False` | `False` | Reclassified to Context Compression; skips server error retries and compresses history. |
| **Generic Unknown Exception** (`RuntimeError("unknown")`) | `unknown` | `True` | `False` | `False` | Max-Retry Fallback (Attempt 3) |
| **HTTP 404 without model pattern** ("404 page not found") | `unknown` | `True` | `False` | `False` | Max-Retry Fallback (Attempt 3) |
| **HTTP 404 with Model Not Found pattern** ("model does not exist") | `model_not_found` | `False` | `True` | `False` | Eager Fallback (Attempt 1) |
| **HTTP 400 Content Policy** ("violates our usage policies") | `content_policy_blocked` | `False` | `True` | `False` | Eager Fallback (Attempt 1) |
| **HTTP 400 Anthropic Safety Filter** ("prompt was flagged by safety system") | `content_policy_blocked` | `False` | `True` | `False` | Eager Fallback (Attempt 1) |
| **Azure Responsible AI Policy** (`ResponsibleAIPolicyViolation`) | `content_policy_blocked` | `False` | `True` | `False` | Eager Fallback (Attempt 1) |
| **MiniMax Stream Safety Filter** ("new_sensitive (1027)") | `content_policy_blocked` | `False` | `True` | `False` | Eager Fallback (Attempt 1) |
| **HTTP 429 Standard Rate Limit** | `rate_limit` | `True` | `True` | `True` | Credential Rotation / Eager Fallback |
| **HTTP 429 Aggregator Upstream Limit** ("provider OpenRouter throttled upstream") | `upstream_rate_limit` | `True` | `True` | `False` | **Eager Model Fallback**. Bypasses pool rotation. |
| **HTTP 402 Payment Required** / Billing Exhaustion | `billing` | `False` | `True` | `True` | Credential Rotation / Eager Fallback |
| **HTTP 429 with Billing Body** ("insufficient credits") | `billing` | `False` | `True` | `True` | Reclassified to Billing; Credential Rotation / Eager Fallback. |
| **HTTP 401 / 403 Authentication Error** | `auth` | `False` | `True` | `True` | Credential Refresh -> Eager Fallback |
| **HTTP 400 Format Error** | `format_error` | `False` | `True` | `False` | Eager Fallback (Attempt 1) |
| **SSL Certificate Verification Failure** (`ssl_cert_verification`) | `ssl_cert_verification`| `False` | `True` | `False` | Eager Fallback (Attempt 1) |
| **Stale Call Circuit Breaker** (`RuntimeError("aborting after 5 stale attempts")`) | `timeout` | `False` | `True` | `False` | Eager Fallback (Attempt 1) |

---

## 6. Fallback Activation Thresholds & Recovery Ladders

Hermes differentiates between three major categories of fallback timing:

```
                          [Error Occurs in Main Turn]
                                       |
                   +-------------------+-------------------+
                   |                                       |
         [Eager Failover Sites]                 [Classified Transport/Server]
         - Malformed HTTP 200                              |
         - HTTP 200 Safety Refusal          +--------------+--------------+
         - Stream Content Filter Stall      |                             |
         - Content Policy Exception    [Transport Failures]         [Server / Unknown]
         - Upstream 429 Throttling     - Timeout                    - 500, 502, 504
         - Unrecoverable 429 / 402     - Overload (503/529)         - Unknown 4xx/5xx
         - Auth Refresh Failed         - ConnectError               - Generic Exception
         - Non-retryable 4xx                |                             |
                   |                   Attempt 1: Retry              Attempt 1: Retry
                   |                   Attempt 2: FALLBACK           Attempt 2: Retry
                   v                   Attempt 3: Primary Recovery   Attempt 3: FALLBACK
           FALLBACK ACTIVATES                                        (Max Retries Exceeded)
          IMMEDIATELY ON ATT 1
```

### 6.1 The Ten Fallback Trigger Sites in Production Code
Traced directly in [`agent/conversation_loop.py`](file:///home/eins0fx/development/hermes-agent-port/agent/conversation_loop.py) and [`agent/chat_completion_helpers.py`](file:///home/eins0fx/development/hermes-agent-port/agent/chat_completion_helpers.py):

1. **Nous Portal Rate Guard Preflight** (`conversation_loop.py:3360`):
   - *Trigger*: `nous_rate_limit_remaining > 0` before making network call.
   - *Attempt*: Attempt `0` (before network call).
   - *Action*: Activates fallback immediately without network call; resets `retry_count = 0`.
2. **Malformed HTTP 200 Response** (`conversation_loop.py:3851`):
   - *Trigger*: `validate_response(response)` returns `False`.
   - *Attempt*: Attempt `1` (immediate failover).
   - *Action*: Activates fallback immediately without same-provider retries; resets `retry_count = 0`.
3. **HTTP 200 Safety Refusal** (`conversation_loop.py:4103`):
   - *Trigger*: `finish_reason == "content_filter"` or `message.refusal`.
   - *Attempt*: Attempt `1`.
   - *Action*: Activates fallback once; terminal if no fallback; resets `retry_count = 0`.
4. **Content Filter Stream Stall** (`conversation_loop.py:4327`):
   - *Trigger*: Partial stream stub with `_content_filter_terminated == True`.
   - *Attempt*: Attempt `1`.
   - *Action*: Rolls back partial assistant messages to last clean turn; activates fallback; resets `retry_count = 0`.
5. **Classified Rate Limit / Billing / Upstream** (`conversation_loop.py:6073`):
   - *Trigger*: `FailoverReason.rate_limit`, `billing`, or `upstream_rate_limit`.
   - *Attempt*: Attempt `1` if pool cannot recover or upstream aggregator; resets `retry_count = 0`.
6. **Classified Transport Failure Ladder** (`conversation_loop.py:6088`):
   - *Trigger*: `FailoverReason.timeout`, `overloaded`, or connection errors.
   - *Attempt*: Attempt `2` (`retry_count >= 2`). Retries attempt 1 on primary; fallback activates at attempt 2; resets `retry_count = 0`.
7. **Auth Failure Escalation** (`conversation_loop.py:6117`):
   - *Trigger*: `classified.is_auth` and credential refresh failed.
   - *Attempt*: Attempt `1`. Activates fallback; resets `retry_count = 0`.
8. **Thinking Prefill Recovery Exhaustion** (`conversation_loop.py:6155`):
   - *Trigger*: Thinking signature / prefill rejected and retry budget exhausted.
   - *Attempt*: Activates fallback; resets `retry_count = 0`.
9. **Non-Retryable Client Error** (`conversation_loop.py:6824`):
   - *Trigger*: HTTP 4xx, `content_policy_blocked` exception, `format_error`, `ssl_cert_verification`.
   - *Attempt*: Attempt `1`. Tries fallback before aborting; resets `retry_count = 0`.
10. **Max Retries Exhausted** (`conversation_loop.py:7348`):
    - *Trigger*: `retry_count >= max_retries` for server errors (500, 502, 504) or unknown errors.
    - *Attempt*: Attempt `3` (with default `max_retries = 3`). Activates fallback; resets `retry_count = 0`.

### 6.2 The Three Threshold Tiers
1. **Tier 1: Eager Fallback (Attempt 1)**:
   - Activates on the very first failure (`retry_count == 1` or `0`).
   - Applies to: malformed HTTP 200, safety refusals, stream content filter stalls, content policy exceptions, unrecoverable 429/402, upstream 429s, auth failures, and non-retryable 4xx client errors.
2. **Tier 2: Transport Ladder (Attempt 2 / `retry_count >= 2`)**:
   - Applies to: connection drops, TCP resets, broken pipes, DNS lookup failures, connect timeouts, read timeouts, HTTP 408, HTTP 503, HTTP 529, and overload 429s.
   - *Attempt 1*: Retries on the same provider with jittered exponential backoff (`retry_count` becomes `1`; fallback is blocked by `retry_count >= 2`).
   - *Attempt 2*: `retry_count` reaches `2`. Fallback activates immediately if an alternate provider is configured.
   - *Attempt 3 (No Fallback Configured)*: `try_recover_primary_transport` executes on direct endpoints (rebuilds client, clears socket pool, resets `retry_count = 0`). Aggregator endpoints skip recovery.
3. **Tier 3: Max-Retry Fallback (Attempt 3 / `retry_count >= max_retries`)**:
   - Applies to: HTTP 500, 502, 504, other 5xx, and unknown generic exceptions.
   - Retries on the same provider for attempts 1 and 2 with exponential backoff.
   - Fallback activates only after exhausting the full retry budget (`retry_count >= 3`).

---

## 7. Credential Rotation vs Provider Fallback Interaction

Credential rotation and provider fallback coordinate through `_pool_may_recover_from_rate_limit(pool)` in [`run_agent.py`](file:///home/eins0fx/development/hermes-agent-port/run_agent.py).

### 7.1 Suppression of Fallback for Credential Pools
When an HTTP 429 / rate limit occurs:
1. `conversation_loop.py` evaluates:
   ```python
   pool_may_recover = False if _is_upstream else _pool_may_recover_from_rate_limit(self.credential_pool)
   should_fallback = classified.should_fallback and (not pool_may_recover)
   ```
2. **Multi-Credential Pool with Available Keys**:
   - `_pool_may_recover_from_rate_limit()` returns `True`.
   - **Eager fallback is suppressed on attempt 1**.
   - Attempt 1: First 429 marks `state.has_retried_429 = True` and retries.
   - Attempt 2: Second 429 triggers `recover_with_credential_pool()`, which rotates to the next available API key in the pool, marks the exhausted key in cooldown, rebuilds the client, and resets `retry_count = 0`.
3. **Single-Credential Pool or All Keys in Cooldown**:
   - `_pool_may_recover_from_rate_limit()` returns `False`.
   - **Eager fallback activates immediately on attempt 1**, bypassing any useless same-key retries.
4. **Upstream Rate Limit Bypass (`FailoverReason.upstream_rate_limit`)**:
   - When an aggregator (e.g., OpenRouter) reports that the *underlying model provider* is throttled (rather than the user's account key), `_is_upstream` is `True`.
   - `pool_may_recover` is forced to `False`.
   - Hermes **bypasses credential rotation entirely** and activates provider/model fallback immediately, preserving valid pool credentials.

---

## 8. Stream Failure Boundaries & Partial Turn Recovery

Stream failures are partitioned at the exact boundary where tokens have or have not been delivered to the user or tool parser.

```
                              [Streaming Failure]
                                       |
                   +-------------------+-------------------+
                   |                                       |
          [Before Any Deltas]                     [After Deltas Delivered]
                   |                                       |
      Inner worker retries up to              +------------+------------+
        HERMES_STREAM_RETRIES (2)             |                         |
                   |                  [Tool Call In-Flight]     [Pure Text Generated]
      If still failing:                       |                         |
      Re-raises exception to           Silent reconnect in        Does NOT re-raise.
      outer conversation loop          stream worker; emits       Returns partial stub
                   |                   warning delta banner:      with finish_reason='length'.
      Enters standard classified       "⚠ Connection dropped..."  Outer loop appends text
      failover ladder                  Clears partial tool state  and sends continuation
                                       and continues stream       nudge to complete turn.
```

### 8.1 Failure Before Any Deltas
- **Inner Worker Behavior**: Retries transient network failures up to `HERMES_STREAM_RETRIES` (default `2` retries, `3` attempts total).
- **Outer Loop Behavior**: If all stream worker attempts fail, the exception (`ReadTimeout`, `ConnectError`, etc.) is re-raised to `conversation_loop.py`.
- **Classification**: Caught by `except Exception as api_error:`, classified via `classify_api_error(api_error)`.
- **Outcome**: Follows standard transport ladder (retry on attempt 1; fallback on attempt 2).

### 8.2 Failure After Deltas with In-Flight Tool Call
- **Detection**: `deltas_were_sent is True` and `tool_call_in_flight is True` (unclosed tool call argument buffer).
- **Inner Worker Action**: The streaming worker catches the transient disconnect, resets tool delivery tracking via `_reset_stream_delivery_tracking()`, emits an inline warning delta:
  `"\n\n⚠ Connection dropped mid tool-call; reconnecting…\n\n"`
  and reconnects silently without bubbling to the outer conversation loop.

### 8.3 Failure After Deltas with Pure Text (No Tool Calls)
- **Detection**: `deltas_were_sent is True` and `tool_call_in_flight is False`.
- **Inner Worker Action**: The worker does **not** re-raise. It synthesizes a partial stream stub:
  - `stub.id = "partial_stream_stub"`
  - `stub.choices[0].finish_reason = "length"`
- **Outer Loop Action**: `conversation_loop.py` handles the response under `finish_reason == "length"`, appends the partial assistant response to history, and sends a continuation nudge to prompt the model to complete the remaining text.

### 8.4 Stream Content-Filter Stall (`_content_filter_terminated`)
- **Detection**: Stream terminates prematurely with safety filter patterns (e.g. MiniMax `new_sensitive (1027)` or Azure content filter). The partial stub is tagged `response._content_filter_terminated = True`.
- **Outer Loop Action** (`conversation_loop.py:4313-4335`):
  - **Message Rollback**: Rolls back assistant messages in history to the last clean turn, stripping the filtered partial tokens.
  - **Fallback Switch**: Activates fallback provider immediately.
  - **Counter Reset**: Resets `retry_count = 0`.
  - **Operator Notice**: If fallback available: `"Content filter terminated stream; switching to fallback..."`. If no fallback configured: `"⚠️ No fallback provider configured -- retrying with same provider (may re-hit filter)..."` (punctuation normalized here for repository style).

---

## 9. Retry Resets, Stickiness, and Turn Boundaries

### 9.1 Retry Counter Resets
In all **10 fallback trigger sites** identified in Section 6.1, `conversation_loop.py` executes:
```python
retry_count = 0
```
Fallback grants the new provider a **completely fresh retry budget** (`0..max_retries`). The failed attempts of the primary provider do not count against the fallback provider. Similarly, `try_recover_primary_transport()` resets `retry_count = 0`.

### 9.2 Within-Turn Fallback Stickiness
- When `_try_activate_fallback()` succeeds, it mutates the agent in-place:
  - `agent.model = fb_model`
  - `agent.provider = fb_provider`
  - `agent.client = fb_client`
  - `agent._fallback_activated = True`
- In multi-step turns involving tool calls (assistant calls tool -> tool executes -> assistant interprets result):
  - **Fallback is strictly sticky**.
  - Subsequent iterations within the same turn continue using `agent.model` and `agent.provider`.
  - Primary runtime is **never** restored mid-turn between tool execution iterations.

### 9.3 Cross-Turn Primary Restoration
- In long-lived interactive CLI and gateway sessions, a single agent instance spans multiple turns.
- At the top of `run_conversation()` (the entry point for each new user turn), Hermes calls:
  [`restore_primary_runtime(agent)`](file:///home/eins0fx/development/hermes-agent-port/agent/agent_runtime_helpers.py#L1641)
- **Restoration Gates**:
  1. If `_rate_limited_until > time.monotonic()`: primary provider is still in cooldown; restoration is skipped, and session remains on fallback.
  2. If primary credential pool indicates all keys are in reset cooldown: restoration is skipped.
  3. If cooldown has expired: restores `agent.model`, `agent.provider`, `agent.client`, and client settings from `agent._primary_runtime`, resets `agent._fallback_activated = False`, and resets `agent._fallback_index = 0`.
  4. Console confirmation: `"✅ Primary model restored: {model} via {provider}; fallback {fb_model} via {fb_provider} is no longer active."`.

---

## 10. Operator-Visible Status Text & Terminal Return Structures

Status messages emitted directly by the production code path:

### 10.1 `_fallback_reason_text()` Explanations
Executed in [`agent/chat_completion_helpers.py:2669`](file:///home/eins0fx/development/hermes-agent-port/agent/chat_completion_helpers.py#L2669):
- `rate_limit`: `"rate limit"`
- `billing`: `"billing or quota exhausted"`
- `upstream_rate_limit`: `"upstream model rate limit"`
- `overloaded`: `"provider overloaded"`
- `timeout`: `"request timeout"`
- `content_policy_blocked`: `"content policy blocked the request"`
- `auth`: `"authentication failed"`
- Notice banner:
  `"⚠️ Model fallback: {primary_model} via {primary_provider} unavailable ({reason_text}); using {fallback_model} via {fallback_provider}."`

### 10.2 Live Production Status Strings
Directly emitted to status callback / terminal buffer:
- Eager Fallback Rate Limit: `"⚠️ Rate limited -- switching to fallback provider..."`
- Eager Fallback Billing Verified: `"⚠️ Billing or credits exhausted -- switching to fallback provider..."`
- Eager Fallback Billing Unverified: `"⚠️ Provider reported usage/credit exhaustion (unverified -- may be a content-filter rejection) -- switching to fallback provider..."`
- Eager Fallback Transport: `"⚠️ Provider unreachable -- switching to fallback provider..."`
- Eager Fallback Upstream: `"⚠️ Upstream aggregator rate-limited -- switching to fallback model..."`
- Eager Fallback Auth: `"🔐 Authentication failed and could not be refreshed -- switching to fallback provider..."`
- Eager Fallback Malformed: `"⚠️ Empty/malformed response -- switching to fallback..."`
- Eager Fallback Safety Refusal: `"⚠️ Model declined to respond (safety refusal) -- trying fallback..."`
- Max Retries Exhausted Fallback Available: `"⚠️ Max retries ({max_retries}) exhausted -- trying fallback..."`
- Stream Content Filter Stall: `"Content filter terminated stream; switching to fallback..."`
- Terminal API Failed: `"❌ API failed after {max_retries} retries -- {summary}"`
- Terminal Rate Limited: `"❌ Rate limited after {max_retries} retries -- {summary}"`
- Terminal Billing: `"❌ Billing or credits exhausted -- {summary}"`
- Terminal Invalid Responses: `"❌ Max retries ({max_retries}) exceeded for invalid responses. Giving up."`
- Terminal Safety Refusal: `"⚠️ The model declined to respond to this request (safety refusal)."`

### 10.3 Terminal Failure Return Dictionary Structure
When all retries and fallbacks are exhausted, `conversation_loop.py` returns:
```python
{
    "final_response": "API call failed after {max_retries} retries: {summary}",
    "messages": messages,
    "api_calls": total_api_calls,
    "completed": False,
    "failed": True,
    "error": summary,
    "failure_reason": reason.value,
    "failure_retryable": False,
    "billing_unverified": False,
    "billing_block": None,
}
```

---

## 11. TurnRetryState Guards Contract

[`agent/turn_retry_state.py`](file:///home/eins0fx/development/hermes-agent-port/agent/turn_retry_state.py) implements 22 one-shot recovery guards. All default to `False` and are reset at turn start:

1. `primary_recovery_attempted`: Rebuilds client once on direct endpoint after transport retries exhaust.
2. `has_retried_429`: Marks that a 429 has occurred; allows second 429 to trigger credential rotation.
3. `auth_failover_attempted`: Prevents recursive auth refresh loops.
4. `restart_with_rebuilt_messages`: Forces fresh API call after context truncation or system prompt change.
5. `rebuilt_fresh_attempt_consumed`: Guards message rebuild budget.
6. `empty_response_fallback_attempted`: Ensures malformed HTTP 200 fallback triggers only once per response shape.
7. `content_filter_fallback_attempted`: Prevents looping safety refusal fallbacks.
8. `refusal_fallback_attempted`: Guards HTTP 200 `message.refusal` failover.
9. `stream_content_filter_fallback_attempted`: Guards mid-stream content filter stall fallback.
10. `stream_reset_attempted`: Guards TCP connection reset recovery in streaming.
11. `stream_timeout_fallback_attempted`: Guards streaming read timeout failover.
12. `stream_stall_circuit_broken`: Latches when 5 consecutive stream attempts fail to yield tokens.
13. `stream_partial_length_continuation_attempted`: Guards continuation prompts on partial stream stubs.
14. `stream_tool_disconnect_reconnected`: Marks successful mid-stream tool disconnect recovery.
15. `transport_fallback_attempted`: Latches when transport ladder activates fallback at attempt 2.
16. `max_retries_fallback_attempted`: Latches when server error fallback activates at attempt 3.
17. `rate_limit_fallback_attempted`: Guards 429 fallback activation.
18. `billing_fallback_attempted`: Guards 402/billing fallback activation.
19. `upstream_rate_limit_fallback_attempted`: Guards upstream aggregator fallback activation.
20. `format_error_fallback_attempted`: Guards 400 Bad Request fallback activation.
21. `context_compression_attempted`: Latches after history compression to prevent compression loops.
22. `thinking_prefill_retry_attempted`: Guards retry when model rejects thinking prefill signatures.

---

## 12. Executed Source vs Inferred Inspection Provenance Catalog

The table below catalogs the 157 deterministic test cases in [`rust/tools/main-provider-retry-goldens.json`](file:///home/eins0fx/development/hermes-agent-port/rust/tools/main-provider-retry-goldens.json), detailing the execution mechanism and exact verification status:

| Section | Cases | Provenance Methodology | Source Modules Executed |
| :--- | :---: | :--- | :--- |
| **1. Retry Budgets & Config Defaults** | 12 | **Executed Source** | `agent.agent_init` config parser, `agent.empty_response_guard.DEFAULT_EMPTY_RETRY_BUDGET`, `agent.retry_utils.zai_coding_overload_retry_ceiling`, `utils.env_int`. |
| **2. Error Classification Taxonomy Matrix** | 35 | **Executed Source** | Live execution of `agent.error_classifier.classify_api_error` against instantiated HTTP exceptions, status errors, and bodies. |
| **3. Response Validation & Malformed Shapes** | 18 | **Executed Source** | Live execution of `validate_response()` across `ChatCompletionsTransport`, `ResponsesApiTransport`, `AnthropicTransport`, and `BedrockTransport`. |
| **4. Eager Fallback Decisions & Predicates** | 20 | **Executed Source & Trace** | Live execution of failover reason routing predicates, plus verified trace mapping of eager fallback sites in `conversation_loop.py`. |
| **5. Transport Retry & Fallback Thresholds** | 9 | **Executed Source & Trace** | Live execution of `agent.agent_runtime_helpers.try_recover_primary_transport` for direct vs aggregator endpoints, plus progression ladder trace. |
| **6. Credential Rotation vs Fallback** | 5 | **Executed Source** | Live execution of `run_agent._pool_may_recover_from_rate_limit` across None, single-key, multi-key available, and multi-key cooling down pools. |
| **7. Stream Failure Boundaries** | 4 | **Source Inspection & Trace** | Comprehensive control-flow trace of `interruptible_streaming_api_call` and `_stream_worker` before/after deltas, tool-call state, and content filter termination. |
| **8. Retry Resets & Fallback Stickiness** | 3 | **Executed Source & Trace** | Live execution of `agent._try_activate_fallback` and `agent._restore_primary_runtime`, verifying model mutation, counter resets, and cooldown gates. |
| **9. Operator Status & Terminal Structures** | 16 | **Executed Source & Trace** | Live execution of `_fallback_reason_text`, literal verification of production status banners in `conversation_loop.py`, and terminal dictionary schema. |
| **10. TurnRetryState Guards Contract** | 23 | **Executed Source** | Live inspection and mutation of `agent.turn_retry_state.TurnRetryState` dataclass fields and default values. |
| **11. Executed vs Inferred Catalog** | 12 | **Executed Source** | Meta-catalog mapping all contract sections to execution status and source files. |
| **Total** | **157** | **Deterministic Suite** | Fully automated, reproducible via `.venv/bin/python3 rust/tools/gen_main_provider_retry_goldens.py --check`. |

---

## 13. Golden Generator Command & Validation

To generate and verify the golden dataset:
```bash
# Generate corpus:
.venv/bin/python3 rust/tools/gen_main_provider_retry_goldens.py

# Verify byte-for-byte parity:
.venv/bin/python3 rust/tools/gen_main_provider_retry_goldens.py --check
```
Output:
```
OK: verified byte-for-byte parity across 11 sections and 157 test cases
```
