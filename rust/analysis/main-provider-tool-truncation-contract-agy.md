# Native Main-Provider Tool Call Truncation Behavior Contract

**Document Target**: `rust/analysis/main-provider-tool-truncation-contract-agy.md`
**Evidence Lane**: Live Python Chat-Completions HTTP Success Tool-Call Truncation Retry Contract
**Primary Sources**:
- [`agent/conversation_loop.py`](file:///home/eins0fx/development/hermes-agent-port/agent/conversation_loop.py)
- [`agent/chat_completion_helpers.py`](file:///home/eins0fx/development/hermes-agent-port/agent/chat_completion_helpers.py)
- [`agent/message_sanitization.py`](file:///home/eins0fx/development/hermes-agent-port/agent/message_sanitization.py)
- [`agent/transports/chat_completions.py`](file:///home/eins0fx/development/hermes-agent-port/agent/transports/chat_completions.py)
- [`run_agent.py`](file:///home/eins0fx/development/hermes-agent-port/run_agent.py)
- [`rust/tools/gen_main_provider_tool_truncation_goldens.py`](file:///home/eins0fx/development/hermes-agent-port/rust/tools/gen_main_provider_tool_truncation_goldens.py)
- [`rust/tools/main-provider-tool-truncation-goldens.json`](file:///home/eins0fx/development/hermes-agent-port/rust/tools/main-provider-tool-truncation-goldens.json)

---

## 1. Executive Summary and Architectural Scope

This specification establishes the authoritative behavioral contract for truncated tool calls on the ordinary main-provider chat-completions transport after an HTTP 200 success (or successful SSE stream open) in the Hermes Agent architecture.

When an LLM attempts to emit structured function calls but is truncated before finishing the argument payload (either because it hits the provider output token limit, or because the upstream stream drops while delivering tool arguments), the resulting tool arguments are invalid JSON or incomplete. In contrast to ordinary text generation--where partial output is preserved and extended via synthetic user nudges--truncated tool calls present an acute safety risk. Incomplete tool payloads cannot be safely executed. Executing a tool with truncated parameters (such as half a file path, partial shell commands, or unclosed JSON) risks data corruption, arbitrary side effects, or runtime panics.

The Python runtime resolves this condition through a specialized, strictly bounded **same-request retry protocol**:

1. **Eligibility and Preemption**: Responses tagged with `finish_reason == "length"` that carry an assistant message with `tool_calls` enter the tool truncation retry lane. The presence of tool calls explicitly preempts both the thinking-budget exhaustion guard and the repetition-dominated abort guard.
2. **Same-Request Retry Paradigm**: Unlike text continuation, tool call truncation never appends intermediate assistant fragments or synthetic user continuation nudges to the conversational transcript. The message history remains completely unchanged, and the engine re-runs the exact same request payload from the existing conversational state.
3. **Bounded Retry Budget**: The engine grants up to 4 retry attempts (`truncated_tool_call_retries < 4`). A turn can make up to 5 total API attempts (1 initial attempt + 4 retries) before ceiling exhaustion. The retry counter is turn-scoped and resets to 0 upon any successful tool execution.
4. **Exponential Output-Cap Growth**: On each retry attempt `r` in `1..=4`, the engine exponentially scales the ephemeral output token cap: `boost = base * (2 ** r)`, where `base = agent.max_tokens or 4096`. The boost preserves higher caller-requested caps and is bounded by `max(32768, requested_cap or 0)`. The ephemeral cap is consumed in one shot by `_build_api_kwargs` and never leaks to subsequent turns.
5. **Absolute Tool Execution Prohibition**: Truncated tool calls are strictly forbidden from executing. If all 4 retries fail, or if a router disguised a truncation under `finish_reason == "tool_calls"`, the handler refuses execution without calling any tool dispatcher.
6. **Partial-Stream Stub Distinctions**: Responses arriving as `PARTIAL_STREAM_STUB_ID` with `tool_calls` present (`_is_stub_stall == True`) represent mid-stream network stalls rather than genuine output-cap limits. They trigger distinct retry logging and distinct terminal failure messages (`"Stream repeatedly dropped mid tool-call (network); the tool was not executed"` versus `"Response truncated due to output length limit"`).
7. **Streaming Zero-Byte Drops and Semantic Continuation Boundary**: When an SSE stream ends before a single argument byte arrives (`_tool_args_dropped_no_finish`), the stream helper returns a stub with `tool_calls = None` and `_dropped_tool_names = [...]`. Because `tool_calls` is empty, this condition diverts to the semantic length continuation lane with a targeted dropped-tools chunking prompt (capped at 3 tool names).
8. **Transcript Repair and Role Alternation**: Upon ceiling exit, the engine invokes `close_interrupted_tool_sequence`. If a prior successful tool execution in the same turn left a trailing `tool` message, a synthetic assistant message with the terminal error is appended to guarantee valid role alternation (`tool -> assistant`).
9. **Accounting and Persistence**: Truncated tool responses are never written to disk or persisted in session history. Unsuccessful retry calls do not increment `session_api_calls` or accrue token usage / cost. Only successful responses are billed.

All behavioral contracts below are pinned by executed Python production code via [`rust/tools/gen_main_provider_tool_truncation_goldens.py`](file:///home/eins0fx/development/hermes-agent-port/rust/tools/gen_main_provider_tool_truncation_goldens.py), generating 81 deterministic test cases across 11 sections in [`rust/tools/main-provider-tool-truncation-goldens.json`](file:///home/eins0fx/development/hermes-agent-port/rust/tools/main-provider-tool-truncation-goldens.json).

---

## 2. Sequence and Architecture Diagram

```
                  +----------------------------------------------------+
                  |  HTTP 200 OK / SSE Stream Started Successfully     |
                  +----------------------------------------------------+
                                             |
                                             v
                  +----------------------------------------------------+
                  | ChatCompletionsTransport.normalize_response()      |
                  | - Stringify finish_reason (e.g. 24 -> "24")        |
                  | - Map absent finish_reason -> "stop"               |
                  | - In stream: unrepairable tool args -> "length"    |
                  +----------------------------------------------------+
                                             |
                                             v
                              [finish_reason == "length"]
                               /                         \
                             Yes                          No
                             /                             \
                            v                               v
+------------------------------------------+    +--------------------------------+
| Pre-Continuation Guardrails:             |    | [finish_reason == "tool_calls"]|
| - Thinking Exhaustion Check:             |    | Check unclosed JSON delimiter: |
|   Gated on `not _trunc_has_tool_calls`   |    | - If cut off mid-stream:       |
| - Repetition Guard Check:                |    |   ABORT immediately (0 retries)|
|   Gated on `not _trunc_has_tool_calls`   |    | - If format mistake:           |
| - Content-Filter Stream Stall:           |    |   formatting retry (up to 3)   |
|   If _content_filter_terminated:         |    +--------------------------------+
|   Eager fallback escalation (0 retries)  |
+------------------------------------------+
                     |
                     v
+------------------------------------------+
| Tool Call Branch Gate:                   |
| assistant_message and tool_calls present |
+------------------------------------------+
           /                        \
    [Has Tool Calls]           [Text Only]
          /                            \
         v                              v
+------------------------------------+  +---------------------------------------+
| Truncated Tool Call Lane:          |  | Ordinary Text Continuation Lane:     |
| Check `truncated_tool_call_retries`|  | - Append interim fragment             |
+------------------------------------+  | - Append continuation nudge           |
         |                              | - Accumulate truncated parts          |
         v                              +---------------------------------------+
    [retries < 4]
     /         \
   Yes          No (Ceiling Hit)
   /             \
  v               v
+--------------------------------------+   +------------------------------------+
| Same-Request Retry Pass:             |   | Ceiling Exit (Attempt 4 Failed):   |
| - truncated_tool_call_retries += 1   |   | - Flush buffered logs              |
| - Do NOT append broken response      |   | - Prohibit tool execution          |
| - Do NOT append user nudge           |   | - Determine _final_response:       |
| - Compute boosted output cap:        |   |   * If stub stall: Network drop    |
|   base * (2 ** retries) [max 32768]  |   |   * Else: Output length limit      |
| - Set agent._ephemeral_max_output_   |   | - close_interrupted_tool_sequence: |
|   tokens = boost                     |   |   * If tool tail: append assistant |
| - Re-run API call from unchanged     |   | - agent._persist_session(messages) |
|   messages list                      |   | - Return terminal partial failure: |
+--------------------------------------+   |   {completed: False, partial: True}|
                                           +------------------------------------+
```

---

## 3. Eligibility Matrix and Guardrail Preemption

### 3.1 Eligibility Gate

A response enters the truncated tool call lane if and only if:
1. `api_mode` is one of `{"chat_completions", "bedrock_converse", "anthropic_messages"}` (with `chat_completions` being the primary lane examined here).
2. Effective `finish_reason` is `"length"` (either reported natively by the provider, stringified from integer status codes, or set during streaming assembly when JSON arguments are incomplete).
3. The normalized `assistant_message` is not `None`.
4. `bool(getattr(assistant_message, "tool_calls", None))` is `True`.

### 3.2 Guardrail Preemption

In [`agent/conversation_loop.py`](file:///home/eins0fx/development/hermes-agent-port/agent/conversation_loop.py#L4202-L4264), the pre-continuation guardrails specifically evaluate `not _trunc_has_tool_calls`:

- **Thinking Budget Exhaustion**: Evaluated at line 4202:
  ```python
  _thinking_exhausted = (
      not _trunc_has_tool_calls
      and _has_think_tags
      and (
          (_trunc_content is not None and not agent._has_content_after_think_block(_trunc_content))
          or _trunc_content is None
      )
  )
  ```
  When `_trunc_has_tool_calls` is `True`, `_thinking_exhausted` is strictly `False`. Even if the model output consists solely of `<think>...</think>` scratchpad blocks followed immediately by a truncated tool call, the agent does not abort. It proceeds to retry the tool call.

- **Repetition Dominated Abort**: Evaluated at line 4260:
  ```python
  _repetition_dominated = (
      not _trunc_has_tool_calls
      and bool(_visible_trunc)
      and is_repetition_dominated(_visible_trunc)
  )
  ```
  When `_trunc_has_tool_calls` is `True`, `_repetition_dominated` is strictly `False`. A tool call payload that happens to repeat keys or values does not trip the repetition abort guard.

- **Content-Filter Stream Stall**: Evaluated at line 4312:
  ```python
  _cf_terminated = getattr(response, "_content_filter_terminated", False)
  if _cf_terminated and agent._fallback_index < len(agent._fallback_chain):
  ```
  Unlike thinking exhaustion and repetition, the content-filter safety check occurs before the tool-call branch. If an upstream safety filter aborts the stream mid-tool-call and sets `_content_filter_terminated = True`, the engine immediately activates the fallback provider on pass 1 without burning tool truncation retries. If no fallback is configured, it falls through to tool retry.

### 3.3 Router Finish-Reason Rewrite (`finish_reason == "tool_calls"`)

Proxy routers (e.g. OpenRouter or LiteLLM) occasionally mask output token limits by rewriting `finish_reason: "length"` to `finish_reason: "tool_calls"`. When this occurs, execution bypasses the `finish_reason == "length"` block and reaches the tool argument validation gate in [`agent/conversation_loop.py`](file:///home/eins0fx/development/hermes-agent-port/agent/conversation_loop.py#L7877-L7909):

```python
if invalid_json_args:
    _truncated = any(
        not (tc.function.arguments or "").rstrip().endswith(("}", "]"))
        for tc in assistant_message.tool_calls
        if tc.function.name in {n for n, _ in invalid_json_args}
    )
    if _truncated:
        agent._vprint(
            f"{agent.log_prefix}⚠️  Truncated tool call arguments detected "
            f"(finish_reason={finish_reason!r}) - refusing to execute.",
            force=True,
        )
        ...
        return {
            "final_response": "Response truncated due to output length limit",
            "messages": messages,
            "api_calls": api_call_count,
            "completed": False,
            "partial": True,
            "error": "Response truncated due to output length limit",
        }
```

- **Zero-Retry Refusal**: If arguments do not end with a valid closing bracket (`}` or `]`), the engine recognizes the router rewrite and refuses execution immediately (0 retries granted).
- **Formatting Mistake Retries**: Conversely, if arguments do end with `}` or `]` but contain syntax mistakes (e.g. `{"key": }`), it is classified as a model formatting error and granted up to 3 retries tracked by `agent._invalid_json_retries`.

---

## 4. Same-Request Retry versus Semantic Continuation

The architecture enforces a strict dichotomy between ordinary text continuation and tool call truncation:

| Behavioral Dimension | Ordinary Text Continuation | Truncated Tool Call Continuation |
| :--- | :--- | :--- |
| **Trigger** | `finish_reason == "length"` and `not tool_calls` | `finish_reason == "length"` and `tool_calls` |
| **Continuation Paradigm** | Semantic continuation (multi-turn extension) | Same-request retry (single-turn re-execution) |
| **Assistant Fragment Appended** | Yes (`_length_continuation_fragment: True`) | **No** (broken response discarded from messages) |
| **User Nudge Appended** | Yes (`_length_continuation_nudge: True`) | **No** (zero synthetic user messages added) |
| **Message History State** | Grows by 2 messages per continuation pass | **Completely unchanged** across all retry attempts |
| **Wire Payload** | Replays accumulated history + user nudge | Exact same messages as initial request |
| **Token Cap Adjustment** | Ephemerally doubled up to 32,768 cap | Ephemerally doubled up to 32,768 cap |
| **Reasoning Override** | `_ephemeral_reasoning_off = True` if thinking-only | Reasoning configuration preserved |
| **Ceiling Result** | Collapses fragments via `_join_truncated_parts` | Returns terminal failure; incomplete tools never run |

### 4.1 Invariant: Transcript Non-Pollution

A broken tool call must never be appended to `messages`. Appending an assistant turn with invalid `tool_calls` JSON creates invalid transcripts that strict upstream providers (e.g. OpenAI, Anthropic, Gemini) reject on subsequent turns with HTTP 400 (`"invalid tool call arguments"` or `"dangling tool_use without matching tool_result"`).

By re-running the same API call from current message state without appending the broken response, the agent avoids poisoning conversation history.

---

## 5. Retry Budget and Exponential Output-Cap Growth Schedule

### 5.1 Retry Counter Lifecycle

- **Counter Initialization**: `truncated_tool_call_retries = 0` at turn start ([`agent/conversation_loop.py:2218`](file:///home/eins0fx/development/hermes-agent-port/agent/conversation_loop.py#L2218)).
- **Retry Gate**: Checked at line 4518:
  ```python
  if truncated_tool_call_retries < 4:
      truncated_tool_call_retries += 1
      ...
      continue
  ```
- **Ceiling Threshold**: Exactly 4 retries are permitted.
  - Initial request: `retries == 0` (Attempt 1). Truncation detected -> `retries` becomes 1 -> `continue`.
  - Retry 1: `retries == 1` (Attempt 2). Truncation detected -> `retries` becomes 2 -> `continue`.
  - Retry 2: `retries == 2` (Attempt 3). Truncation detected -> `retries` becomes 3 -> `continue`.
  - Retry 3: `retries == 3` (Attempt 4). Truncation detected -> `retries` becomes 4 -> `continue`.
  - Retry 4: `retries == 4` (Attempt 5). Truncation detected -> `retries < 4` is `False` -> ceiling exit.
- **Turn Reset**: Upon any successful tool execution in the turn, `truncated_tool_call_retries` is reset to 0 ([`agent/conversation_loop.py:8195`](file:///home/eins0fx/development/hermes-agent-port/agent/conversation_loop.py#L8195)) so that a subsequent tool truncation in a multi-step workflow receives a fresh retry budget.

### 5.2 Exponential Token Cap Schedule

The ephemeral token cap for retry `r` in `1..=4` is determined by lines 4539-4545:

```python
_tc_boost_base = agent.max_tokens if agent.max_tokens else 4096
_tc_boost = _tc_boost_base * (2 ** truncated_tool_call_retries)
_tc_requested_cap = agent._requested_output_cap_from_api_kwargs(api_kwargs)
if _tc_requested_cap is not None:
    _tc_boost = max(_tc_boost, _tc_requested_cap)
_tc_boost_cap = max(32768, _tc_requested_cap or 0)
agent._ephemeral_max_output_tokens = min(_tc_boost, _tc_boost_cap)
```

#### Exact Mathematical Schedules:

1. **Default Configuration (`agent.max_tokens is None`, base = 4096)**:
   - Initial call: Provider default (e.g. `None` / wire default).
   - Retry 1 (`r=1`): `4096 * (2 ** 1) = 8,192`. Cap = `max(32768, 0) = 32768`. Result = `8,192`.
   - Retry 2 (`r=2`): `4096 * (2 ** 2) = 16,384`. Cap = `32768`. Result = `16,384`.
   - Retry 3 (`r=3`): `4096 * (2 ** 3) = 32,768`. Cap = `32768`. Result = `32,768`.
   - Retry 4 (`r=4`): `4096 * (2 ** 4) = 65,536`. Cap = `32768`. Result = `min(65536, 32768) = 32,768`.

2. **Low Base (`agent.max_tokens = 1024`, base = 1024)**:
   - Retry 1 (`r=1`): `1024 * 2 = 2,048`.
   - Retry 2 (`r=2`): `1024 * 4 = 4,096`.
   - Retry 3 (`r=3`): `1024 * 8 = 8,192`.
   - Retry 4 (`r=4`): `1024 * 16 = 16,384`.

3. **High Base (`agent.max_tokens = 8192`, base = 8192)**:
   - Retry 1 (`r=1`): `8192 * 2 = 16,384`.
   - Retry 2 (`r=2`): `8192 * 4 = 32,768`.
   - Retry 3 (`r=3`): `8192 * 8 = 65,536 -> capped at 32,768`.
   - Retry 4 (`r=4`): `8192 * 16 = 131,072 -> capped at 32,768`.

4. **Caller-Requested Cap Exceeding Default Ceiling (`requested_cap = 65536`)**:
   - `_tc_boost_cap = max(32768, 65536) = 65,536`.
   - `_tc_boost = max(_tc_boost, 65536) = 65,536`.
   - All retries 1..4 receive `65,536`.

### 5.3 One-Shot Ephemeral Consumption

The ephemeral cap is stored in `agent._ephemeral_max_output_tokens`. In [`agent/chat_completion_helpers.py`](file:///home/eins0fx/development/hermes-agent-port/agent/chat_completion_helpers.py#L2033), `_build_api_kwargs` reads and immediately zeroes it:

```python
ephemeral_out = getattr(agent, "_ephemeral_max_output_tokens", None)
if ephemeral_out is not None:
    agent._ephemeral_max_output_tokens = None  # consume immediately
```

This guarantees that if the request fails or is interrupted, the boosted cap never leaks into subsequent unrelated turns.

---

## 6. Tool Execution Prohibition and Safety Invariants

### 6.1 Safety Rationale

A tool call is a remote procedure call issued by the model to interact with the external world (e.g. `write_file`, `execute_code`, `delete_file`, `browser_action`). When `finish_reason == "length"`:
- The argument string is truncated mid-JSON.
- String literals inside JSON are unclosed (e.g. `'{"path": "foo.py", "content": "print("hell'`).
- Parameters are partially emitted or missing entirely.

Attempting to deserialize or execute this partial argument string would either crash the tool handler with `JSONDecodeError` or, worse, execute an operation with corrupt or truncated inputs.

### 6.2 Invariants

1. **No Partial Tool Execution**: If any tool call in a response is truncated, `run_agent.handle_function_call` is **never called** for that response.
2. **Atomicity Across Parallel Calls**: If a response returns multiple parallel tool calls where call 0 is complete and valid but call 1 is truncated, the entire response is rejected. Call 0 is not executed in isolation.
3. **No Coerced Defaults**: Truncated arguments are never replaced with `{}` or default arguments during truncation retry.

---

## 7. Partial-Stream Stubs, Stalls, and Dropped Tool Names

### 7.1 The `_is_stub_stall` Distinction

In streaming mode, a connection drop can occur while the model is emitting a tool call. Hermes distinguishes two fundamentally different root causes:

1. **Genuine Output-Cap Truncation**:
   - The provider sends `finish_reason == "length"` alongside the incomplete arguments.
   - The response has a regular stream ID (e.g. `stream-uuid` or `chatcmpl-...`).
   - `_is_stub_stall = False`.
   - Meaning: The model ran out of tokens while generating arguments. Boosting `max_tokens` gives the model more space to finish.

2. **Network Stream Stall**:
   - The upstream SSE stream closes with no `finish_reason` and no `[DONE]`.
   - Created via `_build_partial_stream_stub` with `id = PARTIAL_STREAM_STUB_ID`.
   - If `tool_calls` is preserved on the stub, `_is_stub_stall = True`.
   - Meaning: The network or peer connection died mid-generation. Boosting `max_tokens` is not strictly necessary, but is harmless.

#### Diagnostic Logging and Terminal Failure Matrix:

| Response Type | `_is_stub_stall` | Retry Log (Buffer) | Ceiling Log (Terminal) | `_final_response` & `error` |
| :--- | :--- | :--- | :--- | :--- |
| **Genuine Length** | `False` | `⚠️ Truncated tool call detected - retrying API call ({r}/4)...` | `⚠️ Truncated tool call response detected again - refusing to execute incomplete tool arguments.` | `"Response truncated due to output length limit"` |
| **Network Stall** | `True` | `⚠️ Stream interrupted mid tool-call - retrying ({r}/4)...` | `⚠️ Stream kept dropping mid tool-call after 4 retries - the action was not executed.` | `"Stream repeatedly dropped mid tool-call (network); the tool was not executed"` |

### 7.2 Zero-Byte Tool Arg Drops (`_tool_args_dropped_no_finish`)

A unique edge case occurs when the stream drops immediately after emitting the tool name, before a single byte of arguments arrives (issue #80498). In [`agent/chat_completion_helpers.py:4825-4842`](file:///home/eins0fx/development/hermes-agent-port/agent/chat_completion_helpers.py#L4825-L4842):

```python
_tool_args_dropped_no_finish = has_truncated_tool_args and finish_reason is None
if _tool_args_dropped_no_finish:
    _dropped_names = [
        (tool_calls_acc[idx]["function"]["name"] or "?")
        for idx in sorted(tool_calls_acc)
    ]
    return _build_partial_stream_stub(
        role, full_content,
        "".join(reasoning_parts) or None,
        model_name, usage_obj,
        dropped_tool_names=_dropped_names or None,
    )
```

In `_build_partial_stream_stub`:
- `mock_message.tool_calls = None` (cleared so the stub cannot be mistaken for executable tool calls).
- `response._dropped_tool_names = _dropped_names`.
- `choices[0].finish_reason = "length"`.
- `response.id = PARTIAL_STREAM_STUB_ID`.

Because `tool_calls` is `None`, this stub enters the **text continuation lane** with dropped-tools metadata:
- `_get_continuation_prompt(True, dropped_tools)` is invoked.
- Formats prompt: `[System: Your previous tool call ({tool_list}) was too large and the stream timed out before it could be delivered. Do NOT retry the same tool call with the same large content. Instead, break the content into multiple smaller tool calls...]`.
- `tool_list` is capped at the first 3 names: `", ".join(dropped_tools[:3])`.
- Injects a synthetic user continuation nudge instructing the model to chunk its output.

---

## 8. Terminal Result Shape, Transcript Repair, and Persistence

### 8.1 Terminal Result Structure

When all 4 retries are exhausted, the conversation loop terminates early and returns:

```python
return {
    "final_response": _final_response,
    "messages": messages,
    "api_calls": api_call_count,
    "completed": False,
    "partial": True,
    "error": _final_response,
}
```

- `completed`: `False`.
- `partial`: `True`.
- `error`: Equal to `_final_response` (`"Response truncated due to output length limit"` or `"Stream repeatedly dropped mid tool-call (network); the tool was not executed"`).
- `api_calls`: Outer turn iteration index (`api_call_count`), which remains 1 for a failure on the first turn.

### 8.2 Transcript Repair (`close_interrupted_tool_sequence`)

Because the loop terminates with an early `return` without passing through `finalize_turn`, it must repair any dangling tool execution tail before persisting session history ([`agent/conversation_loop.py:4571`](file:///home/eins0fx/development/hermes-agent-port/agent/conversation_loop.py#L4571)):

```python
close_interrupted_tool_sequence(messages, _final_response)
agent._persist_session(messages, conversation_history)
```

[`close_interrupted_tool_sequence`](file:///home/eins0fx/development/hermes-agent-port/agent/message_sanitization.py#L296) inspects `messages[-1]`:
- If `messages[-1]["role"] == "tool"`: Appends a synthetic assistant turn:
  ```json
  {
    "role": "assistant",
    "content": "Response truncated due to output length limit"
  }
  ```
  This resolves the role alternation violation (`tool -> user`) on the next user turn, preventing strict providers (Claude, Gemini) from rejecting subsequent prompts with HTTP 400 or losing context.
- If `messages[-1]["role"] != "tool"` (e.g. `role == "user"` when truncation occurred on the initial request): It performs **no operation** (`returns False`).

### 8.3 Persistence Guarantee

- The broken assistant response (containing incomplete `tool_calls`) is **never added** to `messages`.
- `agent._persist_session` writes only the clean messages plus any synthetic closing assistant turn to disk.

---

## 9. Usage, Cost, and Observable Call Accounting

### 9.1 Accounting Invariants

1. **Rejected Calls Are Not Billed**:
   - Lines 4612-4630 of `conversation_loop.py` handle token usage and cost accounting.
   - Truncated tool responses either execute `continue` (during retries 1..4) or `return` (upon ceiling exit) before reaching line 4615.
   - Therefore, `agent.session_api_calls` is **not incremented** for truncated calls.
   - `agent.session_total_tokens`, `agent.session_prompt_tokens`, and `agent.session_completion_tokens` remain unchanged.
   - `agent.session_estimated_cost_usd` is not billed for rejected tokens.
2. **Billing on Successful Recovery**:
   - If attempt `k` succeeds (e.g. valid tool call arguments returned), execution falls through to lines 4615+.
   - `agent.session_api_calls += 1`.
   - Tokens from the successful attempt's `usage` block are recorded normally.
3. **Underlying Network Call Observability**:
   - While `agent.session_api_calls` tracks clean logical turns, the underlying transport client issues all 5 HTTP requests.
   - Each attempt is recorded in middleware logs with `call_role: "primary"` and the corresponding `retry_count`.

---

## 10. Provider-Specific Nuances and Edge Cases

### 10.1 Ollama GLM Streaming Argument Repair

Local Ollama instances serving GLM models frequently emit minor JSON syntax errors or trailing commas due to sampling artifacts. In [`agent/chat_completion_helpers.py:4754-4767`](file:///home/eins0fx/development/hermes-agent-port/agent/chat_completion_helpers.py#L4754-L4767), during streaming assembly:

```python
try:
    json.loads(arguments)
except json.JSONDecodeError:
    repaired = _repair_tool_call_arguments(arguments, tool_name)
    if repaired != "{}":
        arguments = repaired
    else:
        has_truncated_tool_args = True
```

- **Repairable Malformations**: Trailing commas (`{"a": 1,}`), missing final curly brackets (`{"a": 1`), and Python `None` literals (`None` -> `{}`) are repaired to valid JSON. `has_truncated_tool_args` remains `False`, allowing tool execution to proceed without triggering truncation retries.
- **Unrepairable Mid-String Cutoffs**: When text is cut off inside a string literal (`{"path": "foo.py", "content": "part`), `_repair_tool_call_arguments` returns `"{}"`. This causes `has_truncated_tool_args = True`, properly routing the response into tool truncation retry.

### 10.2 Gemini Thought Signatures (`thought_signature`)

Gemini 3 thinking models attach `thought_signature` metadata within `extra_content` on each tool call. Without this signature replayed on subsequent requests, Gemini rejects the call with HTTP 400.
- `ChatCompletionsTransport.normalize_response` stores this in `ToolCall.provider_data["extra_content"]`.
- The `ToolCall.extra_content` property exposes it.
- During tool truncation same-request retry, because `messages` is untouched, thought signatures from earlier completed turns are preserved.

### 10.3 Strict Empty-Assistant Rejection (Moonshot / Kimi)

Providers like Moonshot (Kimi) reject any request containing an assistant message with empty content (`{"role": "assistant", "content": ""}`) with HTTP 400.
- When tool truncation retries execute, zero assistant messages are appended.
- When dropped-tools stubs arrive with zero text delivered, the empty assistant turn is omitted entirely, appending only the user nudge.

### 10.4 xAI Reserved Tool Names (`tool_search` Alias)

xAI chat-completions reserves the function name `tool_search` for its server-side web tool.
- The transport aliases client declarations to `hermes_tool_search`.
- In `normalize_response`, the alias is un-mapped back to `tool_search`.
- Truncated tool calls retain their canonical name across the retry cycle.

---

## 11. Boundary Notes and Separation Matrix

This lane operates strictly within the boundaries defined below:

```
+-----------------------------------------------------------------------------------+
|                              API Transport Request                                |
+-----------------------------------------------------------------------------------+
                                         |
               +-------------------------+-------------------------+
               |                                                   |
       [api_mode == chat_completions]                    [Other API Modes]
               |                                                   |
        [finish_reason == length]                       +--------------------+
               |                                        | - anthropic_msgs   |
       +-------+-------+                                | - codex_responses  |
       |               |                                +--------------------+
   [No Tools]     [Has Tools]
       |               |
       v               v
+--------------+ +------------------------------------------------------------------+
| Text Length  | | MAIN-PROVIDER TOOL TRUNCATION LANE                               |
| Continuation | | - Same-request retry (0 messages appended)                       |
| Lane         | | - 4 retry attempts max (5 calls total)                           |
| (Fragments & | | - Ephemeral cap doubling: base * (2^r) [max 32768]               |
| Nudges)      | | - Prohibits tool execution                                       |
+--------------+ | - Distinguishes _is_stub_stall vs genuine length                 |
                 | - Repairs tool tail via close_interrupted_tool_sequence           |
                 +------------------------------------------------------------------+
```

### Detailed Boundary Specifications:

1. **Ordinary Text Length Continuation**:
   - Gated on `assistant_message is not None and not _trunc_has_tool_calls`.
   - Accumulates partial fragments in `truncated_response_parts`.
   - Appends interim assistant fragments (`_length_continuation_fragment`) and user nudges (`_length_continuation_nudge`).
   - Collapses fragments into settled text on ceiling exit.
   - *Boundary*: Text continuation is strictly separate; it never retries the same request without appending scaffolding.

2. **Codex Responses (`api_mode == "codex_responses"`)**:
   - Uses OpenAI Responses API (`/v1/responses`).
   - Normalizes truncation as `status: "incomplete"`, `incomplete_reason: "max_output_tokens"`.
   - Uses dedicated `_CODEX_INCOMPLETE_NUDGE` and merges native reasoning items.
   - *Boundary*: Codex Responses bypasses chat-completions conversation loop handling entirely.

3. **Anthropic Messages (`api_mode == "anthropic_messages"`)**:
   - Uses Anthropic Messages SDK.
   - Maps wire `stop_reason: "max_tokens"` to normalized `finish_reason: "length"`.
   - Handles OAuth tool prefix stripping and content block structures.

4. **Response-Stall Timeout Watchdog**:
   - Governed by background stream watchdog threads and socket read timeouts.
   - When a socket hangs mid-stream, the watchdog terminates the stream and triggers error handling in `agent/chat_completion_helpers.py`.
   - *Boundary*: The watchdog detects connection stalls; this lane consumes the resulting `PARTIAL_STREAM_STUB_ID` stub.

---

## 12. Golden Suite Verification and Test Parity

The behavioral contract established in this document is verified by live Python execution in [`rust/tools/gen_main_provider_tool_truncation_goldens.py`](file:///home/eins0fx/development/hermes-agent-port/rust/tools/gen_main_provider_tool_truncation_goldens.py), emitting [`rust/tools/main-provider-tool-truncation-goldens.json`](file:///home/eins0fx/development/hermes-agent-port/rust/tools/main-provider-tool-truncation-goldens.json).

### 12.1 Execution and Parity Commands

```bash
# Generate deterministic golden test corpus (81 test cases across 11 sections)
.venv/bin/python rust/tools/gen_main_provider_tool_truncation_goldens.py

# Verify exact byte parity of golden corpus
.venv/bin/python rust/tools/gen_main_provider_tool_truncation_goldens.py --check

# Run focused Python test suites verifying tool truncation behaviors
.venv/bin/pytest tests/run_agent/test_run_agent.py -k "truncated_tool" -v
.venv/bin/pytest tests/run_agent/test_streaming_tool_call_repair.py -v
.venv/bin/pytest tests/agent/test_close_interrupted_tool_sequence.py -v
.venv/bin/pytest tests/run_agent/test_partial_stream_finish_reason.py -k "Tool" -v
```

Executed result: **14 passed** (4 truncated-tool cases, 2 streaming-repair
cases, 3 interrupted-sequence cases, and 5 partial-stream tool cases).

### 12.2 Corpus Summary and Section Breakdown

The generated golden corpus contains **81 test cases across 11 contract sections**:

1. `section_01_eligibility_and_preemption` (8 cases): Tests eligibility rules, preemption of thinking-exhaustion and repetition guards by tool calls, and router rewrite refusals.
2. `section_02_same_request_retry_vs_semantic_continuation` (4 cases): Verifies message transcript non-pollution, absence of fragments/nudges, and dropped-tool stub semantic prompts.
3. `section_03_retry_progression_and_ceiling` (10 cases): Tracks step-by-step retry progression from attempt 0 to 4, ceiling exit, and recoveries on attempts 1, 2, 3, and 4.
4. `section_04_output_cap_exponential_growth` (19 cases): Evaluates base 4096, base 1024, base 8192, requested cap overrides (65536, 20000), cap extraction priority, and one-shot consumption.
5. `section_05_tool_execution_prohibition` (3 cases): Proves that incomplete arguments, empty arguments, and mixed parallel calls are never executed.
6. `section_06_partial_stream_stub_distinctions` (2 cases): Tests genuine length versus `PARTIAL_STREAM_STUB_ID` log messages and terminal error payloads.
7. `section_07_dropped_tool_names_and_streaming_drops` (7 cases): Audits zero-byte stream drops, dropped-tool names on stubs, prompt formatting, and capping at 3 names.
8. `section_08_terminal_result_and_transcript_repair` (5 cases): Validates terminal result dictionary shapes and `close_interrupted_tool_sequence` role alternation repairs.
9. `section_09_persistence_and_usage_accounting` (3 cases): Confirms unbilled token usage on failure, zero increment to `session_api_calls`, and proper billing on successful recovery.
10. `section_10_provider_specific_exceptions_and_fallbacks` (14 cases): Exercises content-filter stream stalls with fast failover, Ollama argument repairs, Gemini thought signatures, and Poolside integer finish reasons.
11. `section_11_boundaries_and_separation_matrix` (6 cases): Establishes formal boundary distinctions against text continuation, Codex Responses, Anthropic Messages, and response stall watchdogs.

### 12.3 Invariants, Uncertainties, and Deferrals

- **Invariant 1**: Tool arguments that fail JSON deserialization or are truncated mid-payload must never be passed to tool handlers.
- **Invariant 2**: Incomplete assistant messages containing truncated tool calls must never be appended to session history.
- **Invariant 3**: Retrying truncated tool calls must never exceed 4 retry attempts (5 total attempts).
- **Invariant 4**: Ephemeral token cap boosts must be consumed on the immediate next request and never persist into future turns.
- **Invariant 5**: The transcript must maintain valid role alternation upon ceiling exit; trailing tool messages must be closed with a synthetic assistant turn.
- **Uncertainty 1 (Provider Token Cap Variance)**: Upstream providers may enforce internal hard ceilings below 32,768 (e.g. 4,096 or 8,192). In such environments, exponential boosts beyond the provider ceiling are capped by the upstream API rather than the client.
- **Deferral 1 (Partial JSON Streaming Repair)**: Heuristic argument repair for broken strings mid-token is deferred to dedicated tool argument parser lanes; this lane enforces strict rejection of unrepairable JSON.
- **Deferral 2 (Rust Gateway Implementation)**: This specification and golden corpus define the authoritative contract for the upcoming Rust gateway port; no Rust production code was modified.
