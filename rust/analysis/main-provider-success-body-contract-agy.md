# Main Provider Chat-Completions Success-Body Validation and Recovery Contract

**Document Target**: `rust/analysis/main-provider-success-body-contract-agy.md`
**Evidence Lane**: Live Python Chat-Completions HTTP Success Validation and Recovery Contract
**Primary Sources**:
- [`run_agent.py`](file:///home/eins0fx/development/hermes-agent-port/run_agent.py)
- [`agent/conversation_loop.py`](file:///home/eins0fx/development/hermes-agent-port/agent/conversation_loop.py)
- [`agent/chat_completion_helpers.py`](file:///home/eins0fx/development/hermes-agent-port/agent/chat_completion_helpers.py)
- [`agent/empty_response_guard.py`](file:///home/eins0fx/development/hermes-agent-port/agent/empty_response_guard.py)
- [`agent/error_classifier.py`](file:///home/eins0fx/development/hermes-agent-port/agent/error_classifier.py)
- [`agent/transports/chat_completions.py`](file:///home/eins0fx/development/hermes-agent-port/agent/transports/chat_completions.py)
- [`agent/agent_runtime_helpers.py`](file:///home/eins0fx/development/hermes-agent-port/agent/agent_runtime_helpers.py)
- [`rust/tools/gen_main_provider_success_body_goldens.py`](file:///home/eins0fx/development/hermes-agent-port/rust/tools/gen_main_provider_success_body_goldens.py)
- [`rust/tools/main-provider-success-body-goldens.json`](file:///home/eins0fx/development/hermes-agent-port/rust/tools/main-provider-success-body-goldens.json)

---

## 1. Executive Summary & Scope

This specification establishes the authoritative behavioral contract for ordinary main-provider chat-completions successful HTTP response body validation, error detection, recovery ladders, stream boundaries, and terminal outcomes in the Hermes Agent architecture.

When an HTTP 200 OK status code is returned (or an SSE stream opens successfully and delivers chunks), the response payload is not automatically accepted as a usable conversational completion. The live Python runtime executes an intricate, multi-stage validation, classification, and recovery pipeline:

1. **Structural Validation**: Ensures response containers have valid `choices` lists. Missing, null, or empty choice lists are rejected immediately without entering normalization.
2. **Refusal and Content Filter Detection**: Detects provider safety refusals signaled via `finish_reason="content_filter"`, structured OpenAI `message.refusal`, or stream-stalling safety exceptions. Refusals are strictly non-retryable on the same provider and trigger immediate cross-provider fallback or clean terminal refusal delivery.
3. **Stream Truncation and Exact No-Replay Boundary**: Distinguishes failures that occur before any deltas are delivered to the user (fully replayable) from failures after visible tokens are delivered. Once visible tokens are delivered, full request replays are strictly forbidden. Pure text drops return a length-truncated partial stream stub (`PARTIAL_STREAM_STUB_ID`) driving continuation passes without duplicate output.
4. **Empty Response Ladders & Cost-Aware Guards**: Distinguishes unsignaled empties (zero completion tokens) from ambiguous transient drops. Evaluates signature determinism (same model, provider, finish reason) and estimated input cost ($0.25 threshold) to skip futile, costly retries and escalate directly to cross-provider fallback.
5. **Thinking-Only Prefill Continuation**: Handles models producing structured reasoning without visible content by executing up to 2 prefill continuation cycles before escalating to empty retries. At final exhaustion, delivers a labeled reasoning excerpt rather than a blank response, while preserving transcript replay safety via the `_empty_terminal_sentinel` marker.
6. **Tool-Call Decoupling**: Accepts empty visible text when valid tool calls are present. Strips protocol scaffolding tokens (e.g. `[memory]`), pads reasoning content for strict thinking models (DeepSeek v4, Kimi), executes tools, and guards post-tool empty follow-ups.

All behavioral contracts below are pinned by executed Python production code via [`rust/tools/gen_main_provider_success_body_goldens.py`](file:///home/eins0fx/development/hermes-agent-port/rust/tools/gen_main_provider_success_body_goldens.py), generating 114 deterministic test cases across 8 sections in [`rust/tools/main-provider-success-body-goldens.json`](file:///home/eins0fx/development/hermes-agent-port/rust/tools/main-provider-success-body-goldens.json).

---

## 2. Success-Body Processing Pipeline & Architecture

The post-HTTP-200 processing pipeline for ordinary chat completions follows a deterministic sequential decision tree:

```
                  +----------------------------------------------+
                  | HTTP 200 Response Received / Stream Started  |
                  +----------------------------------------------+
                                         |
                                         v
                  +----------------------------------------------+
                  |  Transport validate_response(response)       |
                  |  (response is not None, has choices, not []) |
                  +----------------------------------------------+
                            /                          \
                    [Invalid]                          [Valid]
                          /                              \
                         v                                v
+------------------------------------+   +------------------------------------+
| response_invalid = True            |   | ChatCompletionsTransport           |
| - Invoke API request error hook    |   | .normalize_response(response)      |
| - retry_count += 1                 |   +------------------------------------+
| - Eager fallback on attempt 1!     |                     |
| - If no fallback: jittered backoff |                     v
| - If max_retries: terminal failure |   +------------------------------------+
+------------------------------------+   | Map finish_reason & Refusal        |
                                         | - message.refusal promoted?        |
                                         | - _content_filter_terminated stub? |
                                         +------------------------------------+
                                                   /                \
                                [finish_reason=="content_filter"]   [Other finish_reason]
                                                /                      \
                                               v                        v
            +------------------------------------+   +------------------------------------+
            | Safety Refusal Path                |   | Check Truncation (finish=="length")|
            | - retryable = False                |   +------------------------------------+
            | - Eager fallback immediately!      |             /                        \
            | - If no fallback: terminal refusal |      [Length Truncated]          [Not Length]
            +------------------------------------+             /                            \
                                                              v                              v
                                      +------------------------------------+   +----------------------------+
                                      | Truncation Handler                 |   | Check Tool Calls           |
                                      | - Thinking budget exhausted?       |   +----------------------------+
                                      | - Repetition loop detected?        |          /               \
                                      | - Stream stall with dropped tools? |   [Tool Calls]       [No Tools]
                                      | - Up to 4 continuation passes      |        /                   \
                                      +------------------------------------+       v                     v
                                                                     +--------------------+  +----------------------+
                                                                     | Tool Dispatch      |  | Final Text / Empty   |
                                                                     | - Pad reasoning    |  | Response Path        |
                                                                     | - Strip [memory]   |  +----------------------+
                                                                     | - Execute & loop   |             |
                                                                     +--------------------+             v
                                                                                     +----------------------+
                                                                                     | Post-tool nudge?     |
                                                                                     | Thinking prefill?    |
                                                                                     | Empty-response guard |
                                                                                     | & fallback ladder    |
                                                                                     +----------------------+
```

---

## 3. Scope Item 1: Malformed and Structurally Missing Choices/Message Bodies

### 3.1 Transport Validation Matrix
In [`agent/transports/chat_completions.py`](file:///home/eins0fx/development/hermes-agent-port/agent/transports/chat_completions.py#L1129-L1138), `ChatCompletionsTransport.validate_response(response)` enforces structural invariants:
```python
def validate_response(self, response: Any) -> bool:
    if response is None:
        return False
    if not hasattr(response, "choices") or response.choices is None:
        return False
    if not response.choices:
        return False
    return True
```

The validation results for canonical test shapes:

| Response Shape | `validate_response` | Downstream Classification | Error Details Emitted |
| :--- | :---: | :--- | :--- |
| `None` | `False` | `response_invalid = True` | `["response is None"]` |
| `SimpleNamespace(id="resp-1")` (no choices attribute) | `False` | `response_invalid = True` | `["response has no 'choices' attribute"]` |
| `SimpleNamespace(choices=None)` | `False` | `response_invalid = True` | `["response.choices is None"]` |
| `SimpleNamespace(choices=[])` (empty list) | `False` | `response_invalid = True` | `["response.choices is empty"]` |
| `SimpleNamespace(choices=[SimpleNamespace(message=...)])` | `True` | Accepted | None |
| `SimpleNamespace(choices=[SimpleNamespace(message=None)])` | `True` | Accepted by transport; message parsed in `normalize_response` | None |

### 3.2 Loop Recovery on `response_invalid`
When `validate_response` returns `False` in [`agent/conversation_loop.py`](file:///home/eins0fx/development/hermes-agent-port/agent/conversation_loop.py#L3806-L3859):
1. **Hook Invocation**: Calls `agent._invoke_api_request_error_hook(...)` with `error_type="InvalidAPIResponse"`, `error_message=", ".join(error_details)`, `reason="invalid_response"`, and `retryable=True`.
2. **Attempt Accounting**: Increments `retry_count += 1`.
3. **Eager Fallback Ladder**:
   - If fallback models are configured in `agent._fallback_chain`, eager fallback fires immediately on attempt 1 (`if agent._fallback_index < len(agent._fallback_chain)`).
   - Buffers status: `"⚠️ Empty/malformed response -- switching to fallback..."`.
   - Calls `agent._try_activate_fallback()`. If successful:
     - Synchronizes system prompt via `_sync_failover_system_message`.
     - Resets `retry_count = 0`.
     - Resets `compression_attempts = 0`.
     - Breaks out of the inner retry loop (`_retry.restart_with_rebuilt_messages = True`) to restart iteration under the fallback route.
4. **Same-Provider Retry Fallback (No Fallback Configured)**:
   - If `retry_count < max_retries`:
     - Computes jittered exponential backoff: `wait_time = jittered_backoff(retry_count, base_delay=5.0, max_delay=120.0)`.
     - Sleeps with responsiveness to user interrupts and gateway activity touches.
     - Loops to retry the same provider.
   - If `retry_count >= max_retries`:
     - Emits status: `"❌ Max retries ({max_retries}) exceeded for invalid responses. Giving up."`.
     - Derives `_failure_hint` based on error code or response duration:
       - 524: `"upstream provider timed out (Cloudflare 524, {duration:.0f}s)"`
       - 504: `"upstream gateway timeout (504, {duration:.0f}s)"`
       - 429: `"rate limited by upstream provider (429)"`
       - 500 / 502: `"upstream server error ({code}, {duration:.0f}s)"`
       - 503 / 529: `"upstream provider overloaded ({code})"`
       - Duration < 10s: `"fast response ({duration:.1f}s) -- likely rate limited"`
       - Duration > 60s: `"slow response ({duration:.0f}s) -- likely upstream timeout"`
     - Formats terminal response: `"Invalid API response after {max_retries} retries: {_failure_hint}"`.
     - Persists session and returns: `{"final_response": ..., "completed": False, "failed": True, "error": ...}`.

### 3.3 Normalization of Choice Bodies
In [`agent/transports/chat_completions.py`](file:///home/eins0fx/development/hermes-agent-port/agent/transports/chat_completions.py#L984-L1057):
- **Choice Message None**: If `choice.message` is `None`, `normalize_response` produces `NormalizedResponse(content=None, tool_calls=None, finish_reason="stop", reasoning=None)`. Downstream, `assistant_message.tool_calls` is `None` and `content` is `""`, routing into the empty response ladder.
- **Malformed Tool Call Filtering**:
  - If `tc.function is None` or `tc.function.name is None`: skipped from the tool call array (`continue`).
  - If `tc.function.name == ""`: explicitly preserved as a blank name so Hermes invalid tool name handling can repair or error-result it.
- **Integer Finish Reasons**: Providers returning integer finish reasons (such as Poolside returning integer 24) are coerced to strings (`str(_fr)`), yielding `"24"`.

---

## 4. Scope Item 2: Empty Assistant Responses, Configured Guard, and Exhaustion

### 4.1 Configured Empty-Response Guard (`agent/empty_response_guard.py`)
The guard protects against billing loops where large context windows are repeatedly re-sent for unsignaled provider empties.

#### Config Resolution (`resolve_guard_settings`)
Resolved from `agent.empty_response_guard` in `config.yaml`:
- Defaults: `DEFAULT_GUARD_ENABLED = True`, `DEFAULT_COST_THRESHOLD_USD = Decimal("0.25")`.
- YAML string booleans (`"false"`, `"0"`, `"off"`, `"no"`) resolve to `False`.
- Custom thresholds (`Decimal("1.50")`, `5`) resolve cleanly; invalid or negative thresholds fall back to default `$0.25`.

#### Zero Output Extraction (`_zero_output`)
- Normalizes usage via `agent.usage_pricing.normalize_usage`.
- Evaluates `output_tokens` and `reasoning_tokens`.
- Output is zero if `(output_tokens + reasoning_tokens) == 0` and `prompt_tokens > 0`.
- If `completion_tokens == 0` but `reasoning_tokens > 0` (hidden chain-of-thought), output is **NOT** zero; reasoning counts as generation.
- Missing usage object or all-zero prompt tokens proxy artifact fails open: `(False, False)`.

#### Deterministic Empty Detection (`deterministic_empty`)
Requires `>= 2` consecutive attempts in the active streak:
1. **Signature Match**: All attempts must match `(model, provider, finish_reason)`. If model or finish reason changes across attempts, determinism resets.
2. **Usage Evidence**: All attempts have usage present and zero output.
3. **Usage-Absent Evidence**: All attempts lack usage and have `observed_generation == False`.
4. **Fails Open**: Mixed evidence (one with usage, one without) fails open (`False`). When guard is disabled via config, always returns `False`.

#### Dynamic Retry Budget (`empty_retry_budget`)
- Default budget: `DEFAULT_EMPTY_RETRY_BUDGET = 3`.
- Reduced budget: If estimated attempt input cost >= `cost_threshold_usd` ($0.25), drops budget to `REDUCED_EMPTY_RETRY_BUDGET = 1`.
- If pricing is unknown, missing, or guard disabled: stays at `3`.

#### Streak Cost Tracking (`streak_cost_usd`)
- Accumulates estimated attempt costs in `_STREAK_COST_ATTR`.
- Cleared whenever `_empty_content_retries == 0` (turn start, tool success, compaction, fallback activation).

### 4.2 Loop Progression and Fallback Ladder
In [`agent/conversation_loop.py`](file:///home/eins0fx/development/hermes-agent-port/agent/conversation_loop.py#L8567-L8797):
1. **Empty Candidate Predicate**:
   ```python
   _truly_empty = not agent._strip_think_blocks(final_response).strip()
   _prefill_exhausted = (_has_structured and agent._thinking_prefill_retries >= 2)
   _empty_candidate = _truly_empty and (not _has_structured or _prefill_exhausted)
   ```
2. **Attempt Recording**: When `_empty_candidate` is true, calls `record_empty_attempt(agent, finish_reason=finish_reason, response=response, observed_generation=_has_structured)`.
3. **Same-Provider Retry**:
   - If `_empty_content_retries < _empty_retry_budget and not _deterministic_empty`:
     - Increments `agent._empty_content_retries += 1`.
     - Waits with jittered backoff (base 5.0s, cap 60.0s):
       `wait_time = jittered_backoff(agent._empty_content_retries, base_delay=5.0, max_delay=60.0)`.
     - Buffers status: `"⚠️ Empty response from model -- retrying ({retries}/{budget}) in {wait_time}s"`.
     - Retries the API call (`continue`).
4. **Deterministic Empty Short-Circuit**:
   - If `_deterministic_empty` is true, logs warning: `"Repeated empty response detected ... skipping remaining retries"`.
   - Buffers status: `"⚠️ Model is repeatedly returning empty content -- skipping further retries to avoid repeat charges"`.
5. **Cross-Provider Fallback Activation**:
   - If retries exhausted or skipped, and `agent._fallback_chain` is non-empty:
     - Buffers status: `"⚠️ Model returning empty responses -- switching to fallback provider..."`.
     - Calls `agent._try_activate_fallback()`.
     - If activated: resets `agent._empty_content_retries = 0`, syncs system prompt, sets `_preflight_compression_blocked = False`, and restarts iteration (`continue`).
6. **Exhaustion Terminal Outcome**:
   - If no fallback or fallback exhausted:
     - Buffers streak cost if known: `"ℹ️ Estimated cost of these empty attempts: ~${cost:.2f} ..."`.
     - Flushes status buffer.
     - Drops trailing synthetic scaffolding: `agent._drop_trailing_empty_response_scaffolding(messages)`.
     - Builds assistant message:
       ```python
       assistant_msg = agent._build_assistant_message(assistant_message, finish_reason)
       assistant_msg["content"] = "(empty)"
       assistant_msg["_empty_terminal_sentinel"] = True
       append_message(messages, assistant_msg)
       ```
     - Terminal delivered `final_response`:
       - If reasoning exists: returns labeled reasoning excerpt.
       - Else: returns `"(empty)"`.
     - Sets `_turn_exit_reason = "empty_response_exhausted"` and terminates turn.

---

## 5. Scope Item 3: finish_reason=content_filter and Equivalent Tagged Stream Refusals

### 5.1 HTTP 200 Refusal Signals
Provider safety refusals can arrive via three channels:
1. **Direct `finish_reason="content_filter"`**: Explicit finish reason on the choice.
2. **OpenAI Structured Refusal (`message.refusal`)**: The OpenAI SDK populates `message.refusal` while leaving `content` empty. In [`agent/transports/chat_completions.py`](file:///home/eins0fx/development/hermes-agent-port/agent/transports/chat_completions.py#L1086-L1119):
   - If `refusal` is non-empty, and visible text and tool calls are absent:
     - Promotes `content = refusal`.
     - Promotes `finish_reason = "content_filter"`.
     - Preserves `provider_data["refusal"] = refusal`.
   - If visible text or tool calls are present alongside refusal: refusal is kept only in `provider_data["refusal"]`; `finish_reason` is unchanged (`"stop"` or `"tool_calls"`).
3. **Mid-Stream Safety Stall**: When upstream safety filters (e.g. MiniMax `output new_sensitive (1027)`, Azure OpenAI `responsibleaipolicyviolation`) sever the connection mid-delivery after deltas were sent:
   - In [`agent/chat_completion_helpers.py`](file:///home/eins0fx/development/hermes-agent-port/agent/chat_completion_helpers.py#L5831-L5864), `classify_api_error` evaluates the error.
   - If classified as `FailoverReason.content_policy_blocked`: stamps the partial stub `_stub._content_filter_terminated = True` with `finish_reason = "length"`.

### 5.2 Error Classification Taxonomy
In [`agent/error_classifier.py`](file:///home/eins0fx/development/hermes-agent-port/agent/error_classifier.py#L956-L961), safety refusals classify as:
- `reason = FailoverReason.content_policy_blocked`
- `retryable = False` (deterministic for unchanged prompt; retrying primary is strictly disallowed)
- `should_fallback = True` (a different model or provider may accept the prompt)
- `should_compress = False`

### 5.3 Recovery Actions in Conversation Loop
- **HTTP 200 Refusal** ([`agent/conversation_loop.py:4060-4148`](file:///home/eins0fx/development/hermes-agent-port/agent/conversation_loop.py#L4060-L4148)):
  - Calls `_invoke_api_request_error_hook` with `error_type="ContentPolicyBlocked"`, `retryable=False`.
  - Checks `agent._try_activate_fallback()`. If fallback activates: resets `retry_count = 0`, syncs system prompt, restarts iteration.
  - If no fallback: delivers clean user-facing refusal:
    ```
    ⚠️  The model declined to respond to this request (safety refusal -- not a Hermes/gateway failure).

    Model's explanation: {refusal_text}

    {_CONTENT_POLICY_RECOVERY_HINT}
    ```
    Returns `completed: False, content_policy_blocked: True`.
- **Stream Content Filter Stall** ([`agent/conversation_loop.py:4300-4348`](file:///home/eins0fx/development/hermes-agent-port/agent/conversation_loop.py#L4300-L4348)):
  - Detects `response._content_filter_terminated == True`.
  - Activates fallback immediately:
    - Rolls back partial content fragments to clean boundary via `agent._get_messages_up_to_last_assistant(messages)`.
    - Unmarks `_length_continuation_fragment` and `_length_continuation_nudge`.
    - Resets `length_continue_retries = 0`, `retry_count = 0`, and restarts iteration.
  - If no fallback: falls through to length continuation on same provider (best-effort).

---

## 6. Scope Item 4: Nonempty Reasoning with Empty Visible Content

### 6.1 Reasoning Channels
Reasoning is extracted from:
1. `assistant_message.reasoning` (standard reasoning attribute)
2. `assistant_message.reasoning_content` (DeepSeek / Moonshot format)
3. `assistant_message.reasoning_details` (OpenRouter unified format)
4. In-content inline blocks `<think>...</think>` (Ollama / Qwen format)

### 6.2 Thinking-Only Prefill Continuation
When visible content is empty (`_truly_empty == True`) but structured reasoning is detected (`_has_structured == True`):
1. **Prefill Attempt 1 & 2**:
   - Checked at `agent._thinking_prefill_retries < 2`.
   - Increments `agent._thinking_prefill_retries += 1`.
   - Buffers status: `"↻ Thinking-only response -- prefilling to continue ({retries}/2)"`.
   - Builds interim assistant message with `finish_reason="incomplete"`.
   - Marks message with `_thinking_prefill = True`.
   - Appends to `messages` and restarts loop (`continue`).
2. **Prefill Recovery**:
   - If subsequent call returns visible text or tool calls:
     - Pops all trailing `_thinking_prefill` messages from `messages`.
     - Resets `agent._thinking_prefill_retries = 0`.
     - Normal conversational execution resumes.
3. **Prefill Exhaustion**:
   - If `agent._thinking_prefill_retries >= 2`: sets `_prefill_exhausted = True`.
   - Transitions into the empty-response retry ladder (Scope Item 2).

### 6.3 Terminal Reasoning Excerpt Delivery
If empty response retries and fallback exhaust:
- **Delivery Text**: Promotes the reasoning preview to `final_response`:
  ```
  ⚠️ The model produced only internal reasoning and no final answer, despite retries. Its last reasoning, which may contain the answer:

  {reasoning_preview}
  ```
- **Transcript Invariant**: The delivered excerpt is strictly **delivery-only**. The message persisted in the transcript has:
  ```python
  assistant_msg["content"] = "(empty)"
  assistant_msg["_empty_terminal_sentinel"] = True
  ```
  This prevents poisoning future conversation context with ungrounded chain-of-thought blocks.

### 6.4 Thinking Budget Exhaustion under `finish_reason="length"`
- If `finish_reason == "length"` and the model produced `<think>` tags but zero text after them:
  - Classified as thinking budget exhaustion (`_thinking_exhausted`).
  - Terminal response: `"⚠️ **Thinking Budget Exhausted**\n\nThe model used all its output tokens on reasoning..."`.
  - Aborts immediately without burning continuation retries.
- If reasoning was returned in a separate field (`reasoning_content`) and output hit the cap:
  - Sets `agent._ephemeral_reasoning_off = True` for the continuation request so the model produces the visible answer without re-thinking.

---

## 7. Scope Item 5: Empty Content Paired with Valid Tool Calls

### 7.1 Valid Tool Turn Acceptance
In [`agent/conversation_loop.py:7741`](file:///home/eins0fx/development/hermes-agent-port/agent/conversation_loop.py#L7741):
- Evaluates `if assistant_message.tool_calls:`.
- Models commonly emit `content=None` or `content=""` when issuing function calls. This is accepted as a valid tool turn; it **never** triggers empty response retries or empty guard counters.
- **Protocol Scaffolding Discard**: If content contains a bare bracketed tool marker (e.g. `[memory]`, matching `_STALE_MARKER_RE = re.compile(r"^\[[A-Za-z_][A-Za-z0-9_.-]*\]$")`), it is stripped to `""`.

### 7.2 Assistant Message Construction & Strict Reasoning Padding
In `build_assistant_message`:
- Tool calls are attached with validated IDs.
- Provider data extras (e.g. Gemini `thought_signature` in `extra_content`) are preserved for replay.
- **Strict Echo-Back Padding**: If `agent._needs_thinking_reasoning_pad()` is True (DeepSeek v4, Kimi / Moonshot, Xiaomi MiMo, or `reasoning_echo_opt_in`):
  - Strict providers reject assistant tool-call messages that omit `reasoning_content` (HTTP 400).
  - If reasoning was captured, attaches it. If absent, pads with a single space `" "` (`msg["reasoning_content"] = reasoning_text or " "`). A non-empty whitespace string satisfies validation without fabricating reasoning.

### 7.3 State Lifecycle and Post-Tool Follow-Up
1. **Counter Resets**: When tool calls land, resets `_thinking_prefill_retries = 0`, `_empty_content_retries = 0`, `_post_tool_empty_retried = False`, and `_dropped_toolcall_retries = 0`.
2. **Post-Tool Empty Follow-Up**:
   - If the subsequent turn returns empty content and no tool calls:
     - **Housekeeping Content Reuse**: If the previous turn had content alongside tools, and all tools were housekeeping (`memory`, `todo_list`, `skill_manage`, `session_search`), reuses the earlier content as `final_response` (`exit_reason="fallback_prior_turn_content"`).
     - **Substantive Tool Nudge**: If substantive tools were run, sends synthetic nudge once:
       ```python
       _nudge_msg = agent._build_assistant_message(assistant_message, finish_reason)
       _nudge_msg["content"] = "(empty)"
       _nudge_msg["_empty_recovery_synthetic"] = True
       append_message(messages, _nudge_msg)
       append_message(messages, {
           "role": "user",
           "content": _EMPTY_TOOL_RESPONSE_NUDGE,
           "_empty_recovery_synthetic": True,
       })
       agent._post_tool_empty_retried = True
       ```
     - If the model remains empty after the nudge, proceeds into the empty-response retry ladder.

---

## 8. Scope Item 6: Truncated/Partial Streamed Output and Exact No-Replay Boundary

### 8.1 The Two-Sided Stream Failure Boundary
In [`agent/chat_completion_helpers.py:5190-5285`](file:///home/eins0fx/development/hermes-agent-port/agent/chat_completion_helpers.py#L5190-L5285) and [`5771-5870`](file:///home/eins0fx/development/hermes-agent-port/agent/chat_completion_helpers.py#L5771-L5870):

```
                                  Stream Connection Error
                                             |
                                             v
                             Were any deltas sent to user?
                              (deltas_were_sent["yes"])
                                    /                 \
                                  [No]               [Yes]
                                  /                     \
                                 v                       v
               +-----------------------------+   Tool call in-flight AND
               | Inner Stream Worker Retry   |   error is transient?
               | (up to HERMES_STREAM_RETRIES|          /             \
               |  default 2 retries = 3 total)       [Yes]            [No]
               +-----------------------------+        /                 \
                             |                       v                   v
               Retries exhausted?          +--------------------+  +----------------------------+
                    /        \             | Narrow Tool Gate:  |  | NO REPLAY PERMITTED!       |
                 [No]        [Yes]         | - Emit reconnect   |  | - Suppress exception       |
                  /            \           |   marker to user   |  | - Build length-truncated   |
                 v              v          | - Reset deltas flag|  |   partial stream stub      |
            [Reconnect]  [Re-raise Error]  | - Worker retries   |  |   (PARTIAL_STREAM_STUB_ID) |
                                |          +--------------------+  | - Return stub to loop      |
                                v                                  +----------------------------+
                     +----------------------+                                     |
                     | Outer Retry Loop:    |                                     v
                     | REPLAY PERMITTED     |                      +----------------------------+
                     | - Full request retry |                      | Continuation Machinery:    |
                     | - Credential rotate  |                      | - Up to 4 text/tool passes |
                     | - Provider fallback  |                      | - Ceiling exit at pass 4   |
                     +----------------------+                      +----------------------------+
```

1. **Before Token Delivery (`deltas_were_sent == False`)**:
   - User has seen zero output.
   - If inner worker retries exhaust, re-raises `result["error"]` to outer loop.
   - **Replay is permitted**: Outer retry loop may re-send the full request, rotate credentials, or activate fallback.
2. **After Token Delivery (`deltas_were_sent == True`)**:
   - User has already seen partial output on screen.
   - **Narrow Tool Gate**: If a tool call was in flight (`_partial_tool_in_flight`), the error is transient, and inner stream attempts remain: emits reconnect marker (`"\n\n⚠ Connection dropped mid tool-call; reconnecting…\n\n"`), clears delivery flag, and retries the connection.
   - **General Rule (No Replay Permitted)**: Re-raising is strictly suppressed. The worker returns `_build_partial_stream_stub(...)` tagged with `id=PARTIAL_STREAM_STUB_ID` and `finish_reason=FINISH_REASON_LENGTH`. The outer loop enters continuation passes without re-streaming earlier text.

### 8.2 Continuation Prompts (`_get_continuation_prompt`)
When continuing from a length-truncated turn:
1. **Dropped Tools**: If stream stalled mid tool-call:
   `"[System: Your previous tool call ({tool_list}) was too large and the stream timed out before it could be delivered. Do NOT retry the same tool call with the same large content. Instead, break the content into multiple smaller tool calls... Each tool call's arguments must be under ~8K tokens...]"`
2. **Network Interruption**: If `is_partial_stub == True`:
   `"[System: The previous response was cut off by a network error mid-stream. Continue exactly where you left off. Do not restart or repeat prior text. Finish the answer directly.]"`
3. **Output Length Limit**: If `finish_reason == "length"` (genuine token cap):
   `"[System: Your previous response was truncated by the output length limit. Continue exactly where you left off. Do not restart or repeat prior text. Finish the answer directly.]"`

### 8.3 Continuation Progression Limits
- **Text Continuation**: Up to 4 passes (`length_continue_retries < 4`). Pass 4 trips ceiling exit: stitches partial fragments, strips nudges, marks `completed: False, partial: True`.
- **Tool Call Truncation**: Up to 4 passes (`truncated_tool_call_retries < 4`). Retries boost `max_tokens` exponentially (`agent.max_tokens * (2 ** retries)`, capped at 32,768) without appending broken tool calls. Pass 4 terminates with refusal to execute incomplete tool arguments.

---

## 9. Scope Item 7: Retry vs Fallback Stickiness, Counters, Notices, and Lifecycle

### 9.1 Decision Matrix: Same-Provider Retry vs Fallback
| Failure Condition | Has Fallback Chain? | Immediate Action | Budget / Pass Limit |
| :--- | :---: | :--- | :--- |
| Malformed HTTP 200 (`response_invalid`) | Yes | Fallback immediately (attempt 1 eager fallback) | 0 retries on primary |
| Malformed HTTP 200 (`response_invalid`) | No | Same-provider retry with jittered backoff (5s-120s) | `max_retries` (default 3) |
| Ambiguous Empty Response | Any | Same-provider retry with jittered backoff (5s-60s) | 3 (or 1 if cost >= $0.25) |
| Deterministic Empty Response | Yes | Skip remaining retries; fallback immediately | 0 remaining retries |
| Deterministic Empty Response | No | Skip remaining retries; terminal `(empty)` | 0 remaining retries |
| Content Policy Refusal (`content_filter`) | Yes | Fallback immediately | 0 retries on primary |
| Content Policy Refusal (`content_filter`) | No | Terminal refusal response | 0 retries on primary |
| Stream Content Filter Stall | Yes | Fallback immediately; rollback partial fragment | 0 retries on primary |
| Stream Text Truncation | Any | Same-provider length continuation passes | Up to 4 passes |
| Stream Tool Truncation | Any | Same-provider retry with token boost | Up to 4 passes |

### 9.2 Fallback Route Stickiness and Prompt Rewriting
When `agent._try_activate_fallback()` succeeds:
1. **Runtime Reconfiguration**: Updates `agent.model`, `agent.provider`, `agent.base_url`, `agent.api_key`, and `agent.api_mode`.
2. **System Prompt Identity Rewriting**: Calls `rewrite_prompt_model_identity(agent, model, provider)` to update the volatile tail lines (`Model: ...`, `Provider: ...`) of `agent._cached_system_prompt`.
3. **Within-Turn Stickiness**: The fallback route remains sticky for all subsequent iterations and tool-call rounds of the current turn.
4. **Counter Resets**: Resets `retry_count = 0` and `_empty_content_retries = 0`.
5. **Notice Emission**: Calls `agent._emit_pending_fallback_notice()` upon successful completion so the user is informed of the provider switch even after noisy retry logs are cleared.
6. **Multi-Turn Restoration**: At the start of the next turn, `restore_primary_runtime(agent)` restores the primary configuration.

---

## 10. Scope Item 8: Provider-Specific Chat-Completions Distinctions

The ordinary chat-completions transport accommodates several provider quirks:
1. **DeepSeek**:
   - Native cache hit/miss tokens extracted from `usage.prompt_cache_hit_tokens` and `usage.prompt_cache_miss_tokens`.
   - `reasoning_content` preserved in `provider_data["reasoning_content"]`.
   - DeepSeek v4 thinking mode requires `reasoning_content` echo-back on assistant tool-call messages (padded with whitespace `" "`).
2. **Kimi / Moonshot**:
   - Strict content validation: rejects empty assistant content `""` with HTTP 400. Pre-send sanitizer `repair_empty_non_final_messages` heals textless turns by substituting placeholder `"[response interrupted]"`.
   - Requires `reasoning_content` echo-back.
3. **OpenAI / OpenRouter**:
   - Cache stats in `usage.prompt_tokens_details.cached_tokens`.
   - `reasoning_details` list preserved for multi-turn reasoning continuity.
   - Structured refusal in `message.refusal` promoted to `content` + `finish_reason="content_filter"` when text and tools are absent.
4. **Gemini (OpenAI-Compatible)**:
   - Preserves `extra_content` on tool calls (thought signature); omitting it on replay results in HTTP 400.
5. **Poolside**:
   - Returns integer finish reasons (e.g. 24); stringified to `"24"`.
6. **xAI**:
   - Reverses tool search wire alias `hermes_tool_search` back to `tool_search`.
7. **Ollama / GLM**:
   - In-content `<think>...</think>` tags stripped from content and extracted into reasoning.
   - Premature stop heuristic (`_should_treat_stop_as_truncated`) rewrites finish reason `"stop"` to `"length"`.

---

## 11. Verification Commands & Golden Manifest

### 11.1 Executed Test Commands
All contracts were validated through direct test execution:

```bash
# 1. Focused test suite execution (161 tests passed in 27.85s)
.venv/bin/pytest \
  tests/agent/test_empty_response_guard.py \
  tests/run_agent/test_18028_content_policy_blocked.py \
  tests/run_agent/test_empty_response_recovery_persistence.py \
  tests/run_agent/test_empty_terminal_reasoning_surface.py \
  tests/agent/transports/test_chat_completions.py \
  tests/agent/transports/test_chat_completions_empty_tool_calls.py \
  tests/run_agent/test_partial_stream_finish_reason.py \
  tests/test_empty_model_fallback.py \
  tests/run_agent/test_continuation_repetition_guard.py \
  tests/run_agent/test_continuation_ceiling_wedge.py

# 2. Golden generator execution (114 cases generated)
.venv/bin/python rust/tools/gen_main_provider_success_body_goldens.py

# 3. Golden parity check
.venv/bin/python rust/tools/gen_main_provider_success_body_goldens.py --check
```

### 11.2 Case Count Breakdown across 8 Golden Sections
The deterministic golden file [`rust/tools/main-provider-success-body-goldens.json`](file:///home/eins0fx/development/hermes-agent-port/rust/tools/main-provider-success-body-goldens.json) contains 114 cases:

1. `malformed_and_missing_choice_bodies`: 23 cases
2. `empty_assistant_response_guard_and_exhaustion`: 34 cases
3. `content_filter_and_tagged_stream_refusals`: 11 cases
4. `nonempty_reasoning_with_empty_visible_content`: 11 cases
5. `empty_content_paired_with_valid_tool_calls`: 11 cases
6. `stream_truncation_and_no_replay_boundary`: 8 cases
7. `retry_vs_fallback_stickiness_and_lifecycle`: 10 cases
8. `provider_specific_chat_completions_distinctions`: 6 cases
**Total Deterministic Golden Cases**: 114

---

## 12. Uncertainties and Deferred Non-Chat / OAuth Behavior

1. **Codex Responses API**: Deferred to Codex transport checkpoint. Codex responses utilize session items, encrypted reasoning containers, and `status="incomplete"` with `incomplete_details.reason`.
2. **Anthropic Messages / Bedrock Converse Transports**: Deferred to their respective transport modules. Anthropic uses content block lists and `stop_reason="refusal"`; Bedrock uses Converse API output blocks.
3. **OAuth Device Flow and Token Refresh**: Deferred to authentication subsystem. OAuth token refresh on 401 is handled before success-body validation.
4. **Dynamic Provider Plugins**: External dynamic provider plugins that intercept HTTP streams remain out of scope for the static Rust chat-completions engine.
