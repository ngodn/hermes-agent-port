# Native Main-Provider Length Continuation Behavior Contract

**Document Target**: `rust/analysis/main-provider-length-continuation-contract-agy.md`
**Evidence Lane**: Live Python Chat-Completions HTTP Success Text Truncation Continuation Contract
**Primary Sources**:
- [`agent/conversation_loop.py`](file:///home/eins0fx/development/hermes-agent-port/agent/conversation_loop.py)
- [`agent/repetition_guard.py`](file:///home/eins0fx/development/hermes-agent-port/agent/repetition_guard.py)
- [`agent/chat_completion_helpers.py`](file:///home/eins0fx/development/hermes-agent-port/agent/chat_completion_helpers.py)
- [`agent/transports/chat_completions.py`](file:///home/eins0fx/development/hermes-agent-port/agent/transports/chat_completions.py)
- [`run_agent.py`](file:///home/eins0fx/development/hermes-agent-port/run_agent.py)
- [`rust/tools/gen_main_provider_length_continuation_goldens.py`](file:///home/eins0fx/development/hermes-agent-port/rust/tools/gen_main_provider_length_continuation_goldens.py)
- [`rust/tools/main-provider-length-continuation-goldens.json`](file:///home/eins0fx/development/hermes-agent-port/rust/tools/main-provider-length-continuation-goldens.json)

---

## 1. Executive Summary & Architectural Scope

This specification establishes the authoritative behavioral contract for native main-provider chat-completions text truncation after an HTTP 200 success (or successful SSE stream open) in the Hermes Agent architecture.

When an LLM hits its output token limit or the upstream transport drops mid-stream after delivering text, the response is incomplete. Rather than discarding partial progress or leaving an incomplete response in the user transcript, the Python runtime executes a bounded, multi-pass length continuation protocol:

1. **Finish-Reason Normalization & Misreport Detection**: Normalizes provider finish reasons (`"length"`, `"stop"`, integer status codes) and detects local Ollama GLM models that erroneously report `"stop"` when text is visibly truncated.
2. **Pre-Continuation Guardrails**: Guards against futile or pathological loops before issuing continuation calls:
   - *Thinking-budget exhaustion*: When output budget was consumed entirely by reasoning scratchpads with no visible text, aborts immediately with actionable user guidance instead of burning further API calls.
   - *Repetition-dominated truncation*: When output entered a degenerate echo loop (issue #86581), aborts immediately instead of stitching tens of thousands of duplicate characters into transcript history.
   - *Content-filter stream stall*: When provider safety filters kill a stream mid-flight, escalates to fallback providers before retrying.
3. **Continuation Prompt Construction**: Dynamically selects between normal output limit nudges, network error notices, or dropped tool-call notices (capped at 3 tool names).
4. **Synthetic Metadata & Role Alternation**: Flags interim assistant fragments (`_length_continuation_fragment`) and user nudges (`_length_continuation_nudge`), while omitting empty assistant messages for thinking-only truncations to prevent HTTP 400 errors from strict upstream providers.
5. **Request Budgets & Progressive Boosting**: Limits continuation passes to 4 attempts. Progressively doubles the ephemeral output token cap (`2^retry` multiplier) up to a 32,768 cap while preserving higher provider defaults. Ephemerally disables reasoning for thinking-only retries.
6. **Whitespace-Safe Accumulation**: Glues multi-pass fragments via `_join_truncated_parts`, ensuring words are not glued together without spacing while avoiding doubled newlines.
7. **Ceiling Exit & Wedge Prevention**: Upon exhausting 4 attempts, purges all scaffolding nudges and intermediate fragments from the current turn, collapses the partial text into a single settled assistant turn, and guarantees that subsequent user turns start unencumbered by continuation state.
8. **Stream No-Replay Boundary**: Enforces that once visible tokens are delivered to the consumer, full request replays are strictly forbidden; drops are converted to `PARTIAL_STREAM_STUB_ID` stubs driving text continuation without re-emitting already-seen text.

All behavioral contracts below are pinned by executed Python production code via [`rust/tools/gen_main_provider_length_continuation_goldens.py`](file:///home/eins0fx/development/hermes-agent-port/rust/tools/gen_main_provider_length_continuation_goldens.py), generating 104 deterministic test cases across 10 sections in [`rust/tools/main-provider-length-continuation-goldens.json`](file:///home/eins0fx/development/hermes-agent-port/rust/tools/main-provider-length-continuation-goldens.json).

---

## 2. Lifecycle Sequence and Architecture Diagram

```
                  +----------------------------------------------------+
                  |  HTTP 200 OK / SSE Stream Started Successfully     |
                  +----------------------------------------------------+
                                             |
                                             v
                  +----------------------------------------------------+
                  | ChatCompletionsTransport.normalize_response()      |
                  | - Stringify finish_reason (e.g. 24 -> "24")        |
                  | - If None: default to "stop"                       |
                  | - Check stream drop: text-only drop without usage  |
                  |   becomes PARTIAL_STREAM_STUB_ID ("length")        |
                  +----------------------------------------------------+
                                             |
                                             v
                  +----------------------------------------------------+
                  | Ollama GLM Stop Misreport Check                    |
                  | _should_treat_stop_as_truncated()                  |
                  | Local GLM + prior tool + no punct -> "length"      |
                  +----------------------------------------------------+
                                             |
                                             v
                              [finish_reason == "length"]
                               /                         \
                             Yes                          No
                             /                             \
                            v                               v
+------------------------------------------+    +--------------------------------+
| Pre-Continuation Guardrails:             |    | Finalize Turn or Tool Dispatch |
| 1. Thinking Exhaustion Check             |    +--------------------------------+
|    (_has_think_tags & no visible text)   |
|    -> ABORT: user guidance, no retry     |
| 2. Repetition Guard Check                |
|    (is_repetition_dominated)            |
|    -> ABORT: degenerate echo prevented   |
| 3. Content-Filter Stream Stall           |
|    (_content_filter_terminated)          |
|    -> Eager fallback escalation          |
+------------------------------------------+
                     |
                     v
+------------------------------------------+
| Eligibility Gate:                        |
| assistant_message and not tool_calls     |
+------------------------------------------+
           /                        \
    [Has Tool Calls]           [Text Only]
          /                            \
         v                              v
+------------------------+   +------------------------------------------+
| Tool Call Truncation   |   | length_continue_retries += 1             |
| Retry (up to 4 passes) |   |                                          |
| Re-runs same call with |   | Non-empty content:                       |
| boosted max_tokens     |   | - Build interim assistant message        |
| (SEPARATE LANE)        |   | - Tag _length_continuation_fragment      |
+------------------------+   | - Append to messages & truncated_parts   |
                             | Empty content (thinking-only):           |
                             | - Skip appending assistant message       |
                             | - Set _ephemeral_reasoning_off = True    |
                             +------------------------------------------+
                                                  |
                                                  v
                                     [length_continue_retries < 4]
                                      /                         \
                                    Yes                          No (Ceiling Hit)
                                    /                             \
                                   v                               v
+--------------------------------------------+    +------------------------------------+
| Continue Pass:                             |    | Ceiling Exit (Attempt 4 Exhausted):|
| - Select prompt via                        |    | - Join truncated_parts             |
|   _get_continuation_prompt(stub, tools)    |    | - Reset _ephemeral_reasoning_off   |
| - Append user nudge message with           |    | - Purge all _length_* messages     |
|   _length_continuation_nudge: True         |    |   from current turn start          |
| - Strip marks before wire request          |    | - Append single settled assistant  |
| - Boost max_tokens: 4096 * (2^retry)       |    |   turn with joined text            |
| - If _ephemeral_reasoning_off:             |    | - Persist session & cleanup        |
|   consume once, set effort="none"          |    | - Return completed=False,          |
| - Re-issue API call to provider            |    |   partial=True, error message      |
+--------------------------------------------+    +------------------------------------+
```

---

## 3. Finish-Reason Normalization and Misreport Heuristics

### 3.1 Transport Normalization (`ChatCompletionsTransport`)

The primary transport layer normalizes provider responses into a uniform [`NormalizedResponse`](file:///home/eins0fx/development/hermes-agent-port/agent/transports/types.py):

- **Stringification**: Providers like Poolside return integer finish reasons (e.g. `24`). The transport stringifies any integer finish reason to prevent downstream `.strip()` or string comparison crashes.
- **Absent Finish Reason**: When `choices[0].finish_reason` is `None` or missing, the transport defaults to `"stop"`.
- **Refusal Promotion**: When an OpenAI-compatible proxy returns `message.refusal` as the sole payload without text or tool calls, the transport promotes the refusal to content and sets `finish_reason = "content_filter"`.
- **Stream Drops**: In streaming mode ([`interruptible_streaming_api_call`](file:///home/eins0fx/development/hermes-agent-port/agent/chat_completion_helpers.py#L3640-L3655)):
  - If the SSE connection terminates with `finish_reason is None` after delivering text without tool calls and without a trailing usage chunk, it is classified as `_text_only_dropped_no_finish` and wrapped in `_build_partial_stream_stub` with `id = PARTIAL_STREAM_STUB_ID` and `finish_reason = "length"`.
  - Conversely, receiving an `include_usage` chunk without a `finish_reason` proves clean provider completion, retaining `finish_reason = "stop"`.

### 3.2 Ollama GLM Stop Misreport Detection (`_should_treat_stop_as_truncated`)

Local Ollama instances serving GLM models (such as `glm-4-9b`) frequently misreport mid-generation truncations as `finish_reason = "stop"`. The runtime implements a conservative heuristic in [`run_agent.py`](file:///home/eins0fx/development/hermes-agent-port/run_agent.py#L1911-L1941) to rewrite `"stop"` to `"length"` only when all six conditions are satisfied:

1. `finish_reason == "stop"` and `api_mode == "chat_completions"`.
2. Backend is a local Ollama GLM instance (`_is_ollama_glm_backend`):
   - Model name contains `glm` (or provider is `zai`).
   - Base URL points to localhost:11434 or contains `ollama`.
   - **Explicit Exclusion**: Excludes hosted Ollama Cloud (`ollama.com` in base URL) and `:cloud` model proxies (e.g. `glm-5.1:cloud`), which report finish reasons faithfully.
3. Prior conversational turns contain at least one message with `role == "tool"`.
4. Assistant message has no tool calls (`tool_calls is None` or empty).
5. Visible text (after stripping `<think>` tags) has length >= 20 characters and contains whitespace.
6. Visible text lacks a natural completion boundary (`_has_natural_response_ending`):
   - Does not end with terminal punctuation: `.`, `!`, `?`, `:`, `)`, `"`, `'`, `]`, `}`.
   - Does not end with Chinese punctuation: `。`, `！`, `？`, `：`, `）`, `】`, `」`, `』`, `》`.
   - Does not end with code fence: ` ``` `.
   - Does not end with superscript indicator: `^`.
   - Does not end with an emoji character (`ord(char) >= 0x1F300`).

When all conditions hold, the loop logs `"Treating suspicious Ollama/GLM stop response as truncated"` and updates `finish_reason = "length"`.

---

## 4. Pre-Continuation Guardrails and Eligibility Gates

Once a response is tagged with `finish_reason == "length"`, three pre-continuation guardrails execute sequentially before allocating retry budgets.

### 4.1 Guardrail 1: Thinking-Budget Exhaustion (`_thinking_exhausted`)

Reasoning models may consume their entire output token budget generating internal reasoning scratchpads without producing any visible answer:

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

- **Action**: If `_thinking_exhausted` is True, continuing is futile because the model will re-derive thinking from scratch and exhaust the cap again. The turn terminates immediately with:
  - `completed = False`, `partial = True`.
  - `error = "Model used all output tokens on reasoning with none left for the response. Try lowering reasoning effort or increasing max_tokens."`.
  - `final_response`: Actionable markdown warning recommending `/reasoning low` or `/model`.
- **Fail-Open Exemption**: Models that do not use XML think tags (e.g. GLM-4.7 on NVIDIA NIM, minimax) but return empty content are treated as normal truncations eligible for thinking-off retries rather than thinking exhaustion.

### 4.2 Guardrail 2: Repetition-Dominated Truncation (`is_repetition_dominated`)

In incident #86581, a model fell into a degenerate repetition loop, burning its entire token limit echoing a single phrase. Re-prompting with "continue where you left off" caused the loop to stitch identical repeats into a 60,698-character response across 31 messages.

- **Action**: Visible text is stripped of think blocks (`_strip_think_blocks`) and evaluated by [`is_repetition_dominated`](file:///home/eins0fx/development/hermes-agent-port/agent/repetition_guard.py#L43-L81). If True, the turn immediately aborts:
  - `completed = False`, `partial = True`.
  - `error = "Model output entered a repetition loop and was truncated mid-loop; refusing to continue a degenerate response."`.
  - `final_response`: User notification explaining repetition loop detection; partial degenerate text is discarded.

### 4.3 Guardrail 3: Content-Filter Stream Stall Fallback (#32421)

When an upstream output safety filter (e.g. Azure content filter, MiniMax 1027) terminates an active stream mid-flight, the stub is tagged `_content_filter_terminated = True`.
- **Action**: Because safety filters are content-deterministic, retrying against the same provider will reliably fail. If a fallback chain is configured, the loop activates the fallback provider immediately, rolls back messages via `_get_messages_up_to_last_assistant`, unmarks fragments, and restarts the turn against the secondary provider.
- If no fallback provider exists, it falls through to best-effort continuation.

### 4.4 Eligibility Gate

- If `assistant_message is not None and not _trunc_has_tool_calls`: Routes to **Text Length Continuation** (this lane).
- If `assistant_message is not None and _trunc_has_tool_calls`: Routes to **Tool-Call Truncation Retry** (separate lane).

---

## 5. Repeated-Fragment Detection Algorithm (`agent/repetition_guard.py`)

The repetition guard uses a two-tier algorithm designed for high speed and strict conservative thresholds:

### 5.1 Algorithmic Constants
- `MIN_FRAGMENT_LENGTH = 400`: Responses shorter than 400 characters are never evaluated (fail-open for ordinary short truncations).
- `_REPEAT_WINDOW = 60`: Window length for substring repeat matching.
- `_MIN_REPEAT_COUNT = 5`: Minimum frequency required for a window to trip the guard.
- `_DOMINANCE_RATIO = 0.5`: Occurrences of the repeated pattern must account for at least 50% of the total fragment length.

### 5.2 Fast Path: Line-Based Repetition (`_line_repetition_dominated`)
Normalizes and counts line occurrences across `text.splitlines()`:
- Trips if any stripped line has `count >= 5` and `count * len(line) >= total_len * 0.5`.
- Highly effective for echoed sentences or repeated bullet points.

### 5.3 General Path: Sliding Window
Slides a 60-character window character-by-character:
- Calculates `needed = max(5, ceil(total_len * 0.5 / 60))`.
- Maintains a hash map of window occurrences; trips as soon as any window reaches `needed`.

---

## 6. Continuation Prompt Construction and Wire Invariants

Prompt construction is handled by [`_get_continuation_prompt(is_partial_stub: bool, dropped_tools: Optional[List[str]])`](file:///home/eins0fx/development/hermes-agent-port/agent/conversation_loop.py#L1329-L1349):

### 6.1 Prompt Variants
1. **Normal Output Length Limit** (`is_partial_stub=False`, `dropped_tools=None`):
   ```
   [System: Your previous response was truncated by the output length limit. Continue exactly where you left off. Do not restart or repeat prior text. Finish the answer directly.]
   ```
2. **Network Stream Interruption** (`is_partial_stub=True`, `dropped_tools=None`):
   ```
   [System: The previous response was cut off by a network error mid-stream. Continue exactly where you left off. Do not restart or repeat prior text. Finish the answer directly.]
   ```
3. **Dropped Tool Call Stream Interruption** (`is_partial_stub=True`, `dropped_tools=[...]`):
   ```
   [System: Your previous tool call (tool1, tool2, tool3) was too large and the stream timed out before it could be delivered. Do NOT retry the same tool call with the same large content. Instead, break the content into multiple smaller tool calls (e.g. use multiple patch calls or write smaller files). Each tool call's arguments must be under ~8K tokens to avoid stream timeouts.]
   ```
   - Capped at the first 3 tool names (`dropped_tools[:3]`).

### 6.2 Context Compressor Recognition
These prompts are stored as module constants (`_LENGTH_CONTINUATION_OUTPUT_LIMIT`, `_LENGTH_CONTINUATION_NETWORK_STUB`, `_LENGTH_CONTINUATION_DROPPED_TOOLS_PREFIX`). The context compression engine identifies them via `str.startswith` to prune transient continuation scaffolding during history compaction.

---

## 7. Synthetic Message Metadata, Role Alternation, and Wire Sanitization

### 7.1 Metadata Flags
- `_length_continuation_fragment`: Set to `True` on interim assistant messages containing partial visible text.
- `_length_continuation_nudge`: Set to `True` on user continuation prompt messages.

### 7.2 Thinking-Only Truncations (Empty Content Suppression)
When a model returns `finish_reason = "length"` with empty visible text (`content == ""`):
- The assistant message is **not** appended to `messages`.
- Upstream strict providers (such as Moonshot/Kimi via OpenRouter) reject `{"role": "assistant", "content": ""}` with HTTP 400. Suppressing empty assistant entries avoids poisoning transcript history.
- Sets `agent._ephemeral_reasoning_off = True` so the subsequent continuation pass requests generation without thinking.

### 7.3 Wire Payload Sanitization
In [`agent/conversation_loop.py`](file:///home/eins0fx/development/hermes-agent-port/agent/conversation_loop.py#L2605-L2606), before any message array is dispatched to the upstream provider:
```python
api_msg.pop("_length_continuation_fragment", None)
api_msg.pop("_length_continuation_nudge", None)
```
Internal bookkeeping tags are strictly removed so provider APIs never encounter unknown JSON fields.

### 7.4 Role Alternation Invariants
For visible text continuation, the message sequence strictly preserves valid conversational turns:
`[..., User Turn, Assistant Fragment 1, User Nudge 1, Assistant Fragment 2, User Nudge 2]`

---

## 8. Request Budgets, Progressive Output Boosting, and Ephemeral Reasoning Override

### 8.1 Attempt Budget
- Maximum continuation attempts: **4** (`length_continue_retries < 4`).
- Passes 1, 2, and 3 schedule continuation requests.
- Attempt 4 trips the continuation ceiling exit.

### 8.2 Progressive Output Token Boosting
Each continuation retry progressively scales up the requested output tokens to provide headroom for completing generation:
```python
_boost_base = agent.max_tokens if agent.max_tokens else 4096
_boost = _boost_base * (2 ** length_continue_retries)
_requested_cap = agent._requested_output_cap_from_api_kwargs(api_kwargs)
if _requested_cap is not None:
    _boost = max(_boost, _requested_cap)
_boost_cap = max(32768, _requested_cap or 0)
agent._ephemeral_max_output_tokens = min(_boost, _boost_cap)
```
- **Schedule**: Retry 1 -> 8,192; Retry 2 -> 16,384; Retry 3 -> 32,768; Retry 4 -> 32,768.
- **Floor Preservation**: If the initial request or provider profile used a larger default (e.g. 65,536), `_requested_cap` ensures the budget never downshifts to a smaller cap.

### 8.3 Ephemeral Reasoning-Off Override
When a model exhausts its cap on reasoning without visible text:
- `_ephemeral_reasoning_off = True` is armed.
- In [`_reasoning_config_for_wire`](file:///home/eins0fx/development/hermes-agent-port/agent/chat_completion_helpers.py#L40-L60), the flag overrides the request reasoning configuration to `{"enabled": False, "effort": "none"}` and resets itself to `False`.
- The override applies strictly to **one** wire request. Subsequent continuation requests restore the user's configured reasoning settings.
- If the provider rejects reasoning disablement (HTTP 400 "reasoning is mandatory"), `_reasoning_disable_rejected` causes the retry to resend the user's config verbatim.
- If the ceiling exit is reached, `_ephemeral_reasoning_off` is explicitly cleared to `False` to prevent leaking into subsequent turns.

---

## 9. Fragment Accumulation, Joining (`_join_truncated_parts`), and Turn Recovery

### 9.1 Fragment Accumulation
During continuation retries, visible text fragments are appended to `truncated_response_parts: List[str]`.

### 9.2 Whitespace-Safe Joining (`_join_truncated_parts`)
Joining continuation fragments must avoid word-gluing while preventing double-newlines:
```python
def _join_truncated_parts(parts: List[str]) -> str:
    joined = ""
    for part in parts:
        if joined and not joined[-1].isspace() and part and not part[0].isspace():
            joined += "\n"
        joined += part
    return joined
```
- If previous text does not end in whitespace and current text does not start with whitespace, inserts `"\n"`.
- If either boundary contains whitespace (spaces, tabs, newlines), parts concatenate directly without modification.

### 9.3 Successful Turn Recovery
When a continuation attempt finishes with `finish_reason == "stop"`:
1. `final_response = _join_truncated_parts([*truncated_response_parts, final_response])`.
2. `truncated_response_parts` is emptied and `length_continue_retries` resets to 0.
3. Live messages are unmarked in place:
   ```python
   for _frag in messages:
       if isinstance(_frag, dict):
           _frag.pop("_length_continuation_fragment", None)
           _frag.pop("_length_continuation_nudge", None)
   ```
   Surviving assistant fragments and user nudges remain in conversation history, but their temporary metadata tags are deleted.
4. Thinking blocks are stripped from `final_response` before return.

---

## 10. Ceiling Exit, Scaffolding Purge, and Session Wedge Prevention

When `length_continue_retries >= 4` (attempt 4 exhausts the budget):

### 10.1 Scaffolding Pruning
Unanswered continuation nudges and intermediate fragments must not survive into durable history. Leaving unanswered "continue" nudges causes every subsequent user turn to continue the truncated response, permanently wedging the session into repeated ceiling exhaustion.

The runtime purges scaffolding scoped strictly to the current turn:
```python
_turn_start = (
    current_turn_user_idx + 1
    if isinstance(current_turn_user_idx, int) and current_turn_user_idx >= 0
    else 0
)
messages[_turn_start:] = [
    m for m in messages[_turn_start:]
    if not (
        isinstance(m, dict)
        and (
            m.get("_length_continuation_fragment")
            or m.get("_length_continuation_nudge")
        )
    )
]
if partial_response:
    append_message(messages, {
        "role": "assistant",
        "content": partial_response,
        "finish_reason": "length",
    })
```

### 10.2 Transcript State Guarantees
1. **Collapsing**: All intermediate fragments collapse into exactly **one** settled assistant turn containing the joined partial text with `finish_reason = "length"`.
2. **Nudge Removal**: All continuation nudges for the current turn are removed.
3. **Turn-Scoped Isolation**: Messages prior to `_turn_start` are untouched, ensuring historical messages preserved on disk survive safely.
4. **All-Empty Ceiling Handling**: If all attempts produced zero visible text, no empty assistant message is appended; `final_response` provides actionable guidance to adjust reasoning settings or increase max output tokens.
5. **Fresh Turn Independence**: The subsequent user message issues a fresh upstream request with `length_continue_retries` reset to 0, completely unencumbered by prior wedge state.

---

## 11. Usage Accounting, API Call Counting, and Stream No-Replay Boundary

### 11.1 API Call Accounting
- `agent.session_api_calls` increments on every completed provider attempt.
- `api_call_count` increments for the initial request and each continuation attempt. Continuation attempts are genuine model executions and are **not refunded** (unlike cross-provider fallback or rebuilt message restarts).

### 11.2 Context Compressor Updates
- Actual response usage is normalized via `normalize_usage` and forwarded to `agent.context_compressor.update_from_response(usage_dict)`.
- For `PARTIAL_STREAM_STUB_ID` stubs, `usage` is `None`. The runtime safely bypasses compressor updates without crashing.

### 11.3 Streaming No-Replay Boundary
In streaming mode ([`interruptible_streaming_api_call`](file:///home/eins0fx/development/hermes-agent-port/agent/chat_completion_helpers.py)):
- **Before Visible Deltas (`deltas_were_sent == False`)**: Network errors and timeouts permit transparent retries (up to `HERMES_STREAM_RETRIES`, default 2) and full request replay because the user has seen zero tokens.
- **After Visible Deltas (`deltas_were_sent == True`)**: Once text tokens have streamed to stdout or UI callbacks, a full request replay would duplicate the already-displayed text.
  - Re-raising is strictly suppressed.
  - The worker returns `_build_partial_stream_stub` tagged with `id = PARTIAL_STREAM_STUB_ID` and `finish_reason = "length"`.
  - The conversation loop seamlessly enters the length continuation path, dispatching `_LENGTH_CONTINUATION_NETWORK_STUB`. The model continues from the last delivered token without repeating prior text.

---

## 12. Explicit Boundary and Separation Matrix

| Dimension | Native Main-Provider Length Continuation | Anthropic Messages Truncation | Codex Responses Truncation | Content Policy Refusals | Tool-Call Truncation | Verification Continuation |
| :--- | :--- | :--- | :--- | :--- | :--- | :--- |
| **Trigger** | `finish_reason == "length"` on chat_completions | `stop_reason == "max_tokens"` mapped to `"length"` | `status == "incomplete"`, `incomplete_reason == "max_output_tokens"` | `finish_reason == "content_filter"` or refusal | `finish_reason == "length"` with `tool_calls` | Complete text turn (`finish_reason == "stop"`) |
| **Normalized Reason** | `"length"` | `"length"` | `"incomplete"` | `"content_filter"` | `"length"` (or `"tool_calls"`) | `"verification_required"` / `"verify_hook_continue"` |
| **Eligible APIs** | `chat_completions`, `bedrock_converse` | `anthropic_messages` | `codex_responses` (exclusive) | All transports | All transports | All transports |
| **Max Retries** | 4 continuation passes | 4 continuation passes | 3 incomplete retries | 0 (immediate fallback/exit) | 4 retries | Bounded by verify budget (1-2) |
| **Prompt Used** | `_LENGTH_CONTINUATION_OUTPUT_LIMIT` or `_NETWORK_STUB` | Same prompt text via adapter | `_CODEX_INCOMPLETE_NUDGE` | None | None (re-runs API call directly) | Synthetic verify prompt (`build_verify_on_stop_nudge`) |
| **Message Scaffolding** | `_length_continuation_fragment`, `_nudge` | Same tags via adapter | Dedicated Codex interim merging | None (exit result) | None (no message appended) | `_verification_stop_synthetic` |
| **Ceiling Action** | Purge nudges, collapse fragments to 1 settled turn | Purge nudges, collapse fragments | Exit with 3-attempt error | Terminal refusal delivery | Abort with truncated tool args error | Fallback to candidate answer |
| **Token Boosting** | `base * (2^retry)` up to 32,768 | `base * (2^retry)` up to 32,768 | Handled by Responses server | None | `base * (2^retry)` up to 32,768 | None |

---

## 13. Parity Test Suite, Invariants, Uncertainties, and Deferrals

### 13.1 Parity Test Commands & Case Counts

- **Golden Generation**:
  ```bash
  .venv/bin/python rust/tools/gen_main_provider_length_continuation_goldens.py
  ```
  Produces [`rust/tools/main-provider-length-continuation-goldens.json`](file:///home/eins0fx/development/hermes-agent-port/rust/tools/main-provider-length-continuation-goldens.json) containing **104 test cases across 10 sections**.

- **Parity Check Verification**:
  ```bash
  .venv/bin/python rust/tools/gen_main_provider_length_continuation_goldens.py --check
  ```
  Confirms byte-exact determinism against disk.

- **Focused Python Regression Suite**:
  ```bash
  .venv/bin/python -m pytest \
    tests/run_agent/test_continuation_ceiling_wedge.py \
    tests/run_agent/test_continuation_repetition_guard.py \
    tests/run_agent/test_length_continuation_thinking_exhaustion.py \
    tests/agent/test_repetition_guard.py \
    tests/run_agent/test_partial_stream_finish_reason.py
  ```
  **Total passing tests: 53 tests** (10 + 2 + 10 + 6 + 25 = 53 passed).

- **Section Breakdown of Golden Test Cases**:
  1. `finish_reason_normalization`: 18 cases
  2. `truncation_eligibility_and_guardrails`: 12 cases
  3. `repetition_guard_algorithm`: 10 cases
  4. `continuation_prompts`: 8 cases
  5. `synthetic_message_metadata_and_wire_sanitization`: 10 cases
  6. `request_budgets_and_progressive_boost`: 10 cases
  7. `fragment_accumulation_and_joining`: 10 cases
  8. `ceiling_exit_and_persistence_cleanup`: 10 cases
  9. `usage_accounting_and_stream_no_replay`: 8 cases
  10. `boundaries_and_separation_matrix`: 8 cases
  **Total Cases**: **104**

### 13.2 Core Architectural Invariants
1. **Scaffolding Non-Leakage**: `_length_continuation_fragment` and `_length_continuation_nudge` must never leak into outgoing API requests, SessionDB, or session JSON files.
2. **Ceiling Wedge Prevention**: Reaching the 4-attempt ceiling must unconditionally remove unanswered continuation nudges from current turn history, collapsing all partial fragments into exactly one settled assistant turn.
3. **Strict One-Shot Reasoning Override**: Ephemeral reasoning-off flags must be consumed on exactly one request and never leak across conversational turns.
4. **No-Replay Streaming Boundary**: A stream drop that occurs after delivering visible tokens must be represented as a partial stub driving continuation, strictly avoiding full request replay.
5. **Role Alternation**: Thinking-only truncations must not insert empty assistant messages, avoiding HTTP 400 rejection from strict upstream providers.

### 13.3 Uncertainties & Explicit Deferrals
1. **Deferred: Provider-Specific Minimum Output Caps**: Certain local proxy setups reject `max_tokens > 4096`. The exponential boost caps at 32,768, which is accepted by OpenRouter and OpenAI; handling endpoints that reject values above 4,096 without 400 recovery is deferred to transport-level capability negotiation.
2. **Deferred: Mid-Turn Model Swapping**: If a user dynamically changes `/model` while a turn is mid-continuation, continuation currently proceeds with the new model configuration; full model-identity rewriting during continuation is deferred to the slash-command handler.
3. **Deferred: Binary Tool Argument Salvage**: When a tool call is truncated mid-JSON, arguments that are valid partial prefixes are currently rejected with `"Response truncated due to output length limit"`; partial JSON streaming repair is deferred to the tool parser lane.
