# Ollama GLM Stop-to-Length Truncation Correction Contract

**Document Target**: `rust/analysis/ollama-glm-truncation-contract-agy.md`
**Evidence Lane**: Live Python Chat-Completions Ollama/GLM Stop-to-Length Correction
**Primary Sources**:
- [`run_agent.py`](file:///home/eins0fx/development/hermes-agent-port/run_agent.py)
- [`agent/agent_runtime_helpers.py`](file:///home/eins0fx/development/hermes-agent-port/agent/agent_runtime_helpers.py)
- [`agent/conversation_loop.py`](file:///home/eins0fx/development/hermes-agent-port/agent/conversation_loop.py)
- [`agent/transports/chat_completions.py`](file:///home/eins0fx/development/hermes-agent-port/agent/transports/chat_completions.py)
- [`tests/run_agent/test_run_agent.py`](file:///home/eins0fx/development/hermes-agent-port/tests/run_agent/test_run_agent.py)
- [`rust/tools/gen_ollama_glm_truncation_goldens.py`](file:///home/eins0fx/development/hermes-agent-port/rust/tools/gen_ollama_glm_truncation_goldens.py)
- [`rust/tools/ollama-glm-truncation-goldens.json`](file:///home/eins0fx/development/hermes-agent-port/rust/tools/ollama-glm-truncation-goldens.json)

---

## 1. Executive Summary and Scope Boundaries

This document defines the exact behavioral specification for Python's local Ollama/GLM stop-to-length correction.

In certain local Ollama deployments serving GLM family models (such as `glm-4-9b`), generation truncates prematurely after delivering partial text, but upstream Ollama erroneously reports `finish_reason = "stop"` instead of `finish_reason = "length"` (tracked upstream in Ollama issue GH-72316). Left uncorrected, the runtime would accept the truncated answer as a successfully completed turn, dropping the remaining instructions and confusing user workflows.

To correct this upstream bug without introducing widespread regressions, the Python runtime executes a conservative, multi-gate heuristic:
1. It intercepts the normalized response inside `agent/conversation_loop.py` immediately after transport normalization on the `chat_completions` transport path.
2. It evaluates 9 strict gating dimensions in exact sequence via [`AIAgent._should_treat_stop_as_truncated`](file:///home/eins0fx/development/hermes-agent-port/run_agent.py#L1911-L1940).
3. If and only if all gates pass, it mutates `finish_reason = "length"`.
4. The response then falls through to the existing downstream length-continuation block, which constructs the continuation envelope metadata, appends an interim assistant fragment, and issues a continuation nudge to resume output where the model stopped.

### Explicit Out-of-Scope Exclusions
By instruction, this analysis and contract deliberately excludes:
- Rust implementation analysis or code edits.
- Dropped streams and partial stream stubs (`PARTIAL_STREAM_STUB_ID`).
- Stream read timeouts and idle deadline watchdog resets.
- Thinking-only budget exhaustion handlers and reasoning-off overrides.
- Degenerate repetition rejection (`is_repetition_dominated`).
- Non-chat transports (`anthropic_messages`, `codex_responses`, `bedrock_converse`).
- Unrelated retry loops, authentication rotations, or general provider fallback chains.

---

## 2. Live Python Implementation References

### 2.1 Core Helpers in [`run_agent.py`](file:///home/eins0fx/development/hermes-agent-port/run_agent.py)

1. [`_should_treat_stop_as_truncated`](file:///home/eins0fx/development/hermes-agent-port/run_agent.py#L1911-L1940):
   Main entry gate called during turn response normalization. Checks finish reason, API mode, backend identity, history tool presence, assistant message shape, content type, visible think stripping, 20-character and whitespace floors, and terminal punctuation heuristic.
2. [`_is_ollama_glm_backend`](file:///home/eins0fx/development/hermes-agent-port/run_agent.py#L1875-L1909):
   Identifies local Ollama GLM instances. Checks model and provider identities, enforces strict exclusions for hosted Ollama Cloud (`ollama.com`) and proxied cloud models (`:cloud`), and checks local signatures (`:11434` port or `ollama` in base URL or `provider == "ollama"`).
3. [`_has_natural_response_ending`](file:///home/eins0fx/development/hermes-agent-port/run_agent.py#L1855-L1873):
   Static heuristic determining whether visible text appears intentionally finished (code fence, caret, ASCII punctuation, CJK punctuation, or emoji with code point >= 0x1F300).
4. [`_strip_think_blocks`](file:///home/eins0fx/development/hermes-agent-port/run_agent.py#L1850-L1853):
   Forwarder to `agent.agent_runtime_helpers.strip_think_blocks`.
5. [`_base_url_lower`](file:///home/eins0fx/development/hermes-agent-port/run_agent.py#L487):
   Lowercased URL cache updated whenever `base_url` is assigned.

### 2.2 Reasoning Scrubber in [`agent/agent_runtime_helpers.py`](file:///home/eins0fx/development/hermes-agent-port/agent/agent_runtime_helpers.py)

1. [`strip_think_blocks`](file:///home/eins0fx/development/hermes-agent-port/agent/agent_runtime_helpers.py#L985-L1082):
   Strips closed and unterminated reasoning tags (`<think>`, `<thinking>`, `<reasoning>`, `<thought>`, `<REASONING_SCRATCHPAD>`) as well as inline tool-call XML (`<tool_call>`, `<function_call>`, `<function name="...">`).

### 2.3 Call Site in [`agent/conversation_loop.py`](file:///home/eins0fx/development/hermes-agent-port/agent/conversation_loop.py)

The call site sits at lines 4031-4045, inside the fallback branch for standard chat-completions transport:
```python
# agent/conversation_loop.py:4031-4045
_cc_fr = agent._get_transport()
_finish_result = _cc_fr.normalize_response(response)
finish_reason = _finish_result.finish_reason
assistant_message = _finish_result
if agent._should_treat_stop_as_truncated(
    finish_reason,
    assistant_message,
    messages,
):
    agent._vprint(
        f"{agent.log_prefix}⚠️  Treating suspicious Ollama/GLM stop response as truncated",
        force=True,
    )
    finish_reason = "length"
```

Notice the critical ordering:
- It executes directly after `_cc_fr.normalize_response(response)`.
- It executes before the content policy refusal branch (`finish_reason == "content_filter"` at line 4060).
- It executes before the length continuation handler (`finish_reason == "length"` at line 4149).

---

## 3. Exhaustive Gating Dimensions and Evaluation Order

The evaluation within `_should_treat_stop_as_truncated` follows strict short-circuit ordering. If any condition fails, execution returns `False` immediately.

```
+-------------------------------------------------------------------------+
| Incoming Response: finish_reason, assistant_message, messages           |
+-------------------------------------------------------------------------+
                                    |
                                    v
+-------------------------------------------------------------------------+
| Gate 1: finish_reason == "stop"                                         |
+-------------------------------------------------------------------------+
                                    | Pass
                                    v
+-------------------------------------------------------------------------+
| Gate 2: agent.api_mode == "chat_completions"                            |
+-------------------------------------------------------------------------+
                                    | Pass
                                    v
+-------------------------------------------------------------------------+
| Gate 3: agent._is_ollama_glm_backend()                                  |
|   3.1 Model contains "glm" OR provider == "zai"                         |
|   3.2 Cloud Exclusion: "ollama.com" in base_url OR ":cloud" in model   |
|   3.3 Local Signature: "ollama" in URL OR ":11434" in URL OR prov=ollama|
+-------------------------------------------------------------------------+
                                    | Pass
                                    v
+-------------------------------------------------------------------------+
| Gate 4: History contains at least one message with role == "tool"       |
+-------------------------------------------------------------------------+
                                    | Pass
                                    v
+-------------------------------------------------------------------------+
| Gate 5: assistant_message is not None AND has NO active tool_calls      |
+-------------------------------------------------------------------------+
                                    | Pass
                                    v
+-------------------------------------------------------------------------+
| Gate 6: isinstance(assistant_message.content, str)                      |
+-------------------------------------------------------------------------+
                                    | Pass
                                    v
+-------------------------------------------------------------------------+
| Gate 7: visible_text = strip_think_blocks(content).strip() is non-empty|
+-------------------------------------------------------------------------+
                                    | Pass
                                    v
+-------------------------------------------------------------------------+
| Gate 8: len(visible_text) >= 20 AND re.search(r"\s", visible_text)      |
+-------------------------------------------------------------------------+
                                    | Pass
                                    v
+-------------------------------------------------------------------------+
| Gate 9: NOT _has_natural_response_ending(visible_text)                  |
|   Check: ends with ```, ^, terminal punctuation, or emoji >= 0x1F300     |
+-------------------------------------------------------------------------+
                                    | Pass (Text is unpunctuated/cut off)
                                    v
+-------------------------------------------------------------------------+
| Action: Mutate finish_reason = "length"                                 |
+-------------------------------------------------------------------------+
```

### Gate 1: Finish Reason Value
Only a finish reason value of `"stop"` is eligible for rewrite.
- `"stop"`: passes to Gate 2.
- `"length"`: returns `False` (already handled natively by length continuation).
- `"tool_calls"`, `"content_filter"`, `"error"`, `None`, `""`: returns `False`.

### Gate 2: API Mode
Only `"chat_completions"` is eligible for rewrite.
- `"chat_completions"`: passes to Gate 3.
- `"anthropic_messages"`: returns `False`.
- `"codex_responses"`: returns `False`.
- `"bedrock_converse"`: returns `False`.

### Gate 3: Ollama GLM Backend Identity (`_is_ollama_glm_backend`)
This helper contains three ordered sub-checks:

#### Sub-check 3.1: Model and Provider Identity
`model_lower = (self.model or "").lower()`
`provider_lower = (self.provider or "").lower()`
If `"glm" not in model_lower and provider_lower != "zai"`, return `False`.
- Matches any model string containing `"glm"` case-insensitively (e.g. `glm-4-9b`, `glm-5.1`, `GLM-4.5-Air`, `THUDM/glm-4-9b-chat`).
- If `"glm"` is not in the model name, provider `"zai"` acts as an escape hatch.
- Models such as `llama3:8b`, `qwen2.5:72b`, `gpt-4o` return `False`.

#### Sub-check 3.2: Cloud Exclusions (Priority Gate)
`base = self._base_url_lower`
If `"ollama.com" in base or ":cloud" in model_lower`, return `False`.
- Hosted Ollama Cloud endpoints (e.g. `https://ollama.com/v1`, `https://api.ollama.com/v1`) report finish reasons faithfully and are never rewritten (GH-72316, GHSA-76xc-57q6-vm5m).
- Models proxied through a local Ollama daemon that carry the `:cloud` suffix (e.g. `glm-5.1:cloud`, `glm-4-9b:cloud`) are generated on cloud infrastructure and report finish reasons faithfully (issue #98406).
- Cloud exclusions take absolute precedence over local URL and port signatures.

#### Sub-check 3.3: Local Signatures and Private Proxies
If `"ollama" in base or ":11434" in base`, return `True`.
Else return `provider_lower == "ollama"`.
- Matches default Ollama port 11434 (`http://localhost:11434/v1`, `http://127.0.0.1:11434`, `http://192.168.1.100:11434`).
- Matches URLs containing `"ollama"` (e.g. `http://ollama.local:8080/v1`, `http://proxy.internal/ollama/v1`).
- Matches custom endpoints where provider is explicitly configured as `"ollama"`.
- Deliberate exclusion: arbitrary private endpoints without Ollama signatures (LiteLLM, vLLM, sglang, LM Studio, Tailscale proxies) return `False`. This prevents the false-positive truncation continuations identified in issue #13971.

### Gate 4: History Tool-Message Requirement
```python
if not any(
    isinstance(msg, dict) and msg.get("role") == "tool"
    for msg in (messages or [])
):
    return False
```
- The Ollama stop misreport manifests specifically on turns that immediately follow tool execution.
- Requires at least one dictionary in the conversation history with `role == "tool"`.
- Conversations with only user, assistant, or system messages return `False`.
- Empty history (`[]`) or `None` returns `False`.

### Gate 5: Assistant Tool Calls and Presence
```python
if assistant_message is None or getattr(assistant_message, "tool_calls", None):
    return False
```
- If `assistant_message` is `None`, return `False`.
- If `assistant_message.tool_calls` is truthy (non-empty list of tool calls), return `False`.
- If `assistant_message.tool_calls` is `None` or `[]`, proceed.

### Gate 6: Content Type Validation
```python
content = getattr(assistant_message, "content", None)
if not isinstance(content, str):
    return False
```
- The raw assistant content must be an instance of `str`.
- If `content` is `None`, `list` (e.g. structured Anthropic content blocks), `dict`, or `int`, it returns `False` immediately.
- Note: even though `strip_think_blocks` can normalize list/dict payloads, `_should_treat_stop_as_truncated` explicitly checks `isinstance(content, str)` beforehand.

### Gate 7: Visible Think Stripping
```python
visible_text = self._strip_think_blocks(content).strip()
if not visible_text:
    return False
```
- Reasoning tags (`<think>`, `<thinking>`, `<reasoning>`, `<thought>`, `<REASONING_SCRATCHPAD>`) are removed.
- Standalone tool-call XML (`<tool_call>`, `<function_call>`, `<function name="...">`) is removed.
- If the remaining visible text after trimming is empty (e.g. thinking-only output), return `False`.

### Gate 8: 20-Character and Whitespace Floors
```python
if len(visible_text) < 20 or not re.search(r"\s", visible_text):
    return False
```
- Length floor: `len(visible_text)` must be >= 20 characters. Short acknowledgements (e.g. "Done", "Fixed") are not treated as truncated.
- Whitespace floor: `visible_text` must contain at least one whitespace character (`\s`, including space, tab, newline). A single long uninterrupted string of tokens is not treated as truncated text.

### Gate 9: Natural Response Ending Heuristic (`_has_natural_response_ending`)
```python
return not self._has_natural_response_ending(visible_text)
```
`_has_natural_response_ending` inspects the trailing boundary of the stripped visible text:
1. Strips trailing whitespace via `stripped = content.rstrip()`.
2. Code fences: returns `True` if `stripped.endswith("```")`.
3. Caret: returns `True` if `stripped.endswith('^')`.
4. Punctuation set: returns `True` if `stripped[-1]` is in:
   `.!?:)"\']}。！？：）】」』》^`
   - ASCII terminal punctuation and brackets: `.`, `!`, `?`, `:`, `)`, `"`, `'`, `]`, `}`
   - CJK terminal punctuation and brackets: `。` (U+3002), `！` (U+FF01), `？` (U+FF1F), `：` (U+FF1A), `）` (U+FF09), `】` (U+3011), `」` (U+300D), `』` (U+300F), `》` (U+300B)
   - Superscript caret: `^`
5. Emoji code point threshold: returns `True` if `ord(stripped[-1]) >= 0x1F300`.
   - Characters with code point >= 0x1F300 count as natural endings:
     `🎉` (U+1F389, ord=127881), `🚀` (U+1F680, ord=128640), `👍` (U+1F44D, ord=128077), `🌀` (U+1F300, ord=127744).
   - Symbols and dingbats below 0x1F300 that are not in the punctuation string do NOT count as natural endings and are treated as truncated:
     `✅` (U+2705, ord=9989), `✨` (U+2728, ord=10024), `⚠️` (U+26A0, ord=9888), `☕` (U+2615, ord=9749), `🉑` (U+1F251, ord=127569).
6. Unpunctuated / incomplete text:
   - Visible text ending on letters, digits, commas, semicolons, slashes, hyphens, or inline backticks returns `False` from `_has_natural_response_ending`.
   - Consequently, `_should_treat_stop_as_truncated` returns `True`.

---

## 4. Downstream Continuation Envelope Metadata

When `_should_treat_stop_as_truncated` evaluates to `True`, `finish_reason` is updated to `"length"`. It immediately enters the length continuation block in [`agent/conversation_loop.py`](file:///home/eins0fx/development/hermes-agent-port/agent/conversation_loop.py#L4149-L4437):

1. **Interim Assistant Fragment**:
   - `_build_assistant_message(assistant_message, finish_reason)` constructs an assistant message with `finish_reason = "length"`.
   - The message is tagged with scaffolding metadata:
     `interim_msg["_length_continuation_fragment"] = True`
   - It is appended to `messages`.
   - The stripped visible text is appended to `truncated_response_parts`.

2. **Continuation Prompt Selection**:
   - Evaluated via `_get_continuation_prompt(is_partial_stream_stub=False, dropped_tools=None)`.
   - Because this is an output length truncation and not a dropped stream stub (`response.id != PARTIAL_STREAM_STUB_ID`), it returns the exact constant `_LENGTH_CONTINUATION_OUTPUT_LIMIT`:
     ```text
     [System: Your previous response was truncated by the output length limit. Continue exactly where you left off. Do not restart or repeat prior text. Finish the answer directly.]
     ```

3. **Synthetic Continuation Nudge Message**:
   - Constructs a synthetic user nudge message:
     ```python
     continue_msg = {
         "role": "user",
         "content": _LENGTH_CONTINUATION_OUTPUT_LIMIT,
         "_length_continuation_nudge": True,
     }
     ```
   - Appended to `messages`.

4. **Retry State Flag**:
   - Sets `_retry.restart_with_length_continuation = True`.
   - Breaks to restart the request loop using the extended message history.

5. **Ceiling Exit and Scaffolding Sanitization**:
   - If continuation retries reach the cap (4 attempts), the ceiling exit purges all intermediate messages tagged with `_length_continuation_fragment` and `_length_continuation_nudge` from the current turn, collapses accumulated text via `_join_truncated_parts`, and emits a single settled assistant turn.

---

## 5. Truth Table Summary Across Gate Dimensions

| Case Dimension | Input Value | Gate Evaluated | Should Treat As Truncated | Rewritten Finish Reason |
| :--- | :--- | :--- | :--- | :--- |
| Finish Reason | `"stop"` | Gate 1 | `True` (all else pass) | `"length"` |
| Finish Reason | `"length"` | Gate 1 | `False` | `"length"` |
| Finish Reason | `"tool_calls"` | Gate 1 | `False` | `"tool_calls"` |
| Finish Reason | `"content_filter"` | Gate 1 | `False` | `"content_filter"` |
| API Mode | `"chat_completions"` | Gate 2 | `True` (all else pass) | `"length"` |
| API Mode | `"anthropic_messages"` | Gate 2 | `False` | `"stop"` |
| API Mode | `"codex_responses"` | Gate 2 | `False` | `"stop"` |
| Backend Model | `glm-4-9b` | Gate 3.1 | `True` (all else pass) | `"length"` |
| Backend Model | `GLM-4.5-Air` | Gate 3.1 | `True` (all else pass) | `"length"` |
| Backend Model | `llama3:8b` | Gate 3.1 | `False` | `"stop"` |
| Backend Provider | provider `zai`, model `custom` | Gate 3.1 | `True` (all else pass) | `"length"` |
| Cloud Exclusion | base_url `https://ollama.com/v1` | Gate 3.2 | `False` | `"stop"` |
| Cloud Exclusion | model `glm-5.1:cloud` | Gate 3.2 | `False` | `"stop"` |
| Local Signature | base_url `http://localhost:11434/v1` | Gate 3.3 | `True` (all else pass) | `"length"` |
| Local Signature | base_url `http://ollama.local:8080/v1`| Gate 3.3 | `True` (all else pass) | `"length"` |
| Local Signature | provider `ollama`, custom base_url | Gate 3.3 | `True` (all else pass) | `"length"` |
| Private Proxy | base_url `http://litellm.internal/v1` | Gate 3.3 | `False` | `"stop"` |
| History Tools | `[{"role": "tool", "content": "res"}]`| Gate 4 | `True` (all else pass) | `"length"` |
| History Tools | `[{"role": "user", "content": "hi"}]` | Gate 4 | `False` | `"stop"` |
| Assistant Tools | `tool_calls = None` | Gate 5 | `True` (all else pass) | `"length"` |
| Assistant Tools | `tool_calls = [ToolCall(...)]` | Gate 5 | `False` | `"stop"` |
| Content Type | `content = "Valid string..."` | Gate 6 | `True` (all else pass) | `"length"` |
| Content Type | `content = [{"type": "text"}]` | Gate 6 | `False` | `"stop"` |
| Think Stripping | `<think>...</think>Visible text...` | Gate 7 | `True` (all else pass) | `"length"` |
| Think Stripping | `<think>only thinking</think>` | Gate 7 | `False` | `"stop"` |
| Floor Check | `len(text) == 19` | Gate 8 | `False` | `"stop"` |
| Floor Check | `len(text) == 20` (has whitespace) | Gate 8 | `True` (all else pass) | `"length"` |
| Floor Check | `len(text) == 30` (no whitespace) | Gate 8 | `False` | `"stop"` |
| Natural Ending | ends with `.` or `!` or `?` | Gate 9 | `False` (natural ending) | `"stop"` |
| Natural Ending | ends with ` ``` ` or `^` | Gate 9 | `False` (natural ending) | `"stop"` |
| Natural Ending | ends with `。` or `！` or `）` | Gate 9 | `False` (natural ending) | `"stop"` |
| Natural Ending | ends with `🎉` (U+1F389 >= 0x1F300) | Gate 9 | `False` (natural ending) | `"stop"` |
| Natural Ending | ends with `✅` (U+2705 < 0x1F300) | Gate 9 | `True` (truncated) | `"length"` |
| Natural Ending | ends unpunctuated: letter `t` | Gate 9 | `True` (truncated) | `"length"` |
| Natural Ending | ends unpunctuated: comma `,` | Gate 9 | `True` (truncated) | `"length"` |
| Natural Ending | ends unpunctuated: digit `2` | Gate 9 | `True` (truncated) | `"length"` |

---

## 6. Focused Test Suite and Exact Verification

The smallest focused Python test set targeting this correction resides in [`tests/run_agent/test_run_agent.py`](file:///home/eins0fx/development/hermes-agent-port/tests/run_agent/test_run_agent.py).

### Exact Test Execution Command
```bash
./.venv/bin/python3 -m pytest tests/run_agent/test_run_agent.py -k "test_ollama_glm_stop_after_tools_without_terminal_boundary_requests_continuation or test_ollama_cloud_glm_stop_is_never_rewritten" -v
```

### Exact Test Results and Counts
```text
============================= test session starts ==============================
platform linux -- Python 3.11.15, pytest-9.1.1, pluggy-1.6.0
rootdir: /home/eins0fx/development/hermes-agent-port
collected 283 items / 280 deselected / 3 selected

tests/run_agent/test_run_agent.py::TestRunConversation::test_ollama_glm_stop_after_tools_without_terminal_boundary_requests_continuation PASSED [ 33%]
tests/run_agent/test_run_agent.py::TestRunConversation::test_ollama_cloud_glm_stop_is_never_rewritten[https://ollama.com/v1-glm-5.3-flash] PASSED [ 66%]
tests/run_agent/test_run_agent.py::TestRunConversation::test_ollama_cloud_glm_stop_is_never_rewritten[http://localhost:11434/v1-glm-5.1:cloud] PASSED [100%]

====================== 3 passed, 280 deselected in 1.19s =======================
```

### Test Case Descriptions
1. `test_ollama_glm_stop_after_tools_without_terminal_boundary_requests_continuation` ([`test_run_agent.py:4346-4390`](file:///home/eins0fx/development/hermes-agent-port/tests/run_agent/test_run_agent.py#L4346-L4390)):
   - Configures local Ollama GLM-4-9b on `http://localhost:11434/v1`.
   - Simulates 3-call sequence: tool execution turn, truncated unpunctuated text response (`"Based on the search results, the best next"`) with `finish_reason="stop"`, and continued turn (`" step is to update the config."`).
   - Asserts that `_should_treat_stop_as_truncated` converts `"stop"` to `"length"`, that 3 API calls are made, and that the continuation user message contains `"truncated by the output length limit"`.
2. `test_ollama_cloud_glm_stop_is_never_rewritten[https://ollama.com/v1-glm-5.3-flash]` ([`test_run_agent.py:4391-4403`](file:///home/eins0fx/development/hermes-agent-port/tests/run_agent/test_run_agent.py#L4391-L4403)):
   - Configures hosted Ollama Cloud endpoint `https://ollama.com/v1` with model `glm-5.3-flash`.
   - Asserts that unpunctuated stop after tool results is never rewritten (`_should_treat_stop_as_truncated(...) is False`).
3. `test_ollama_cloud_glm_stop_is_never_rewritten[http://localhost:11434/v1-glm-5.1:cloud]` ([`test_run_agent.py:4391-4403`](file:///home/eins0fx/development/hermes-agent-port/tests/run_agent/test_run_agent.py#L4391-L4403)):
   - Configures local Ollama endpoint `http://localhost:11434/v1` with cloud proxy model `glm-5.1:cloud`.
   - Asserts that `:cloud` proxy model is never rewritten (`_should_treat_stop_as_truncated(...) is False`).

---

## 7. Deterministic Golden Artifact Verification

The deterministic generator [`rust/tools/gen_ollama_glm_truncation_goldens.py`](file:///home/eins0fx/development/hermes-agent-port/rust/tools/gen_ollama_glm_truncation_goldens.py) executes the live Python helpers across 113 test cases (43 positive, 70 negative) and writes [`rust/tools/ollama-glm-truncation-goldens.json`](file:///home/eins0fx/development/hermes-agent-port/rust/tools/ollama-glm-truncation-goldens.json).

### Generator Execution Commands
```bash
# Generate goldens
./.venv/bin/python3 rust/tools/gen_ollama_glm_truncation_goldens.py

# Verify byte-for-byte parity
./.venv/bin/python3 rust/tools/gen_ollama_glm_truncation_goldens.py --check
```

### Determinism Proof
Two consecutive independent executions produce identical byte outputs:
- Run 1 SHA256: `44b081ba30fb0a251849737dba02f0c1f958c4b6605b47c654c250a0f3730a25`
- Run 2 SHA256: `44b081ba30fb0a251849737dba02f0c1f958c4b6605b47c654c250a0f3730a25`
- `cmp /tmp/goldens_run1.json /tmp/goldens_run2.json`: 0 byte divergence (BYTE-FOR-BYTE IDENTICAL).
- Em dash audit: 0 em dash characters (`\u2014`) in contract markdown, generator script, or golden JSON.
