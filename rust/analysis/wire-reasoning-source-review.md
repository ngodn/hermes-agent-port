# Wire Reasoning Normalization, Pipeline Order, and Provider Exceptions

Read-only audit of [`agent/transports/chat_completions.py`](../../agent/transports/chat_completions.py), [`agent/chat_completion_helpers.py`](../../agent/chat_completion_helpers.py), and provider profile hooks.

## 1. Transport Pipeline Order & Reasoning Lifecycle
- **Pre-Transport Layer (`_reasoning_config_for_wire`)** ([`chat_completion_helpers.py:1971-1995`](../../agent/chat_completion_helpers.py#L1971-L1995)):
  1. Consumes `_ephemeral_reasoning_off` atomically (resets flag to `False`).
  2. Sticky rejection gate: if `agent._reasoning_disable_rejected` is set, disables (`enabled=False` or `effort="none"`) return `None` (omitted -> route default); non-disable configs pass through verbatim.
  3. Ephemeral disable: merges `{**(cfg or {}), "enabled": False, "effort": "none"}`.
- **Transport Call Chain** ([`chat_completions.py:870-972`](../../agent/transports/chat_completions.py#L870-L972)):
  1. Wire clamp: `reasoning_config = _reasoning_config_for_model(model, params.get("reasoning_config"))` runs FIRST (at L879 in profile path, L652 in legacy).
  2. Cap resolution & Gemini raising: `_raise_gemini_thinking_max_tokens(model, reasoning_config, cap)` runs BEFORE profile hooks; raises active thinking cap to >= 65,535.
  3. Profile extras: `profile.build_api_kwargs_extras(reasoning_config=...)` receives the already-clamped config.
  4. Extra body assembly: `profile.build_extra_body(reasoning_config=...)` -> `extra_body.update(profile_body)` -> `extra_body.update(extra_body_from_profile)`.
  5. Overrides: `request_overrides.get("extra_body")` updates `extra_body` last (can bypass transport clamps via raw caller dicts).
  6. Native Gemini filter: `_native_gemini` strips all `extra_body` keys except `thinking_config`/`thinkingConfig`.

## 2. Config Normalization & Exact Preservation Semantics
- **Non-Dict & Identity Preservation** ([`chat_completions.py:182-192`](../../agent/transports/chat_completions.py#L182-L192)): Non-dict values (`None`, primitives, lists) and empty/absent effort return `reasoning_config` untouched (same object reference).
- **Whitespace & Case Normalization**: `effort = str(reasoning_config.get("effort") or "").strip().lower()`. Strips Unicode/ASCII whitespace including `\x1c..\x1f`. Unknown levels not in [`EFFORT_LADDER`](../../agent/reasoning_effort.py#L50) (e.g. `"custom"`, numeric strings) return original config untouched.
- **Nearest-Weaker Clamping**: Clamps against [`OPENAI_COMPAT_WIRE_EFFORTS`](../../agent/reasoning_effort.py#L60) (`none..max`). `"ultra"` is internal-only and clamps down to `"max"`.
- **Shallow Copy on Mutation**: When `clamped != effort`, returns `normalized = dict(reasoning_config)` with `normalized["effort"] = clamped`. All sibling keys (`"enabled"`, `"type"`, custom metadata) are strictly preserved. If `clamped == effort` (e.g. `{"effort": " HIGH "}`), original dict reference is preserved without re-allocation.

## 3. Vercel Ultra: Raw Hook Passthrough vs. Actual Wire Clamp
- **Profile Hook Passthrough** ([`plugins/model-providers/ai-gateway/__init__.py:16-28`](../../plugins/model-providers/ai-gateway/__init__.py#L16-L28)): `VercelAIGatewayProfile.build_api_kwargs_extras` checks `supports_reasoning` and emits `extra_body["reasoning"] = dict(reasoning_config)`. In isolation (e.g. [`rust/tools/gen_vercel_goldens.py`](../../rust/tools/gen_vercel_goldens.py)), `{"effort": "ultra"}` emits un-clamped `{"effort": "ultra"}`.
- **Transport Wire Clamp**: In end-to-end execution, `_reasoning_config_for_model` executes before `build_api_kwargs_extras`, clamping `"ultra"` to `"max"`. On the actual HTTP wire, `extra_body["reasoning"]` contains `{"effort": "max"}`.
- **Rust Test Discrepancy**: [`rust/crates/hermes-gateway/src/native_agent.rs:548-551`](../../rust/crates/hermes-gateway/src/native_agent.rs#L548-L551) asserted `{"effort": "ultra"}` on the wire because it replicated isolated profile hook behavior without transport-level pre-hook wire normalization.

## 4. Model- & Provider-Dependent Exceptions
- **Vercel AI Gateway (`ai-gateway`)**: [`run_agent.py:8221`](../../run_agent.py#L8221) matches host `ai-gateway.vercel.sh` and enables `supports_reasoning` unconditionally; passes clamped config under `extra_body["reasoning"]`.
- **OpenRouter (`openrouter`)**: Mandatory-reasoning Claude (Claude 4.6+, 4.7+, fable) omits `extra_body["reasoning"]` entirely (preventing 400s on disable/tool replay) and maps effort to top-level `verbosity`. Other models clamp against catalog `supported_efforts`; mandatory models omit disables.
- **Nous Portal (`nous`)**: Emits `extra_body["reasoning"]` including `{"enabled": False}`, unless `_cannot_disable_reasoning(model)` is true (which drops the disable to preserve default thinking).
- **Gemini (`gemini`)**: Omitted entirely for non-Gemini models (Gemma/PaLM). Gemini 2.5 uses `includeThoughts=True` (no level). Gemini 3/3.1 maps flash ({`minimal,low`}->`low`, {`high,xhigh,max,ultra`}->`high`) and pro ({`high,xhigh,max,ultra`}->`high`, other->`low`). Disables emit `includeThoughts=False`. OpenAI-compat `/openai` subpath nests snake_case under `extra_body["extra_body"]["google"]["thinking_config"]`; native REST uses camelCase and strips non-thinking extra_body keys.
- **Kimi / Moonshot (`kimi-coding`)**: Top-level `reasoning_effort` and `extra_body.thinking` are mutually exclusive. Disables emit `extra_body.thinking = {"type": "disabled"}`. Explicit effort emits top-level `reasoning_effort` clamped to K3 (`low/high/max` with `medium->high`, `xhigh->max`) or K2 (`low/medium/high`). Unset effort emits `extra_body.thinking = {"type": "enabled"}`.
- **DeepSeek (`deepseek`)**: V3 ignores reasoning. V4 emits `extra_body.thinking = {"type": "enabled"|"disabled"}` and top-level `reasoning_effort` clamped to `low/medium/high/max` (`xhigh`/`ultra`->`max`). Unset effort omits `reasoning_effort`.
- **Nebius & Upstage**: Nebius emits top-level `reasoning_effort` (`low/medium/high`, `ultra`->`high`), omitting on disable. Upstage emits top-level `reasoning_effort` (`low/medium/high`, `ultra`->`high`, unset defaults to `medium`), omitting on disable or `minimal`.
- **Z.AI / GLM (`zai`)**: Emits `extra_body.thinking` toggle; GLM-5.2 emits top-level `reasoning_effort` (`high/max`), GLM-5.3 emits `low/medium/high/max`.
- **Meta AI & Ollama Cloud**: Meta AI self-gates and emits top-level `reasoning_effort` (`minimal..xhigh`). Ollama Cloud requires top-level `reasoning_effort: "none"` to disable (ignores extra_body).
- **OpenCode Zen / Free**: Ox Alpha (`x-preview-f-free`) strictly requires top-level `reasoning_effort` in `{low, high, max}` (`none` 400s).

Reviewer correction: the Upstage default is medium, verified directly in its profile and existing source fixtures.
