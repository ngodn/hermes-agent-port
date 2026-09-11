# Ollama GLM stop-to-length correction: Rust port lane

Scope: only Python's Ollama-hosted GLM `finish_reason="stop"` to `"length"`
rewrite. This is Lane C from `rust/analysis/main-provider-recovery-seam-claude.md`.
Out of scope by instruction: thinking-only handling, repetition rejection,
dropped-stream stubs, timeouts, and non-chat transports. Those are named here
only where the rewrite feeds into them, never analyzed.

## What the correction does

Some local Ollama builds misreport a truncated GLM generation as
`finish_reason="stop"` instead of `"length"` (Ollama GH-72316). Left alone,
that partial answer would be treated as a finished turn. Python detects the
misreport conservatively and rewrites `stop` to `length` at the single point
where the chat-completions transport hands back a normalized finish reason, so
the response then flows into the existing length-continuation block and the
model is nudged to finish where it left off.

Everything the rewrite does is one line: `finish_reason = "length"`. It adds no
prompt, no history mutation, no persistence, and no usage accounting of its own.
All downstream effects are whatever the length block already does.

## Source and test references

Python helpers (`run_agent.py`):

- `_is_ollama_glm_backend` at `run_agent.py:1875-1909` (backend gate).
- `_should_treat_stop_as_truncated` at `run_agent.py:1911-1940` (full gate).
- `_has_natural_response_ending` at `run_agent.py:1855-1873` (staticmethod
  heuristic).
- `_strip_think_blocks` forwarder at `run_agent.py:1850-1853`, real body in
  `agent/agent_runtime_helpers.py` `strip_think_blocks`.
- `_base_url_lower` set in the `base_url` setter at `run_agent.py:487`
  (`value.lower() if value else ""`).

Python call site (`agent/conversation_loop.py`):

- Rewrite gate at `agent/conversation_loop.py:4030-4044`, inside the `else`
  branch that normalizes chat-completions responses. It runs right after
  `finish_reason = _finish_result.finish_reason` and
  `assistant_message = _finish_result`, and before the content-filter branch
  (`4060`) and the length block (`4149`).
- Length block the rewrite feeds: `agent/conversation_loop.py:4149-4510`.
- Continuation prompt returned for this case (`is_partial_stub=False`,
  `dropped_tools=None`): `_LENGTH_CONTINUATION_OUTPUT_LIMIT` at
  `agent/conversation_loop.py:1318-1322`, selected by `_get_continuation_prompt`
  at `1329-1348`.

Python tests (`tests/run_agent/test_run_agent.py`):

- `test_ollama_glm_stop_after_tools_without_terminal_boundary_requests_continuation`
  at `tests/run_agent/test_run_agent.py:4346-4389` (full run-turn: local
  `glm-4-9b` on `http://localhost:11434/v1`, tool round then misreported stop
  then continuation; asserts 3 API calls, stitched final text, and the
  output-limit nudge as the last user message).
- `test_ollama_cloud_glm_stop_is_never_rewritten` at
  `tests/run_agent/test_run_agent.py:4391-4402` (parametrized: `ollama.com`
  host and `glm-5.1:cloud`; asserts `_should_treat_stop_as_truncated(...)`
  returns `False`).

Rust anchors already in place (`rust/crates/hermes-gateway/src/native_agent.rs`):

- Length continuation prompt constant `MAIN_LENGTH_CONTINUATION_PROMPT` at
  `native_agent.rs:1003-1004` (byte-identical to the Python output-limit prompt).
- Length branch and continuation cap at `native_agent.rs:5787` and
  `apply_main_length_continuation_cap` at `native_agent.rs:407-431`.
- Finish-reason capture in the streaming outcome at `native_agent.rs:5370-5380`
  (`observe_main_sse_line`), buffered length gate at `native_agent.rs:4654`.
- Client fields available to a gate: `model` (`native_agent.rs:1877`),
  `base_url` (`1879`), `provider_identity: Option<String>` (`1893`),
  `provider_profile` (`1892`, carries `api_mode`, checked at `2089`).

## Truth tables

Each dimension below is a precondition of the rewrite. All must pass for `stop`
to become `length`. Any single failing row leaves `finish_reason` as `stop`.

### 1. finish_reason (gate entry, `run_agent.py:1918`)

| finish_reason value | rewrite considered |
| --- | --- |
| `"stop"` | yes, continue checking |
| anything else (`"length"`, `"tool_calls"`, `"content_filter"`, ...) | no, return False |

The gate only ever upgrades `stop`. A real `length` is handled by the length
block directly and never reaches this helper's rewrite.

### 2. api_mode (`run_agent.py:1918`)

| api_mode | rewrite considered |
| --- | --- |
| `"chat_completions"` | yes |
| `"anthropic_messages"`, `"codex_responses"`, `"bedrock_converse"`, any other | no, return False |

Enforced twice in practice: the helper checks `self.api_mode != "chat_completions"`,
and the call site at `conversation_loop.py:4030-4044` sits inside the `else`
branch that is only reached for non-Codex, non-Anthropic, non-Bedrock modes
(the chat-completions transport path). Both must be chat_completions.

### 3. Backend detection: model and provider (`_is_ollama_glm_backend`, `run_agent.py:1898-1901`)

`model_lower = (self.model or "").lower()`,
`provider_lower = (self.provider or "").lower()`.

| condition | result |
| --- | --- |
| `"glm"` substring in model OR provider == `"zai"` | pass this check |
| neither | return False (not a GLM backend) |

Note: the model match is a plain substring, so `glm-4-9b`, `glm-5.1`,
`GLM-4.5-Air`, and any `...glm...` name qualify. The provider escape hatch
(`zai`) lets a Z.AI-branded provider qualify even if the model string omits
`glm`.

### 4. Backend detection: base URL and provider signature (`run_agent.py:1902-1909`)

Evaluated in order on `base = self._base_url_lower` (already lowercased) and
`model_lower`:

| step | condition | result |
| --- | --- | --- |
| exclude cloud | `"ollama.com" in base` OR `":cloud" in model_lower` | return False (never rewrite) |
| local by URL | `"ollama" in base` OR `":11434" in base` | return True |
| local by provider | else `provider_lower == "ollama"` | True if provider is `ollama`, else False |

Cloud exclusion wins over every local signature because it is checked first.
So `glm-5.1:cloud` on `http://localhost:11434/v1` returns False even though the
URL contains `:11434` (matches Python test at `4393`). Likewise any base URL on
`ollama.com` returns False (test at `4392`).

Deliberate non-matches (all return False from the URL/provider step): arbitrary
private endpoints such as LiteLLM, sglang, vLLM, LM Studio, or Tailscale boxes.
The docstring at `run_agent.py:1884-1887` calls this out as the fix for the
false-positive continuations in issue #13971. The only positive local
signatures are the literal `ollama` substring, the `:11434` port, or an
explicit `ollama` provider.

Truth table combining sections 3 and 4 (model/provider x base):

| model / provider | base URL / provider signature | backend? |
| --- | --- | --- |
| `glm-4-9b` | `http://localhost:11434/v1` | yes |
| `glm-5.1` | `http://ollama.local/v1` | yes |
| any `glm` | provider == `ollama`, non-ollama URL | yes |
| provider `zai` (model without glm) | `...ollama...` or `:11434` URL | yes |
| `glm-5.3-flash` | `https://ollama.com/v1` | no (cloud host) |
| `glm-5.1:cloud` | `http://localhost:11434/v1` | no (`:cloud` model) |
| `glm-4` | `https://litellm.internal/v1` (no ollama, provider not ollama) | no |
| `qwen3` (no glm, provider not zai) | any | no |

### 5. History requirement: a tool message must be present (`run_agent.py:1922-1926`)

```python
if not any(
    isinstance(msg, dict) and msg.get("role") == "tool"
    for msg in (messages or [])
):
    return False
```

| messages content | pass? |
| --- | --- |
| at least one dict with `role == "tool"` | yes |
| no tool message (empty, None, only user/assistant/system) | no, return False |

Rationale: the misreport is observed specifically on the post-tool answer turn.
Requiring a prior tool result in history narrows the rewrite to that shape and
avoids touching plain first-turn answers. `messages or []` means a None history
is treated as empty and fails the check.

### 6. Assistant content and tool calls (`run_agent.py:1927-1938`)

| condition | result |
| --- | --- |
| `assistant_message is None` | return False |
| `assistant_message.tool_calls` truthy | return False (a tool-call turn is not a truncated text answer) |
| `content` is not a `str` (None, list, dict) | return False |
| stripped visible text is empty after `_strip_think_blocks` | return False |
| `len(visible_text) < 20` | return False |
| `visible_text` has no whitespace char (`re.search(r"\s", ...)` is None) | return False |
| all above pass | proceed to natural-ending check |

`visible_text = self._strip_think_blocks(content).strip()` at
`run_agent.py:1934`. Think/reasoning tag variants and standalone tool-call XML
are removed by `strip_think_blocks` before length and whitespace are measured,
so a response that is only reasoning collapses to empty and fails here.

The two magic thresholds:

- Minimum length 20 characters (after stripping and trimming). Short
  acknowledgements do not qualify.
- Must contain at least one whitespace character, i.e. look like a phrase, not a
  single token.

### 7. Natural ending detection (`_has_natural_response_ending`, `run_agent.py:1855-1873`)

Final gate: `return not self._has_natural_response_ending(visible_text)`
(`run_agent.py:1940`). The rewrite fires only when the visible text does NOT
look intentionally finished.

Operates on `stripped = content.rstrip()`, `last = stripped[-1]`:

| condition on stripped visible text | has_natural_ending | rewrite (stop->length) |
| --- | --- | --- |
| empty after rstrip | False | (already excluded by section 6) |
| ends with ` ``` ` (code fence) | True | no |
| ends with `^` | True | no |
| last char in `.!?:)"']}` or CJK `。！？：）】」』》` or `^` | True | no |
| last char is an emoji, `ord(last) >= 0x1F300` | True | no |
| anything else (letter, digit, comma, mid-word) | False | yes, rewrite |

So `"Based on the search results, the best next"` (ends on a letter) is treated
as truncated and rewritten. `"...update the config."` (ends on a period) is
treated as finished and left as `stop`. This is the false-positive-sensitive
heuristic: a normal answer that happens to end without terminal punctuation
(rare but possible) would be misclassified as truncated and get one continuation
nudge.

### 8. Ordering (evaluation and downstream)

Gate internal order (short-circuit, first failing row wins), from
`_should_treat_stop_as_truncated`:

1. `finish_reason == "stop"` and `api_mode == "chat_completions"`
   (`run_agent.py:1918`).
2. `_is_ollama_glm_backend()` (`1920`), which itself orders: GLM/zai check,
   then cloud exclusion, then local URL/provider signature.
3. tool message present in history (`1922`).
4. assistant not None and no tool_calls (`1927`).
5. content is a str (`1930`).
6. stripped visible text non-empty, length >= 20, has whitespace (`1934-1938`).
7. NOT natural ending (`1940`).

Call-site order at `conversation_loop.py:4030-4044`:

1. `_cc_fr = agent._get_transport()`; `normalize_response(response)` produces
   `_finish_result`.
2. `finish_reason = _finish_result.finish_reason`; `assistant_message =
   _finish_result`.
3. `if agent._should_treat_stop_as_truncated(finish_reason, assistant_message,
   messages): finish_reason = "length"` and a `_vprint` diagnostic
   (`"Treating suspicious Ollama/GLM stop response as truncated"`).
4. content-filter branch at `4060` (only runs if finish_reason is
   `content_filter`, which the rewrite never produces).
5. length block at `4149`.

Because the rewrite happens before the length block, the rewritten response
enters that block exactly as a native `length` would, taking the
non-tool-call text-continuation path (`_trunc_has_tool_calls` is False by
section 6): interim assistant fragment appended, output-limit nudge appended,
one continuation request issued. See section 9.

### 9. Cache and persistence effects

The rewrite itself writes nothing. All state changes are the length block's,
and only because the response is now classified as `length`. For the in-scope
case (chat_completions, visible text, no tool calls, not a partial-stream stub,
below the retry ceiling) the relevant effects from
`conversation_loop.py:4355-4437` are:

- `length_continue_retries += 1` (`4356`).
- Interim assistant message built from the partial content
  (`_build_assistant_message`, `4392`), tagged
  `interim_msg["_length_continuation_fragment"] = True` (`4394`), appended to
  `messages` (`4395`), and its text pushed onto `truncated_response_parts`
  (`4396`). The visible partial text is non-empty by section 6, so this is
  always a real append here (never the empty-stub skip at `4381`).
- A continuation user message `{"role": "user", "content":
  _LENGTH_CONTINUATION_OUTPUT_LIMIT, "_length_continuation_nudge": True}`
  appended (`4429-4434`).
- `agent._session_messages = messages` (`4435`),
  `_retry.restart_with_length_continuation = True`, then `break` to re-request
  on the same route (`4436-4437`).

Cache and history implications specific to this lane:

- The appended interim assistant fragment plus the synthetic user nudge extend
  the prompt that is replayed on the continuation call, so the prompt-cache key
  changes exactly as it would for a genuine `length` continuation. No new cache
  divergence is introduced by the rewrite beyond reclassification.
- Both injected messages carry the `_length_continuation_fragment` /
  `_length_continuation_nudge` tags. Context compression projection strips these
  tags (`conversation_loop.py:1310-1311` comment; compressor in
  `agent/context_compressor.py`). The ceiling exit at `4477-4499` also removes
  tagged fragments/nudges from `messages[_turn_start:]` before the final
  persist, so an unanswered nudge does not poison later turns.
- Because the partial content is non-empty, the consecutive-user-message risk
  noted for the empty-stub path (interim assistant skipped, two user messages
  in a row) does NOT arise in this lane. The assistant fragment always sits
  between the tool round and the nudge.
- The false-positive cost the docstring warns about (`run_agent.py:1893-1896`):
  if the gate rewrites a genuinely finished answer, the continuation nudge
  consumes part of the next request's output budget, making a subsequent false
  truncation marginally more likely. This is the reason the cloud exclusions and
  the natural-ending heuristic exist.

## Current Rust state

No stop-to-length rewrite exists in Rust. The native loop acts only on a
provider-supplied literal `length`:

- Streaming: `observe_main_sse_line` copies the provider's `finish_reason`
  verbatim (`native_agent.rs:5376-5380`); the only synthesized `length` is the
  post-visible stall case (`5448`), which is out of scope here.
- Buffered: length gate reads the raw `finish_reason` at `native_agent.rs:4654`
  and `3215-3217`.
- There is no `is_ollama_glm`, no `should_treat_stop_as_truncated`, no
  natural-ending helper, and no GLM/Ollama gating anywhere in the length path.

The length-continuation machinery the rewrite would feed is already ported:
`MAIN_LENGTH_CONTINUATION_PROMPT` (`1003-1004`) matches the Python output-limit
prompt byte for byte, and `apply_main_length_continuation_cap` (`407-431`) plus
the length branch (`5787`) already issue continuations for a real `length`.
So this lane only needs the pre-classifier that upgrades a qualifying `stop`.

## Narrow Rust module interface

A single pure pre-classifier, kept separate from the length block and from any
content guard, called once at the point where the native loop finalizes a
chat-completions `finish_reason` (the streaming outcome after
`observe_main_sse_line`, and the buffered gate near `native_agent.rs:4654`).
Signature mirrors the recovery-seam sketch:

```text
fn should_treat_stop_as_truncated(
    finish_reason: &str,
    api_mode: ApiMode,          // must be ApiMode::ChatCompletions
    provider: &str,             // provider_identity, lowercased
    model: &str,                // route model, lowercased inside
    base_url: &str,             // route base_url, lowercased inside
    has_tool_message: bool,     // any history msg with role == "tool"
    assistant_content: Option<&str>,
    assistant_has_tool_calls: bool,
) -> bool
```

Returns whether to rewrite `stop` to `length`. It owns three internal pieces
that should each be a small private fn so they can be unit tested in isolation:

- `is_ollama_glm_backend(model, provider, base_url) -> bool`: the GLM/zai check,
  the `ollama.com` / `:cloud` cloud exclusions (checked first), then the
  `ollama` / `:11434` URL signatures and the `ollama` provider fallback.
- `has_natural_response_ending(text: &str) -> bool`: the terminal-punctuation,
  code-fence, caret, and emoji (`>= 0x1F300`) heuristic. Rust must decode the
  last Unicode scalar, not the last byte, to match `stripped[-1]` and the
  `ord(last)` emoji test.
- visible-text derivation: reuse the existing think-strip helper (the
  `think_scrubber` / visible-response path referenced at `native_agent.rs:5438`
  and the recovery-seam doc) so the length-20 and whitespace checks run on the
  same stripped text Python measures. Do not add a second stripper.

Inputs are all already on the client or the parsed response:
`NativeAgentClient.model` (`1877`), `.base_url` (`1879`), `.provider_identity`
(`1893`), `provider_profile.api_mode` (`2089`); the history tool-message flag
and the assistant content/tool-call flags come from the messages the loop
already holds. Lowercasing must match Python: `base_url` is compared lowercased
(`_base_url_lower`), and model/provider are lowercased inside the helper.

Keeping this a standalone pre-gate means it can ship without touching Lane A
(content guards) or Lane B (partial-stream stubs), and the length block stays
unchanged: it simply sees `length` instead of `stop`.

## Recommended public run-turn tests

Port the two Python tests as run-turn tests against the native loop, plus the
false-positive guards that the Python unit assertions cover:

1. Rewrite happy path (mirror
   `test_ollama_glm_stop_after_tools_without_terminal_boundary_requests_continuation`,
   `test_run_agent.py:4346`). Local `glm-4-9b` on `http://localhost:11434/v1`,
   a tool round, then a `stop` response whose visible text ends without terminal
   punctuation (`"Based on the search results, the best next"`), then a
   continuation. Assert three provider calls, the stitched final text, and that
   the last replayed user message is the output-limit nudge
   (`MAIN_LENGTH_CONTINUATION_PROMPT`).
2. Cloud never rewritten (mirror `test_ollama_cloud_glm_stop_is_never_rewritten`,
   `test_run_agent.py:4391`). Two parametrized cases: `https://ollama.com/v1`
   with `glm-5.3-flash`, and `http://localhost:11434/v1` with `glm-5.1:cloud`.
   An unpunctuated `stop` after tools stays `stop`, no continuation, one call.
3. No tool message in history. Same GLM-on-Ollama backend and unpunctuated text
   but a history with no `role == "tool"` message stays `stop`.
4. Natural ending stays stop. GLM-on-Ollama, tool in history, but visible text
   ending in `.` (or ` ``` `, or an emoji) stays `stop`, guarding the heuristic.
5. Non-GLM local endpoint stays stop. A non-GLM model (and provider not `zai`)
   on a `:11434` URL stays `stop`, proving the backend gate.
6. Tool-call assistant stays stop. A `stop` response that also carries
   `tool_calls` is not rewritten (it takes the tool path, not truncation).
7. Short or whitespace-free visible text stays stop. Visible text under 20 chars
   or a single token stays `stop`.
8. Non-chat api_mode stays stop. The same backend under a non-chat-completions
   mode is never rewritten.

## Unresolved risks

- Provider string source. Python reads `self.provider`; Rust's nearest field is
  `provider_identity: Option<String>` (`native_agent.rs:1893`), set via
  `with_provider_identity` (`2226-2228`) and left `None` by default (`1976`).
  Confirm that the `zai` and `ollama` provider values reach the native loop with
  the same spelling Python uses, or the provider escape hatch (section 3) and
  the `provider == "ollama"` fallback (section 4) will silently never fire. If
  `provider_identity` is `None` at the gate, only the URL-based signatures work.
- Last-scalar vs last-byte and rstrip semantics. Python's `content.rstrip()`
  strips all Unicode whitespace and `stripped[-1]` / `ord(last)` operate on a
  code point. A byte-oriented Rust port would mis-handle CJK terminals, the
  emoji range, and multi-byte trailing whitespace. The port must trim Unicode
  whitespace and inspect the last `char`.
- Think-strip parity. The 20-char and whitespace checks run on think-stripped
  text. If the Rust visible-text path strips a different set of tag variants
  than `strip_think_blocks` (documented four cases plus tool-call XML in
  `agent/agent_runtime_helpers.py`), the length threshold can be crossed in Rust
  where Python fails it, or vice versa. Reuse one shared stripper.
- api_mode plumbing. Python double-guards on `chat_completions` (helper plus the
  transport branch). The Rust gate must be reached only on the chat-completions
  path; confirm the buffered and streaming finalize points both restrict to that
  mode before calling the gate, matching `conversation_loop.py:4030` structure.
- `:cloud` suffix location. Python checks `":cloud" in model_lower`, a substring
  test on the whole model string, not just a suffix. Keep it a substring test so
  proxied `...:cloud` variants stay excluded.
- Interaction with the post-visible stall `length` synthesis
  (`native_agent.rs:5448`). That path already sets `length` for a stalled but
  visible stream. The stop-to-length gate should run on a cleanly finished
  `stop` outcome only. Confirm the two do not both fire on the same outcome and
  double-count a continuation. This is a sequencing question for whoever wires
  the gate, not a Python-side ambiguity.
- False-positive budget cost. As Python's docstring notes
  (`run_agent.py:1893-1896`), a mis-rewrite spends part of the next request's
  output budget on a nudge. The Rust port inherits this exactly; no mitigation
  beyond faithfully porting the cloud exclusions and the natural-ending
  heuristic. Any divergence in those two loosens or tightens the false-positive
  rate relative to Python.
