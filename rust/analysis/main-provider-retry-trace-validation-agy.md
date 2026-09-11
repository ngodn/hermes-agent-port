# Main Provider Retry and Operator Notice Trace Validation Report

**Document Target**: [`rust/analysis/main-provider-retry-trace-validation-agy.md`](main-provider-retry-trace-validation-agy.md)
**Evidence Lane**: Live Python Main-Turn Runtime Oracle & Retry Trace Validation
**Primary Source References**:
- [`run_agent.py`](../../run_agent.py) (lines 1083--1115, 1245--1344, 7649--7665)
- [`agent/conversation_loop.py`](../../agent/conversation_loop.py) (lines 3328--3345, 5758--5790, 6020--6091, 6768--6860, 7017--7080, 7259--7286, 8805--8806)
- [`agent/chat_completion_helpers.py`](../../agent/chat_completion_helpers.py) (lines 2669--2703, 2705--2760, 3152--3166)
- [`agent/error_classifier.py`](../../agent/error_classifier.py) (lines 498--507, 1460--1540)
- [`tests/run_agent/test_retry_status_buffer.py`](../../tests/run_agent/test_retry_status_buffer.py)
- [`tests/run_agent/test_conversation_fallback_state.py`](../../tests/run_agent/test_conversation_fallback_state.py)
- [`tests/run_agent/test_92450_outer_error_retry_bound.py`](../../tests/run_agent/test_92450_outer_error_retry_bound.py)

---

## 1. Executive Summary & Scope

This validation report documents the live Python runtime execution traces for the Rust main-provider retry-notice checkpoint. All four required ordinary `chat_completions` scenarios were executed against the actual Python [`AIAgent`](../../run_agent.py#L9258) conversation loop ([`agent.conversation_loop.run_conversation`](../../agent/conversation_loop.py#L2026)) with patched provider wire I/O and zero-duration waits:

1. **Case 1: HTTP 500 then success** (transient retry, silent buffer clearing, zero operator emission)
2. **Case 2: HTTP 500 terminal exhaustion with no fallback** (3 primary attempts, complete FIFO buffer flush, terminal API failure status)
3. **Case 3: Ordinary HTTP 503 overload through one fallback and terminal exhaustion** (2 primary attempts, eager failover, 3 fallback attempts, 5 calls total, complete FIFO buffer flush, terminal failure status)
4. **Case 4: HTTP 500 request-format rejection through one fallback and terminal exhaustion** (1 primary attempt, client error failover, 1 fallback attempt, 2 calls total, complete FIFO buffer flush, terminal non-retryable error status)

Every event, buffer mutation, channel dispatch, retry numerator, and return payload below reflects **real executed Python runtime behavior** driven through current loop methods and patch boundaries, not a handwritten simulation.

---

## 2. Validation Harness & Test Boundary Architecture

### 2.1 Harness Configuration & Pre-requisite Test Verification
The validation harness operates in the project virtual environment (`Python 3.11.15`, `pytest-9.1.1`). Before running the live traces, focused existing test suites were executed to verify loop and buffer harness integrity:
- `tests/run_agent/test_retry_status_buffer.py`: 11 passed in 0.89s
- `tests/run_agent/test_conversation_fallback_state.py`: 2 passed in 1.20s
- `tests/run_agent/test_92450_outer_error_retry_bound.py`: 4 passed in 1.64s

### 2.2 Patch Boundaries and Invariants
- **Loop Invocation**: Direct execution of [`agent.run_conversation("test prompt")`](../../run_agent.py#L9258), routing into [`agent.conversation_loop.run_conversation`](../../agent/conversation_loop.py#L2026).
- **Transport Mode**: Explicitly set to `api_mode = "chat_completions"`, with `provider = "openrouter"`, `model = "meta-llama/llama-3-70b-instruct"`, and `base_url = "https://openrouter.ai/api/v1"`.
- **Wire I/O Boundary**: Primary client requests are dispatched via `agent.client.chat.completions.create`. Fallback route activations construct the secondary client via [`agent.auxiliary_client.resolve_provider_client`](../../agent/chat_completion_helpers.py#L2811), returning a dedicated fallback mock client (`novita` / `deepseek/deepseek-chat` at `https://api.novita.ai/v1`).
- **Wait Elimination**: Zero-duration backoff is enforced using repository standard test monkeypatching:
  - `jittered_backoff = lambda *a, **k: 0.0` in both `run_agent` and `agent.conversation_loop`.
  - `time.sleep = lambda *a, **k: None` in both modules.
- **Side Effect Neutralization**: `_persist_session`, `_save_trajectory`, and `_cleanup_task_resources` were stubbed to prevent unneeded filesystem and SQLite writes.
- **Instrumentation**: Wrapped hooks recorded all calls to:
  - `agent._buffer_status(msg)`
  - `agent._buffer_vprint(msg)`
  - `agent._clear_status_buffer()`
  - `agent._flush_status_buffer()`
  - `agent._emit_pending_fallback_notice()`
  - `agent._emit_status(msg)`
  - `agent._vprint(msg, force=...)`

---

## 3. Case 1: HTTP 500 Then Success

### 3.1 Trace Profile & Execution Dynamics
- **Initial State**: `retry_count = 0`, `max_retries = 3`, `_fallback_chain = []`.
- **Attempt 1 (Primary Route)**:
  - Call 1 returns `openai.InternalServerError: HTTP 500: Internal Server Error`.
  - Classified by [`classify_api_error`](../../agent/error_classifier.py#L815): `FailoverReason.server_error`, `retryable = True`, `should_fallback = False`.
  - Loop increments `retry_count` at [`agent/conversation_loop.py:5758`](../../agent/conversation_loop.py#L5758): `retry_count = 1`.
  - Buffers 5 diagnostic lines via `_buffer_vprint` with numerator `(attempt 1/3)`.
  - Line [`agent/conversation_loop.py:7285`](../../agent/conversation_loop.py#L7285) buffers retry countdown via `_buffer_status`: `⏳ Retrying in 0.0s (attempt 1/3)...`.
  - Backoff wait collapses (`0.0s`).
- **Attempt 2 (Primary Route)**:
  - Call 2 returns valid HTTP 200 assistant response: `"Recovered response"`.
  - Loop reaches the recovery success boundary at [`agent/conversation_loop.py:8805-8806`](../../agent/conversation_loop.py#L8805-L8806):
    1. [`agent._emit_pending_fallback_notice()`](../../run_agent.py#L1285) evaluates `_pending_fallback_notice` (is `None`; emits 0 messages).
    2. [`agent._clear_status_buffer()`](../../run_agent.py#L1276) clears `agent._retry_status_buffer` completely.

### 3.2 Exact Ordered Records

#### Status Records Buffered (`_retry_status_buffer`)
```python
[
    ("vprint", "⚠️  API call failed (attempt 1/3): InternalServerError [HTTP 500]"),
    ("vprint", "   🔌 Provider: openrouter  Model: meta-llama/llama-3-70b-instruct"),
    ("vprint", "   🌐 Endpoint: https://openrouter.ai/api/v1"),
    ("vprint", "   📝 Error: HTTP 500: Internal Server Error"),
    ("vprint", "   ⏱️  Elapsed: 0.04s  Context: 2 msgs, ~477 tokens"),
    ("status", "⏳ Retrying in 0.0s (attempt 1/3)..."),
]
```

#### Buffer Clearing Event
- `_clear_status_buffer()` called with 6 buffered records in `_retry_status_buffer`.
- Result: `_retry_status_buffer` cleared to `[]`.
- No records flushed.

#### Outward Emissions to Operator
- Exact messages emitted: **0**.
- Complete silence on recovery: all transient retry chatter was silently dropped.

### 3.3 Provider Call Counts & Terminal State
| Route | Provider | Model | Calls |
| :--- | :--- | :--- | :---: |
| Primary Route | `openrouter` | `meta-llama/llama-3-70b-instruct` | 2 |
| Fallback Route | N/A | None configured | 0 |
| **Total Provider Calls** | | | **2** |

- **Terminal Dictionary**:
  ```python
  {
      "final_response": "Recovered response",
      "completed": True,
      "failed": False,
      "messages": [...],
      "api_calls": 2,
  }
  ```
- **Final Attributes**: `_retry_status_buffer == []`, `_pending_fallback_notice is None`.

---

## 4. Case 2: HTTP 500 Terminal Exhaustion with No Fallback

### 4.1 Trace Profile & Execution Dynamics
- **Initial State**: `retry_count = 0`, `max_retries = 3`, `_fallback_chain = []`.
- **Attempt 1 (Primary Route)**:
  - Call 1 fails with HTTP 500.
  - `retry_count` increment: 0 -> 1 (`attempt 1/3`).
  - Buffers 5 `vprint` lines + 1 `status` line (`⏳ Retrying in 0.0s (attempt 1/3)...`).
- **Attempt 2 (Primary Route)**:
  - Call 2 fails with HTTP 500.
  - `retry_count` increment: 1 -> 2 (`attempt 2/3`).
  - Buffers 5 `vprint` lines + 1 `status` line (`⏳ Retrying in 0.0s (attempt 2/3)...`).
- **Attempt 3 (Primary Route)**:
  - Call 3 fails with HTTP 500.
  - `retry_count` increment: 2 -> 3 (`attempt 3/3`).
  - Buffers 5 `vprint` lines with numerator `(attempt 3/3)`.
  - Condition `retry_count >= max_retries` (3 >= 3) evaluates to `True` at [`agent/conversation_loop.py:7017`](../../agent/conversation_loop.py#L7017).
  - Primary transport recovery ([`agent._try_recover_primary_transport`](../../agent/agent_runtime_helpers.py#L1437)) returns `False` (`InternalServerError` is not in `_TRANSIENT_TRANSPORT_ERRORS`).
  - Fallback check ([`agent._has_pending_fallback()`](../../run_agent.py#L7654)) returns `False`.
  - Terminal boundary entered at [`agent/conversation_loop.py:7047`](../../agent/conversation_loop.py#L7047):
    1. [`agent._flush_status_buffer()`](../../run_agent.py#L1315) drains all 17 buffered items and replays them in FIFO order.
    2. [`agent._emit_status()`](../../run_agent.py#L1083) emits terminal line:
       `"❌ API failed after 3 retries \u2014 HTTP 500: Internal Server Error"`
    3. Final error line logged:
       `"   💀 Final error: HTTP 500: Internal Server Error"`

### 4.2 Exact Ordered Records

#### Status Records Buffered (17 items total)
```python
[
    # --- Attempt 1 Fail (Primary) ---
    ("vprint", "⚠️  API call failed (attempt 1/3): InternalServerError [HTTP 500]"),
    ("vprint", "   🔌 Provider: openrouter  Model: meta-llama/llama-3-70b-instruct"),
    ("vprint", "   🌐 Endpoint: https://openrouter.ai/api/v1"),
    ("vprint", "   📝 Error: HTTP 500: Internal Server Error"),
    ("vprint", "   ⏱️  Elapsed: 0.00s  Context: 2 msgs, ~477 tokens"),
    ("status", "⏳ Retrying in 0.0s (attempt 1/3)..."),
    # --- Attempt 2 Fail (Primary) ---
    ("vprint", "⚠️  API call failed (attempt 2/3): InternalServerError [HTTP 500]"),
    ("vprint", "   🔌 Provider: openrouter  Model: meta-llama/llama-3-70b-instruct"),
    ("vprint", "   🌐 Endpoint: https://openrouter.ai/api/v1"),
    ("vprint", "   📝 Error: HTTP 500: Internal Server Error"),
    ("vprint", "   ⏱️  Elapsed: 0.01s  Context: 2 msgs, ~477 tokens"),
    ("status", "⏳ Retrying in 0.0s (attempt 2/3)..."),
    # --- Attempt 3 Fail (Primary) ---
    ("vprint", "⚠️  API call failed (attempt 3/3): InternalServerError [HTTP 500]"),
    ("vprint", "   🔌 Provider: openrouter  Model: meta-llama/llama-3-70b-instruct"),
    ("vprint", "   🌐 Endpoint: https://openrouter.ai/api/v1"),
    ("vprint", "   📝 Error: HTTP 500: Internal Server Error"),
    ("vprint", "   ⏱️  Elapsed: 0.01s  Context: 2 msgs, ~477 tokens"),
]
```

#### Flushed & Emitted Records
On terminal failure, `_flush_status_buffer()` immediately drains the 17 buffered records. Each record is dispatched to its designated channel (`status` via `_emit_status`, `vprint` via `_vprint(force=True)`):
1. `vprint`: `⚠️  API call failed (attempt 1/3): InternalServerError [HTTP 500]`
2. `vprint`: `   🔌 Provider: openrouter  Model: meta-llama/llama-3-70b-instruct`
3. `vprint`: `   🌐 Endpoint: https://openrouter.ai/api/v1`
4. `vprint`: `   📝 Error: HTTP 500: Internal Server Error`
5. `vprint`: `   ⏱️  Elapsed: 0.00s  Context: 2 msgs, ~477 tokens`
6. `status`: `⏳ Retrying in 0.0s (attempt 1/3)...`
7. `vprint`: `⚠️  API call failed (attempt 2/3): InternalServerError [HTTP 500]`
8. `vprint`: `   🔌 Provider: openrouter  Model: meta-llama/llama-3-70b-instruct`
9. `vprint`: `   🌐 Endpoint: https://openrouter.ai/api/v1`
10. `vprint`: `   📝 Error: HTTP 500: Internal Server Error`
11. `vprint`: `   ⏱️  Elapsed: 0.01s  Context: 2 msgs, ~477 tokens`
12. `status`: `⏳ Retrying in 0.0s (attempt 2/3)...`
13. `vprint`: `⚠️  API call failed (attempt 3/3): InternalServerError [HTTP 500]`
14. `vprint`: `   🔌 Provider: openrouter  Model: meta-llama/llama-3-70b-instruct`
15. `vprint`: `   🌐 Endpoint: https://openrouter.ai/api/v1`
16. `vprint`: `   📝 Error: HTTP 500: Internal Server Error`
17. `vprint`: `   ⏱️  Elapsed: 0.01s  Context: 2 msgs, ~477 tokens`
18. `status` (Terminal Status via `_emit_status`):
    `❌ API failed after 3 retries \u2014 HTTP 500: Internal Server Error`
19. `vprint` (Final error):
    `   💀 Final error: HTTP 500: Internal Server Error`

### 4.3 Provider Call Counts & Terminal State
| Route | Provider | Model | Calls |
| :--- | :--- | :--- | :---: |
| Primary Route | `openrouter` | `meta-llama/llama-3-70b-instruct` | 3 |
| Fallback Route | N/A | None configured | 0 |
| **Total Provider Calls** | | | **3** |

- **Terminal Dictionary**:
  ```python
  {
      "final_response": "API call failed after 3 retries: HTTP 500: Internal Server Error",
      "completed": False,
      "failed": True,
      "error": "HTTP 500: Internal Server Error",
      "failure_reason": "server_error",
      "failure_retryable": True,
      "billing_unverified": False,
      "billing_block": None,
      "api_calls": 3,
  }
  ```

---

## 5. Case 3: Ordinary HTTP 503 Overload Through One Fallback and Terminal Exhaustion

### 5.1 Trace Profile & Execution Dynamics
- **Initial State**: `retry_count = 0`, `max_retries = 3`.
- **Fallback Configuration**: Single entry `[{"provider": "novita", "model": "deepseek/deepseek-chat", "base_url": "https://api.novita.ai/v1"}]`.
- **Attempt 1 (Primary Route)**:
  - Call 1 returns HTTP 503 `APIStatusError: Service Unavailable`.
  - Classified by [`classify_api_error`](../../agent/error_classifier.py#L1515): `FailoverReason.overloaded, retryable = True`.
  - `retry_count` increment: 0 -> 1 (`attempt 1/3`).
  - Eager fallback check: `_is_transport_failure and retry_count >= 2` evaluates to `False` (1 >= 2 is False).
  - Buffers 5 `vprint` lines + 1 `status` line (`⏳ Retrying in 0.0s (attempt 1/3)...`).
- **Attempt 2 (Primary Route)**:
  - Call 2 returns HTTP 503.
  - `retry_count` increment: 1 -> 2 (`attempt 2/3`).
  - Buffers 5 `vprint` lines (`attempt 2/3`).
  - Eager transport fallback gate at [`agent/conversation_loop.py:6038`](../../agent/conversation_loop.py#L6038):
    `_is_transport_failure and retry_count >= 2` evaluates to `True` (2 >= 2 is True).
  - Fallback check: `agent._fallback_index < len(agent._fallback_chain)` evaluates to `True` (0 < 1).
  - Eager fallback triggers ([`agent/conversation_loop.py:6078-6083`](../../agent/conversation_loop.py#L6078-L6083)):
    1. Line 6078 buffers pre-switch notice:
       `("status", "⚠️ Provider unreachable \u2014 switching to fallback provider...")`
    2. [`agent._try_activate_fallback(reason=classified.reason)`](../../agent/chat_completion_helpers.py#L2705) is invoked with `reason = FailoverReason.overloaded`.
    3. `_fallback_reason_text(FailoverReason.overloaded)` returns `"provider overloaded"`.
    4. Line 3154 formats switch notice:
       `"⚠️ Model fallback: meta-llama/llama-3-70b-instruct via openrouter unavailable (provider overloaded); using deepseek/deepseek-chat via novita."`
    5. Line 3159 buffers model-switch notice into `_retry_status_buffer`.
    6. Line 3166 appends notice to durable registry `agent._pending_fallback_notice`.
    7. Client replaced with `novita` fallback mock client, provider updated to `novita`, model to `deepseek/deepseek-chat`.
    8. Line 6086 resets `retry_count = 0`.
    9. `break` exits inner retry loop to re-issue request on the new fallback provider.
- **Attempt 1 (Fallback Route)**:
  - Call 3 returns HTTP 503 on `novita`.
  - `retry_count` increment: 0 -> 1 (`attempt 1/3`).
  - `retry_count >= 2` is False (1 >= 2 is False).
  - Buffers 5 `vprint` lines + 1 `status` line (`⏳ Retrying in 0.0s (attempt 1/3)...`).
- **Attempt 2 (Fallback Route)**:
  - Call 4 returns HTTP 503 on `novita`.
  - `retry_count` increment: 1 -> 2 (`attempt 2/3`).
  - `retry_count >= 2` is True, but `agent._fallback_index < len(agent._fallback_chain)` is `False` (1 < 1 is False; chain exhausted).
  - Buffers 5 `vprint` lines + 1 `status` line (`⏳ Retrying in 0.0s (attempt 2/3)...`).
- **Attempt 3 (Fallback Route)**:
  - Call 5 returns HTTP 503 on `novita`.
  - `retry_count` increment: 2 -> 3 (`attempt 3/3`).
  - Buffers 5 `vprint` lines (`attempt 3/3`).
  - Condition `retry_count >= max_retries` (3 >= 3) evaluates to `True` at [`agent/conversation_loop.py:7017`](../../agent/conversation_loop.py#L7017).
  - Primary transport recovery skipped (fallback active).
  - Fallback check `agent._has_pending_fallback()` returns `False`.
  - Terminal boundary entered at [`agent/conversation_loop.py:7047`](../../agent/conversation_loop.py#L7047):
    1. [`agent._flush_status_buffer()`](../../run_agent.py#L1315) runs. Line 1325 sets `self._pending_fallback_notice = None` to discard the separate copy, preventing duplicate emissions.
    2. Drains and emits all 30 buffered records in chronological FIFO order.
    3. Line 7077 emits terminal status:
       `"❌ API failed after 3 retries \u2014 HTTP 503: Service Unavailable"`
    4. Line 7078 logs final error:
       `"   💀 Final error: HTTP 503: Service Unavailable"`

### 5.2 Exact Ordered Records

#### Status Records Buffered (30 items total)
```python
[
    # --- Attempt 1 Fail (Primary: openrouter) ---
    ("vprint", "⚠️  API call failed (attempt 1/3): APIStatusError [HTTP 503]"),
    ("vprint", "   🔌 Provider: openrouter  Model: meta-llama/llama-3-70b-instruct"),
    ("vprint", "   🌐 Endpoint: https://openrouter.ai/api/v1"),
    ("vprint", "   📝 Error: HTTP 503: Service Unavailable"),
    ("vprint", "   ⏱️  Elapsed: 0.00s  Context: 2 msgs, ~477 tokens"),
    ("status", "⏳ Retrying in 0.0s (attempt 1/3)..."),

    # --- Attempt 2 Fail (Primary: openrouter) ---
    ("vprint", "⚠️  API call failed (attempt 2/3): APIStatusError [HTTP 503]"),
    ("vprint", "   🔌 Provider: openrouter  Model: meta-llama/llama-3-70b-instruct"),
    ("vprint", "   🌐 Endpoint: https://openrouter.ai/api/v1"),
    ("vprint", "   📝 Error: HTTP 503: Service Unavailable"),
    ("vprint", "   ⏱️  Elapsed: 0.00s  Context: 2 msgs, ~477 tokens"),

    # --- Eager Failover Transition ---
    ("status", "⚠️ Provider unreachable \u2014 switching to fallback provider..."),
    ("status", "⚠️ Model fallback: meta-llama/llama-3-70b-instruct via openrouter unavailable (provider overloaded); using deepseek/deepseek-chat via novita."),

    # --- Attempt 1 Fail (Fallback: novita) ---
    ("vprint", "⚠️  API call failed (attempt 1/3): APIStatusError [HTTP 503]"),
    ("vprint", "   🔌 Provider: novita  Model: deepseek/deepseek-chat"),
    ("vprint", "   🌐 Endpoint: https://api.novita.ai/v1"),
    ("vprint", "   📝 Error: HTTP 503: Service Unavailable"),
    ("vprint", "   ⏱️  Elapsed: 0.00s  Context: 2 msgs, ~473 tokens"),
    ("status", "⏳ Retrying in 0.0s (attempt 1/3)..."),

    # --- Attempt 2 Fail (Fallback: novita) ---
    ("vprint", "⚠️  API call failed (attempt 2/3): APIStatusError [HTTP 503]"),
    ("vprint", "   🔌 Provider: novita  Model: deepseek/deepseek-chat"),
    ("vprint", "   🌐 Endpoint: https://api.novita.ai/v1"),
    ("vprint", "   📝 Error: HTTP 503: Service Unavailable"),
    ("vprint", "   ⏱️  Elapsed: 0.00s  Context: 2 msgs, ~473 tokens"),
    ("status", "⏳ Retrying in 0.0s (attempt 2/3)..."),

    # --- Attempt 3 Fail (Fallback: novita) ---
    ("vprint", "⚠️  API call failed (attempt 3/3): APIStatusError [HTTP 503]"),
    ("vprint", "   🔌 Provider: novita  Model: deepseek/deepseek-chat"),
    ("vprint", "   🌐 Endpoint: https://api.novita.ai/v1"),
    ("vprint", "   📝 Error: HTTP 503: Service Unavailable"),
    ("vprint", "   ⏱️  Elapsed: 0.01s  Context: 2 msgs, ~473 tokens"),
]
```

#### Flushed & Emitted Records
On terminal failure, `_flush_status_buffer()` drains the 30 records and replays them in FIFO order, followed by the terminal line:
1--5. `vprint`: Primary attempt 1 error lines (`attempt 1/3`)
6. `status`: `⏳ Retrying in 0.0s (attempt 1/3)...`
7--11. `vprint`: Primary attempt 2 error lines (`attempt 2/3`)
12. `status`: `⚠️ Provider unreachable \u2014 switching to fallback provider...`
13. `status`: `⚠️ Model fallback: meta-llama/llama-3-70b-instruct via openrouter unavailable (provider overloaded); using deepseek/deepseek-chat via novita.`
14--18. `vprint`: Fallback attempt 1 error lines (`attempt 1/3`)
19. `status`: `⏳ Retrying in 0.0s (attempt 1/3)...`
20--24. `vprint`: Fallback attempt 2 error lines (`attempt 2/3`)
25. `status`: `⏳ Retrying in 0.0s (attempt 2/3)...`
26--30. `vprint`: Fallback attempt 3 error lines (`attempt 3/3`)
31. `status` (Terminal Status via `_emit_status`):
    `❌ API failed after 3 retries \u2014 HTTP 503: Service Unavailable`
32. `vprint` (Final error):
    `   💀 Final error: HTTP 503: Service Unavailable`

### 5.3 Provider Call Counts & Terminal State
| Route | Provider | Model | Calls |
| :--- | :--- | :--- | :---: |
| Primary Route | `openrouter` | `meta-llama/llama-3-70b-instruct` | 2 |
| Fallback Route | `novita` | `deepseek/deepseek-chat` | 3 |
| **Total Provider Calls** | | | **5** |

- **Terminal Dictionary**:
  ```python
  {
      "final_response": "API call failed after 3 retries: HTTP 503: Service Unavailable",
      "completed": False,
      "failed": True,
      "error": "HTTP 503: Service Unavailable",
      "failure_reason": "overloaded",
      "failure_retryable": True,
      "billing_unverified": False,
      "billing_block": None,
      "api_calls": 5,
  }
  ```

---

## 6. Case 4: HTTP 500 Request-Format Rejection Through One Fallback and Terminal Exhaustion

### 6.1 Trace Profile & Execution Dynamics
- **Initial State**: `retry_count = 0`, `max_retries = 3`.
- **Fallback Configuration**: Single entry `[{"provider": "novita", "model": "deepseek/deepseek-chat", "base_url": "https://api.novita.ai/v1"}]`.
- **Error Injection**: HTTP 500 payload containing message `"unsupported parameter: temperature must be between 0 and 1"`.
- **Attempt 1 (Primary Route)**:
  - Call 1 returns HTTP 500 with request-format pattern.
  - Classified by [`classify_api_error`](../../agent/error_classifier.py#L1471): matched in `_REQUEST_VALIDATION_PATTERNS` (`"unsupported parameter"`).
  - Returns `FailoverReason.format_error, retryable = False, should_fallback = True`.
  - `retry_count` increment: 0 -> 1 (`attempt 1/3`).
  - Buffers 5 `vprint` lines (`attempt 1/3`).
  - Non-retryable client error gate at [`agent/conversation_loop.py:6768`](../../agent/conversation_loop.py#L6768): `is_client_error` evaluates to `True` (`not classified.retryable`).
  - Fallback check `agent._has_pending_fallback()` evaluates to `True` (0 < 1).
  - Client-error fallback path entered ([`agent/conversation_loop.py:6817-6831`](../../agent/conversation_loop.py#L6817-L6831)):
    1. Line 6823 buffers pre-switch notice:
       `("status", "⚠️ Non-retryable error (HTTP 500) \u2014 trying fallback...")`
    2. Line 6824 calls `agent._try_activate_fallback()` **without arguments** (`reason = None`).
    3. `_fallback_reason_text(None)` at [`agent/chat_completion_helpers.py:2671`](../../agent/chat_completion_helpers.py#L2671) returns `"provider failure"`.
    4. Line 3154 formats switch notice:
       `"⚠️ Model fallback: meta-llama/llama-3-70b-instruct via openrouter unavailable (provider failure); using deepseek/deepseek-chat via novita."`
    5. Line 3159 buffers model-switch notice into `_retry_status_buffer`.
    6. Line 3166 records notice into `agent._pending_fallback_notice`.
    7. Active provider swapped to `novita`, model to `deepseek/deepseek-chat`, client replaced.
    8. Line 6827 resets `retry_count = 0`.
    9. `break` exits inner retry loop to try fallback provider.
- **Attempt 1 (Fallback Route)**:
  - Call 2 returns HTTP 500 request-format rejection on `novita`.
  - Classified as `FailoverReason.format_error, retryable = False, should_fallback = True`.
  - `retry_count` increment: 0 -> 1 (`attempt 1/3`).
  - Buffers 5 `vprint` lines (`attempt 1/3`) for `novita`.
  - Non-retryable client error gate `is_client_error` evaluates to `True`.
  - Fallback check `agent._has_pending_fallback()` evaluates to `False` (1 < 1 is False; chain exhausted).
  - Line 6824 `agent._try_activate_fallback()` returns `False`.
  - Terminal non-retryable abort path entered ([`agent/conversation_loop.py:6838-6863`](../../agent/conversation_loop.py#L6838-L6863)):
    1. Line 6838 calls [`agent._flush_status_buffer()`](../../run_agent.py#L1315). Discards `_pending_fallback_notice = None` and drains all 12 buffered items in FIFO order.
    2. Line 6858 emits terminal status via `_emit_status`:
       `"❌ Non-retryable error (HTTP 500): HTTP 500: unsupported parameter: temperature must be between 0 and 1"`
    3. Lines 6861--6907 emit non-retryable guidance lines via `_vprint(force=True)`.
    4. Returns terminal dictionary immediately without entering standard retry backoff.

### 6.2 Exact Ordered Records

#### Status Records Buffered (12 items total)
```python
[
    # --- Attempt 1 Fail (Primary: openrouter) ---
    ("vprint", "⚠️  API call failed (attempt 1/3): InternalServerError [HTTP 500]"),
    ("vprint", "   🔌 Provider: openrouter  Model: meta-llama/llama-3-70b-instruct"),
    ("vprint", "   🌐 Endpoint: https://openrouter.ai/api/v1"),
    ("vprint", "   📝 Error: HTTP 500: unsupported parameter: temperature must be between 0 and 1"),
    ("vprint", "   ⏱️  Elapsed: 0.00s  Context: 2 msgs, ~477 tokens"),

    # --- Client-Error Fallback Transition ---
    ("status", "⚠️ Non-retryable error (HTTP 500) \u2014 trying fallback..."),
    ("status", "⚠️ Model fallback: meta-llama/llama-3-70b-instruct via openrouter unavailable (provider failure); using deepseek/deepseek-chat via novita."),

    # --- Attempt 1 Fail (Fallback: novita) ---
    ("vprint", "⚠️  API call failed (attempt 1/3): InternalServerError [HTTP 500]"),
    ("vprint", "   🔌 Provider: novita  Model: deepseek/deepseek-chat"),
    ("vprint", "   🌐 Endpoint: https://api.novita.ai/v1"),
    ("vprint", "   📝 Error: HTTP 500: unsupported parameter: temperature must be between 0 and 1"),
    ("vprint", "   ⏱️  Elapsed: 0.01s  Context: 2 msgs, ~473 tokens"),
]
```

#### Flushed & Emitted Records
On terminal failure, `_flush_status_buffer()` drains the 12 records and replays them in FIFO order, followed by the terminal line and guidance:
1--5. `vprint`: Primary attempt 1 error lines (`attempt 1/3`)
6. `status`: `⚠️ Non-retryable error (HTTP 500) \u2014 trying fallback...`
7. `status`: `⚠️ Model fallback: meta-llama/llama-3-70b-instruct via openrouter unavailable (provider failure); using deepseek/deepseek-chat via novita.`
8--12. `vprint`: Fallback attempt 1 error lines (`attempt 1/3`)
13. `status` (Terminal Status via `_emit_status`):
    `❌ Non-retryable error (HTTP 500): HTTP 500: unsupported parameter: temperature must be between 0 and 1`
14. `vprint`: `❌ Non-retryable client error (HTTP 500). Aborting.`
15. `vprint`: `   🔌 Provider: novita  Model: deepseek/deepseek-chat`
16. `vprint`: `   🌐 Endpoint: https://api.novita.ai/v1`
17. `vprint`: `   💡 This type of error won't be fixed by retrying.`

### 6.3 Provider Call Counts & Terminal State
| Route | Provider | Model | Calls |
| :--- | :--- | :--- | :---: |
| Primary Route | `openrouter` | `meta-llama/llama-3-70b-instruct` | 1 |
| Fallback Route | `novita` | `deepseek/deepseek-chat` | 1 |
| **Total Provider Calls** | | | **2** |

- **Terminal Dictionary**:
  ```python
  {
      "final_response": "HTTP 500: unsupported parameter: temperature must be between 0 and 1",
      "completed": False,
      "failed": True,
      "error": "HTTP 500: unsupported parameter: temperature must be between 0 and 1",
      "messages": [...],
      "api_calls": 1,
  }
  ```

---

## 7. Comparative Synthesis Matrix Across All 4 Cases

| Metric / Dimension | Case 1: 500 Recovery | Case 2: 500 Terminal | Case 3: 503 Overload Fallback | Case 4: Format Rejection Fallback |
| :--- | :---: | :---: | :---: | :---: |
| **Primary Route Calls** | 2 | 3 | 2 | 1 |
| **Fallback Route Calls** | 0 | 0 | 3 | 1 |
| **Total Provider Calls** | **2** | **3** | **5** | **2** |
| **Total Records Buffered** | 6 | 17 | 30 | 12 |
| **Buffer Resolution** | Cleared silently (`_clear_status_buffer`) | Flushed FIFO (`_flush_status_buffer`) | Flushed FIFO (`_flush_status_buffer`) | Flushed FIFO (`_flush_status_buffer`) |
| **Operator Messages Emitted** | **0** | **19** (17 buf + 2 term) | **32** (30 buf + 2 term) | **17** (12 buf + 5 term) |
| **Pre-Switch Notice** | None | None | `⚠️ Provider unreachable \u2014 switching to fallback provider...` | `⚠️ Non-retryable error (HTTP 500) \u2014 trying fallback...` |
| **Fallback Reason Text** | None | None | `(provider overloaded)` | `(provider failure)` *(due to unpassed reason)* |
| **Terminal Status Line** | None (Success) | `❌ API failed after 3 retries \u2014 HTTP 500: Internal Server Error` | `❌ API failed after 3 retries \u2014 HTTP 503: Service Unavailable` | `❌ Non-retryable error (HTTP 500): HTTP 500: unsupported parameter: temperature must be between 0 and 1` |
| **Completed / Failed** | `True / False` | `False / True` | `False / True` | `False / True` |

---

## 8. Critical Rust Port Parity Checklist

The live execution traces expose four critical invariants that the Rust implementation in `hermes-gateway` must reproduce with exact fidelity:

1. **Retry Numerator Timing**:
   `retry_count += 1` occurs **immediately upon catching the exception** at [`agent/conversation_loop.py:5758`](../../agent/conversation_loop.py#L5758), prior to any buffer emission. Thus, the initial failure formats as `(attempt 1/3)`, the second as `(attempt 2/3)`, and the third as `(attempt 3/3)`. In Rust, the attempt accounting must not log using pre-increment values.

2. **Pre-Switch and Switch Notice Sequencing**:
   Whenever a fallback activates, **two sequential notices** are buffered:
   - First, the trigger context line:
     - For transport overload / timeout: `"⚠️ Provider unreachable \u2014 switching to fallback provider..."` ([`conversation_loop.py:6078`](../../agent/conversation_loop.py#L6078))
     - For non-retryable client errors: `"⚠️ Non-retryable error (HTTP {status_code}) \u2014 trying fallback..."` ([`conversation_loop.py:6823`](../../agent/conversation_loop.py#L6823))
   - Second, the durable model-switch banner from `try_activate_fallback`:
     - `"⚠️ Model fallback: {old_model} via {old_provider} unavailable ({reason_text}); using {new_model} via {new_provider}."` ([`chat_completion_helpers.py:3154`](../../agent/chat_completion_helpers.py#L3154))
   - In Case 4, because [`conversation_loop.py:6824`](../../agent/conversation_loop.py#L6824) invokes `agent._try_activate_fallback()` without arguments, the reason defaults to `None`, which resolves to `"provider failure"` rather than `"request format rejected"`. Rust code matching this path must maintain this exact string output.

3. **Dual-Structure Lifecycle Semantics**:
   - On **successful recovery** ([`conversation_loop.py:8805-8806`](../../agent/conversation_loop.py#L8805-L8806)):
     - [`_emit_pending_fallback_notice()`](../../run_agent.py#L1285) surfaces durable model switch banners (if any occurred).
     - [`_clear_status_buffer()`](../../run_agent.py#L1276) discards all buffered retry countdowns and error vprints. Zero retry chatter is emitted.
   - On **terminal failure** ([`conversation_loop.py:6838`](../../agent/conversation_loop.py#L6838), [`:7047`](../../agent/conversation_loop.py#L7047)):
     - [`_flush_status_buffer()`](../../run_agent.py#L1315) sets `_pending_fallback_notice = None` first (preventing duplicate replay).
     - Drains all buffered tuples `(kind, message)` in FIFO order and dispatches each to `_emit_status`, `_emit_warning`, or `_vprint(force=True)`.
     - The terminal failure status banner (`❌ ...`) is emitted *after* the flushed buffer, never before it.

4. **Per-Route Call Accounting**:
   - A single-switch failover resets `retry_count = 0` upon switching to the fallback route.
   - For transient overload (Case 3), the primary fails 2 times before triggering eager fallback, and the fallback fails 3 times, giving exactly 5 total wire requests.
   - For non-retryable format rejection (Case 4), the primary fails 1 time before immediate fallback, and the fallback fails 1 time before immediate terminal abort, giving exactly 2 total wire requests.
