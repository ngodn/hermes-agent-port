# Native Main-Provider Length-Truncation Content Guards Contract

**Document Target**: `rust/analysis/main-provider-truncation-guards-agy.md`
**Evidence Lane**: Python Chat-Completions HTTP Success Length-Truncation Guards
**Primary Sources**:
- [`agent/conversation_loop.py`](file:///home/eins0fx/development/hermes-agent-port/agent/conversation_loop.py)
- [`agent/repetition_guard.py`](file:///home/eins0fx/development/hermes-agent-port/agent/repetition_guard.py)
- [`agent/chat_completion_helpers.py`](file:///home/eins0fx/development/hermes-agent-port/agent/chat_completion_helpers.py)
- [`run_agent.py`](file:///home/eins0fx/development/hermes-agent-port/run_agent.py)
- [`tests/run_agent/test_length_continuation_thinking_exhaustion.py`](file:///home/eins0fx/development/hermes-agent-port/tests/run_agent/test_length_continuation_thinking_exhaustion.py)
- [`tests/agent/test_repetition_guard.py`](file:///home/eins0fx/development/hermes-agent-port/tests/agent/test_repetition_guard.py)
- [`tests/run_agent/test_continuation_repetition_guard.py`](file:///home/eins0fx/development/hermes-agent-port/tests/run_agent/test_continuation_repetition_guard.py)
- [`rust/tools/gen_main_provider_truncation_guard_goldens.py`](file:///home/eins0fx/development/hermes-agent-port/rust/tools/gen_main_provider_truncation_guard_goldens.py)
- [`rust/tools/main-provider-truncation-guard-goldens.json`](file:///home/eins0fx/development/hermes-agent-port/rust/tools/main-provider-truncation-guard-goldens.json)

---

## 1. Scope Boundary

This document specifies the authoritative runtime semantics for Python's three ordinary chat-completions content-level truncation guards evaluated when an upstream response finishes with `finish_reason == "length"`:
1. **Thinking-Exhausted Detection**: Early abort when reasoning tags consume the entire output token cap with zero visible response text.
2. **Repetition-Dominated Rejection**: Immediate abort when visible text enters a degenerate echo loop (incident #86581), preventing tens of thousands of duplicate characters from being stitched into transcript history.
3. **Empty Reasoning-Only One-Shot Reasoning Disable**: One-shot reasoning suppression and progressive token cap boosting when a model returns length truncation with reasoning delivered in a separate field (or empty visible text without inline think tags), while suppressing empty interim assistant messages to prevent upstream HTTP 400 errors.

### Explicit Out-of-Scope Exclusions
As mandated:
- Dropped streaming connections and partial stream stubs (`PARTIAL_STREAM_STUB_ID`)
- Dropped tool call name heuristics and dropped tool continuation prompts
- Local Ollama/GLM stop-to-length correction (`_should_treat_stop_as_truncated`)
- Request timeouts, client network retries, and socket backoff
- Non-chat transports (Codex responses, tool-call truncation retry lane, Anthropic raw messages, Bedrock converse)

---

## 2. Executive Architectural Overview

When an LLM hits its output token ceiling (`finish_reason == "length"`), naive continuation (issuing `"continue where you left off"`) fails catastrophically in two real-world operational regimes:
1. **The Reasoning Echo / Exhaustion Trap**: When a reasoning model spends its entire output token limit thinking, re-prompting with reasoning enabled causes the model to re-derive its scratchpad from scratch. It burns the full budget again, repeatedly emitting zero visible tokens until the turn times out or errors.
2. **The Degenerate Repetition Loop (Issue #86581)**: When a model enters a pathological phrase echo loop, continuing the response stitches duplicates into the transcript. In the incident behind #86581, this produced a 60,698-character response across 31 Discord messages.

To prevent both failures while still enabling legitimate truncations to continue smoothly, Python implements three tightly sequenced content guards.

```
       [Response Received: finish_reason == "length"]
                           |
                           v
        [Assistant Message has tool_calls?]
              /                        \
           (Yes)                       (No)
            /                            \
           v                              v
[Preempted: Route to             [Guard 1: Thinking Exhausted?]
 Tool Truncation Lane]           (_has_think_tags & no visible text)
                                       /                    \
                                    (Yes)                   (No)
                                     /                        \
                                    v                          v
                        [ABORT Turn Immediately]      [Guard 2: Repetition Dominated?]
                        - 0 retries consumed          (is_repetition_dominated)
                        - User guidance emitted             /                  \
                        - Transcript clean               (Yes)                 (No)
                                                          /                      \
                                                         v                        v
                                            [ABORT Turn Immediately]   [Guard 3: Empty Reasoning?]
                                            - 0 retries consumed       (content == "" or None)
                                            - Degenerate text dropped        /             \
                                            - Transcript clean            (Yes)            (No)
                                                                           /                 \
                                                                          v                   v
                                                           [One-Shot Reasoning Off]   [Normal Text Continuation]
                                                           - Suppress empty assistant - Append interim assistant
                                                           - Arm _ephemeral_off = True- Continuation nudge
                                                           - Progressive cap boost    - Progressive cap boost
```

---

## 3. Exact Execution Ordering & Decision Pipeline

The evaluation sequence occurs in [`agent/conversation_loop.py`](file:///home/eins0fx/development/hermes-agent-port/agent/conversation_loop.py#L4149-L4400) under the `if finish_reason == "length":` block:

### Step 1: Normalization & Preemption Gate
1. Response is normalized via `_trunc_transport.normalize_response(response)`.
2. Extract `_trunc_content = getattr(_trunc_msg, "content", None)`.
3. Extract `_trunc_has_tool_calls = bool(getattr(_trunc_msg, "tool_calls", None))`.
4. **Preemption Rule**: If `_trunc_has_tool_calls` is `True`, both thinking-exhausted detection and repetition-dominated rejection are strictly bypassed. Tool calls take precedence and route to the tool truncation retry lane.

### Step 2: Guard 1 -- Thinking-Exhausted Detection
Evaluated at lines 4195-4209:
```python
_has_think_tags = bool(
    _trunc_content and re.search(
        r'<(?:think|thinking|reasoning|REASONING_SCRATCHPAD)[^>]*>',
        _trunc_content,
        re.IGNORECASE,
    )
)
_thinking_exhausted = (
    not _trunc_has_tool_calls
    and _has_think_tags
    and (
        (_trunc_content is not None and not agent._has_content_after_think_block(_trunc_content))
        or _trunc_content is None
    )
)
```
- If `_thinking_exhausted` is `True`:
  - Log warning: `"💭 Reasoning exhausted the output token budget -- no visible response was produced."`
  - Terminate immediately: 0 retries consumed.
  - Task resources cleaned via `agent._cleanup_task_resources(effective_task_id)`.
  - Session persisted via `agent._persist_session(messages, conversation_history)`.
  - Returns abort dictionary.

### Step 3: Guard 2 -- Repetition-Dominated Rejection
Evaluated at lines 4255-4265 (only reached if Guard 1 is `False`):
```python
_visible_trunc = (
    agent._strip_think_blocks(_trunc_content)
    if isinstance(_trunc_content, str)
    else _trunc_content
)
_repetition_dominated = (
    not _trunc_has_tool_calls
    and bool(_visible_trunc)
    and is_repetition_dominated(_visible_trunc)
)
```
- If `_repetition_dominated` is `True`:
  - Log warning: `"🔁 Response dominated by repeated text -- stopping instead of continuing a degenerate response."`
  - Terminate immediately: 0 retries consumed.
  - Task resources cleaned via `agent._cleanup_task_resources(effective_task_id)`.
  - Session persisted via `agent._persist_session(messages, conversation_history)`.
  - Returns abort dictionary. Degenerate fragment is dropped and not appended.

### Step 4: Guard 3 -- Empty Reasoning-Only One-Shot Disable
Evaluated at lines 4355-4395 (only reached if Guard 1 and Guard 2 are `False`):
```python
if assistant_message is not None and not _trunc_has_tool_calls:
    length_continue_retries += 1
    _interim_content = getattr(assistant_message, "content", None)
    _is_empty_partial_stub = (
        getattr(response, "id", "") == PARTIAL_STREAM_STUB_ID
        and not _interim_content
    )
    if not _interim_content and not _is_empty_partial_stub:
        agent._ephemeral_reasoning_off = True
    if _interim_content:
        interim_msg = agent._build_assistant_message(assistant_message, finish_reason)
        interim_msg["_length_continuation_fragment"] = True
        append_message(messages, interim_msg)
        truncated_response_parts.append(_interim_content)
```
- If `_interim_content` is empty/falsy and not a partial stream stub:
  - Sets `agent._ephemeral_reasoning_off = True`.
  - Skips appending interim assistant message to `messages`.
  - Skips appending to `truncated_response_parts`.
  - Appends user continuation prompt (`_LENGTH_CONTINUATION_OUTPUT_LIMIT`).
  - Sets `_retry.restart_with_length_continuation = True`.
  - Boosts output cap for continuation retry: `_boost_base * (2 ** length_continue_retries)`.

---

## 4. Guard 1: Thinking-Exhausted Detection

### 4.1 Purpose and Rationale
Models that write reasoning tokens directly into content enclosed in XML tags (e.g., DeepSeek R1, Qwen QwQ, synthetic think blocks) can consume their entire `max_tokens` allocation without ever exiting the scratchpad. Continuing with identical parameters will re-run reasoning against an even larger prompt, wasting API spend and delay.

### 4.2 Inputs and Pre-Conditions
1. `finish_reason == "length"`
2. `_trunc_has_tool_calls == False`
3. `_trunc_content`: Non-empty string containing think tag matches.
4. `agent._has_content_after_think_block(_trunc_content) == False`:
   - Calls `agent._strip_think_blocks(content)`.
   - Returns `bool(cleaned.strip())`.

### 4.3 Tag Variant Matching
`re.search` evaluates case-insensitively for:
- `<think>` / `</think>`
- `<thinking>` / `</thinking>`
- `<reasoning>` / `</reasoning>`
- `<REASONING_SCRATCHPAD>` / `</REASONING_SCRATCHPAD>`

Unterminated tags at start of content or after `\n` boundary are stripped to end-of-string by `strip_think_blocks`.

### 4.4 Outputs and User-Facing Messages
- **Error Field (`error`)**:
  `"Model used all output tokens on reasoning with none left for the response. Try lowering reasoning effort or increasing max_tokens."`
- **User-Facing Final Response (`final_response`)**:
```markdown
⚠️ **Thinking Budget Exhausted**

The model used all its output tokens on reasoning and had none left for the actual response.

To fix this:
→ Lower reasoning effort: `/reasoning low` or `/reasoning minimal`
→ Or switch to a larger/non-reasoning model with `/model`
```
- **Return Dict Envelope**:
```python
{
    "final_response": _exhaust_response,
    "messages": messages,
    "api_calls": api_call_count,
    "completed": False,
    "partial": True,
    "error": _exhaust_error,
}
```

### 4.5 Retry, Cap, and Usage Behavior
- **Retries Consumed**: 0 retries. The turn aborts immediately on the initial truncated response.
- **Counter Mutation**: `length_continue_retries` is NOT incremented (remains 0).
- **Cap Mutation**: No ephemeral cap boost applied.
- **Usage Accounting**: Lines 4612-4623 of `conversation_loop.py` are not reached. `agent.session_api_calls` is NOT incremented. Output tokens are not folded into `canonical_usage` or session tallies. `api_calls` in the return dictionary reports `api_call_count` (1 for first attempt).

### 4.6 Transcript and Persistence Behavior
- The truncated assistant message is discarded; it is NEVER appended to `messages`.
- No continuation nudges are appended.
- `messages` remains in its prior state (user message at tail).
- Session is persisted via `agent._persist_session(messages, conversation_history)`.
- Active task resources are cleaned via `agent._cleanup_task_resources(effective_task_id)`.

---

## 5. Guard 2: Repetition-Dominated Rejection

### 5.1 Purpose and Rationale
In incident #86581, an LLM entering a degenerate loop repeated a 25-character sentence hundreds of times until cutting off at `finish_reason == "length"`. A standard continuation prompt asked the model to continue, causing it to stitch identical echo loops across 31 consecutive messages (60,698 characters). The repetition guard halts degenerate responses before continuation nudges are generated.

### 5.2 Algorithm (`agent/repetition_guard.py`)

```python
MIN_FRAGMENT_LENGTH = 400
_REPEAT_WINDOW = 60
_MIN_REPEAT_COUNT = 5
_DOMINANCE_RATIO = 0.5
```

The algorithm uses a two-tier evaluation:

#### Tier 1: Minimum Length Gate (Fail-Open)
If `len(text) < MIN_FRAGMENT_LENGTH` (400 characters), returns `False` immediately.
Short truncations (a sentence cut mid-word or a short bullet list) legitimately reuse tokens and are safe to continue.

#### Tier 2: Fast Path -- Line-Based Repetition (`_line_repetition_dominated`)
Designed for common echo loops where the model repeats a sentence or paragraph on its own line:
1. Splits `text.splitlines()`, strips whitespace from each non-empty line.
2. Accumulates frequency counts for each normalized line: `counts[norm] += 1`.
3. Checks if any line satisfies BOTH:
   - `count >= _MIN_REPEAT_COUNT` (at least 5 repetitions)
   - `count * len(norm) >= n * _DOMINANCE_RATIO` (repeated line accounts for >= 50% of total characters)
4. If satisfied, returns `True` immediately without allocating window maps.

#### Tier 3: General Path -- Sliding Window Exact Match
Catches repetition loops that do not align to newline boundaries:
1. `window = _REPEAT_WINDOW` (60 characters).
2. `needed = max(_MIN_REPEAT_COUNT, math.ceil(n * _DOMINANCE_RATIO / window))`.
3. Slides a 60-character window across `text` 1 character at a time:
   `for i in range(n - window + 1): key = text[i : i + window]`.
4. If any 60-character window count reaches `needed`, returns `True`.
5. If the scan finishes without reaching `needed`, returns `False`.

### 5.3 Think Block Stripping Interaction
Repetition check is strictly performed on visible content:
```python
_visible_trunc = agent._strip_think_blocks(_trunc_content)
```
If a model generates repeated thoughts inside `<think>...</think>` but produces clean, non-repetitive visible prose, `is_repetition_dominated` evaluates ONLY the visible prose. Repeated scratchpad tokens do not trigger visible text aborts.

### 5.4 Preemption by Tool Calls
If `_trunc_has_tool_calls` is `True`, repetition guard is bypassed. Tool calls with repetitive argument values (e.g. repeated data arrays) are never aborted by this guard.

### 5.5 Outputs and User-Facing Messages
- **Error Field (`error`)**:
  `"Model output entered a repetition loop and was truncated mid-loop; refusing to continue a degenerate response."`
- **User-Facing Final Response (`final_response`)**:
```markdown
⚠️ **Response Stopped -- Repetition Detected**

The model fell into a repetition loop while writing this response, so continuing would only produce more repeated text. The partial response was discarded.

→ Switch to a different model with `/model`
→ Or resend your message (your conversation history is preserved)
```
- **Return Dict Envelope**:
```python
{
    "final_response": _rep_response,
    "messages": messages,
    "api_calls": api_call_count,
    "completed": False,
    "partial": True,
    "error": _rep_error,
}
```

### 5.6 Retry, Cap, and Usage Behavior
- **Retries Consumed**: 0 retries. Turn terminates immediately.
- **Counter Mutation**: `length_continue_retries` is NOT incremented.
- **Usage Accounting**: Lines 4612-4623 are not reached; usage uncaptured; `api_calls: api_call_count` returned.
- **Transcript**: The pathological fragment is discarded; not appended to `messages`. Session persisted with prior clean history.

---

## 6. Guard 3: Empty Reasoning-Only One-Shot Disable

### 6.1 Purpose and Rationale
Certain provider/model configurations (e.g., GLM-5.3-flash on ollama-cloud with `reasoning_effort=high`, or providers delivering reasoning via dedicated wire fields such as `message.reasoning` or `message.reasoning_content`) burn their entire token cap on reasoning and return `finish_reason == "length"` with `content == ""` or `None`.

The legacy continuation protocol suffered two fatal defects:
1. **Transcript Poisoning**: An empty string `{"role": "assistant", "content": ""}` was appended to history. Strict providers (Moonshot, Kimi via OpenRouter) reject empty assistant messages with HTTP 400 (`"message ... with role 'assistant' must not be empty"`), permanently corrupting the session.
2. **Futile Re-Thinking Loop**: Continuations never replay prior reasoning tokens. Re-issuing the request with reasoning enabled causes the model to re-think against a larger context, burning the cap 4 consecutive times and dying with `"Response remained truncated after 4 continuation attempts"`.

The fix:
1. Skip appending empty assistant fragments.
2. Arm `agent._ephemeral_reasoning_off = True` so the continuation request disables reasoning for exactly one call, forcing the model to spend its output cap writing the visible answer.

### 6.2 Trigger Condition
Evaluated when:
- `finish_reason == "length"`
- `_trunc_has_tool_calls == False`
- `not _interim_content` (`content` is `""` or `None`)
- `not _is_empty_partial_stub` (normal response ID, NOT a dropped stream stub)

### 6.3 Ephemeral Reasoning Override Lifecycle

#### Step A: Arming
In `conversation_loop.py:4390`:
`agent._ephemeral_reasoning_off = True`

#### Step B: Atomic Consumption
In `agent/chat_completion_helpers.py`:
```python
def _consume_ephemeral_reasoning_off(agent) -> bool:
    if getattr(agent, "_ephemeral_reasoning_off", False):
        agent._ephemeral_reasoning_off = False
        return True
    return False
```
`_reasoning_config_for_wire(agent)` consumes the flag on the first call to `build_api_kwargs`:
- If `ephemeral_off` is `True`:
  `cfg = {**(cfg or {}), "enabled": False, "effort": "none"}`
- Wire request body carries reasoning disabled (e.g., OpenRouter `extra_body.reasoning`).

#### Step C: One-Shot Guarantee
Because `_ephemeral_reasoning_off` is reset to `False` upon consumption, any subsequent request in the turn (e.g. a second continuation pass that truncates visible text) automatically sees `_ephemeral_reasoning_off == False` and reverts to the user's configured reasoning settings.

#### Step D: Rejected Disable Resilience
If the active provider route previously replied to a reasoning disable with HTTP 400 (`"reasoning is mandatory"`, setting `agent._reasoning_disable_rejected = True`):
- `_reasoning_config_for_wire` consumes and discards `_ephemeral_reasoning_off`.
- Resends the user's own configuration untouched to match the provider prompt cache key and prevent 400 errors.
- If the user's config itself was a disable, returns `None` to omit reasoning arguments and defer to provider default.

#### Step E: Fresh Turn Isolation
At the very top of each new turn in `conversation_loop.py:2215`:
`agent._ephemeral_reasoning_off = False`
If an earlier turn was interrupted or failed before consuming the armed flag, the stale flag is cleared so it never affects a subsequent user turn.

### 6.4 Progressive Output Cap Boosting Schedule
Continuation retries progressively boost the ephemeral token limit:
```python
_boost_base = agent.max_tokens if agent.max_tokens else 4096
_boost = _boost_base * (2 ** length_continue_retries)
_requested_cap = agent._requested_output_cap_from_api_kwargs(api_kwargs)
if _requested_cap is not None:
    _boost = max(_boost, _requested_cap)
_boost_cap = max(32768, _requested_cap or 0)
agent._ephemeral_max_output_tokens = min(_boost, _boost_cap)
```
- Retry 1: `4096 * 2^1 = 8,192`
- Retry 2: `4096 * 2^2 = 16,384`
- Retry 3: `4096 * 2^3 = 32,768`
- Retry 4: `min(4096 * 2^4, 32768) = 32,768`
- Preserves higher base configurations (e.g., base 16,384 boosts to 32,768 on retry 1).

### 6.5 Ceiling Exit After 4 Empty Attempts
If all 4 continuation attempts return empty reasoning length truncations:
1. `length_continue_retries >= 4` trips the ceiling exit.
2. `agent._ephemeral_reasoning_off = False` is cleared to prevent leak.
3. `partial_response` is empty string.
4. Continuation nudges and interim fragments from the current turn are stripped from `messages`.
5. Emits actionable ceiling final response:
```markdown
⚠️ **No visible answer was produced.** The model hit its output-token limit on every continuation attempt -- its reasoning consumed the entire budget each time.

To fix this:
→ Lower reasoning effort: `/reasoning low` or `/reasoning none`
→ Or raise max_tokens for this model
```
6. Return envelope:
```python
{
    "final_response": _ceiling_final,
    "messages": messages,
    "api_calls": api_call_count,
    "completed": False,
    "partial": True,
    "error": "Response remained truncated after 4 continuation attempts",
}
```

---

## 7. Comparative Specification Matrix

| Dimension | Guard 1: Thinking-Exhausted | Guard 2: Repetition-Dominated | Guard 3: Empty Reasoning Disable |
| :--- | :--- | :--- | :--- |
| **Detection Site** | `conversation_loop.py:4195-4209` | `conversation_loop.py:4255-4265` | `conversation_loop.py:4355-4395` |
| **Trigger Condition** | `finish_reason == "length"` + think tags + no content after think block | `finish_reason == "length"` + `is_repetition_dominated(_visible_trunc)` | `finish_reason == "length"` + empty content + not partial stream stub |
| **Preemption** | Bypassed if `tool_calls` present | Bypassed if `tool_calls` present | Bypassed if `tool_calls` present |
| **Action** | Abort turn immediately | Abort turn immediately | Arm one-shot reasoning off; continue |
| **Retries Consumed** | 0 retries (immediate exit) | 0 retries (immediate exit) | Shares 4-attempt continuation budget |
| **Cap Boosting** | None | None | `base * 2^retry` capped at 32,768 |
| **Reasoning Wire Config**| Unchanged | Unchanged | `{"enabled": False, "effort": "none"}` for 1 call |
| **Assistant Row Appended**| No (discarded) | No (discarded) | No (suppressed to prevent HTTP 400) |
| **Continuation Nudge** | None | None | Appended (`_LENGTH_CONTINUATION_OUTPUT_LIMIT`) |
| **Error Message** | `"Model used all output tokens on reasoning..."` | `"Model output entered a repetition loop..."` | Continuation loop or ceiling exit |
| **User Final Response** | `⚠️ **Thinking Budget Exhausted**` | `⚠️ **Response Stopped -- Repetition Detected**` | Progressed turn or `⚠️ **No visible answer was produced.**` |
| **Usage Captured** | No (lines 4612-4623 skipped) | No (lines 4612-4623 skipped) | Captured only on completed continuation |
| **Transcript State** | Clean (prior turns + user msg) | Clean (prior turns + user msg) | Clean (scaffolding purged at ceiling) |

---

## 8. Edge Cases and Concrete Scenarios

### 8.1 Tool Call Presence Preemption
- **Scenario**: A response returns `finish_reason == "length"` with `tool_calls: [{"name": "write_file", "arguments": "{\"path\":\"a.txt\",\"content\":\"repeated..."}]` and `<think>I will call tool</think>`.
- **Behavior**: Neither Guard 1 nor Guard 2 triggers. Tool call presence (`_trunc_has_tool_calls == True`) routes the response directly to the tool call truncation retry lane (`truncated_tool_call_retries`).

### 8.2 Thinking Tags with Visible Content
- **Scenario**: Response is `<think>analyze data</think>Here is the summary table of results`.
- **Behavior**: `agent._has_content_after_think_block` evaluates to `True`. Guard 1 does NOT trigger. `_visible_trunc` is `"Here is the summary table of results"`. Guard 2 evaluates visible text (not repetition-dominated). Turn proceeds to normal text continuation; interim assistant message is appended.

### 8.3 Gemma `<thought>` Tags
- **Scenario**: Model outputs `<thought>thinking tokens</thought>` with `finish_reason == "length"`.
- **Behavior**: `conversation_loop.py:4196` checks regex `r'<(?:think|thinking|reasoning|REASONING_SCRATCHPAD)[^>]*>'`. The `<thought>` tag is NOT in this regex. Guard 1 does NOT trigger. (Visible text stripping still handles `<thought>`, leaving visible text empty, so it enters Guard 3 empty reasoning continuation).

### 8.4 Repetition Inside Thinking Blocks
- **Scenario**: Model repeats a phrase 1,000 times inside `<think>...</think>`, followed by a short unique answer `"Result: 42"`.
- **Behavior**: `_visible_trunc = agent._strip_think_blocks(_trunc_content)` strips the entire think block. `_visible_trunc` is `"Result: 42"`. `len(_visible_trunc) < 400`, so `is_repetition_dominated` returns `False`. The response is NOT aborted by the repetition guard.

### 8.5 Incident Echo Pattern (#86581)
- **Scenario A (Line path)**: `("Narration\n好，你幫我更改成 Google Gemini 4 31B。\n") * 800`.
  Fast path `_line_repetition_dominated` finds the echoed line appears 800 times (>= 5) and covers > 50% of the total characters. Returns `True` in under 1ms without sliding window allocations.
- **Scenario B (Sliding window path)**: `("好，你幫我更改成 Google Gemini 4 31B。") * 2000` (no line breaks).
  Fast line path finds no newlines; general path slides a 60-character window. Frequency of the repeating window exceeds `needed = max(5, ceil(n * 0.5 / 60))`. Returns `True`.

### 8.6 Short Fragment Fail-Open
- **Scenario**: Truncated text is `"Error: retry again. Error: retry again."` (length 40 characters).
- **Behavior**: `len(text) < 400`. `is_repetition_dominated` returns `False` immediately. Fails open to prevent false positives on legitimate short phrases.

### 8.7 Non-String and Malformed Content
- **Scenario**: Upstream transport delivers `content=None`, `content=12345`, or `content=[]`.
- **Behavior**: `is_repetition_dominated` checks `isinstance(text, str)` and returns `False`. No exception is thrown.

### 8.8 Stale One-Shot Flag from Interrupted Turn
- **Scenario**: Turn 1 arms `_ephemeral_reasoning_off = True`, but the turn is aborted by a user interrupt before calling the API. Turn 2 begins.
- **Behavior**: `conversation_loop.py:2215` explicitly executes `agent._ephemeral_reasoning_off = False` during turn initialization. Turn 2 executes with the user's normal reasoning configuration.

### 8.9 Mixed Multi-Pass Continuation Sequence
- **Attempt 1**: Truncates with visible text `"First chapter of the story. "`. Appended to transcript and `truncated_response_parts`.
- **Attempt 2**: Truncates with empty reasoning content `""`. `agent._ephemeral_reasoning_off = True` is armed. Empty assistant message is suppressed. User continuation nudge appended.
- **Attempt 3**: Request sent with reasoning disabled. Model generates `"Second chapter and finale."` with `finish_reason == "stop"`.
- **Final Result**: Stitches Attempt 1 and Attempt 3. `final_response` contains both chapters. Transcript contains exactly 0 empty assistant rows.

---

## 9. Verification & Test Suite Parity

All behavioral rules in this specification are verified by focused Python tests and deterministic goldens:

### 9.1 Focused Test Execution
```bash
.venv/bin/pytest -v \
  tests/run_agent/test_length_continuation_thinking_exhaustion.py \
  tests/agent/test_repetition_guard.py \
  tests/run_agent/test_continuation_repetition_guard.py \
  tests/run_agent/test_run_agent.py::TestHasContentAfterThinkBlock \
  tests/run_agent/test_run_agent.py::TestRunConversation::test_length_thinking_exhausted_skips_continuation
```
**Test Count**: Exactly 20 passed tests, 0 deselected.
- `test_length_continuation_thinking_exhaustion.py`: 10 tests
- `test_repetition_guard.py`: 6 tests
- `test_continuation_repetition_guard.py`: 2 tests
- `test_run_agent.py`: 2 tests

### 9.2 Deterministic Golden Generator
```bash
python3 rust/tools/gen_main_provider_truncation_guard_goldens.py
python3 rust/tools/gen_main_provider_truncation_guard_goldens.py --check
```
Generates 41 authoritative test cases across 3 sections in `rust/tools/main-provider-truncation-guard-goldens.json` with byte-for-byte reproducibility (SHA-256: `811fe4fe34ddc74588f78212242ddf1b012e7e8dd077c9bcdd60269a16944723`).
