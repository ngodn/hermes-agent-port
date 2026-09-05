# Extra Body Assembly, Override Precedence, and SDK Merge Review

Audit of [`agent/transports/chat_completions.py`](../../agent/transports/chat_completions.py), [`plugins/model-providers/ai-gateway/__init__.py`](../../plugins/model-providers/ai-gateway/__init__.py), [`run_agent.py`](../../run_agent.py), and installed OpenAI SDK (`openai` v2.24.0, [`openai/_base_client.py`](../../.venv/lib/python3.11/site-packages/openai/_base_client.py)).

## 1. Transport Assembly & Precedence Hierarchy
- **Profile Assembly Stages** ([`chat_completions.py:916-972`](../../agent/transports/chat_completions.py#L916-L972)):
  1. Base: `profile_body = profile.build_extra_body(...)` (tags, provider preferences, plugins).
  2. Provider extras: `extra_body_from_profile` via `profile.build_api_kwargs_extras(...)` (`reasoning`, `thinking`).
  3. Caller additions: `params.get("extra_body_additions")`.
  4. User overrides: `request_overrides.get("extra_body")` (when `isinstance(v, dict)`).
  5. Native Gemini filter: `_native_gemini` strips all keys except `thinking_config`/`thinkingConfig`.
  6. Final emission: populated `extra_body` is assigned to `api_kwargs["extra_body"]`.
  7. Prompt cache key: `_add_prompt_cache_key` clamps `extra_body["prompt_cache_key"]` to $\le 64$ chars or pops.
- **Shallow Merge Semantics**: Every step executes `.update()`. Overlapping keys are shallowly overwritten; nested objects (e.g., `reasoning`, `provider`) are replaced in whole rather than deep-merged.
- **Legacy Path Divergence** ([`chat_completions.py:796-808`](../../agent/transports/chat_completions.py#L796-L808)): When `provider_profile` is absent, `api_kwargs.update(overrides)` executes after `extra_body` assignment, completely replacing `api_kwargs["extra_body"]` instead of merging into it.

## 2. Request Overrides Precedence & SDK Kwarg Limits
- **Strict SDK Method Signature**: Local SDK [`Completions.create`](../../.venv/lib/python3.11/site-packages/openai/resources/chat/completions.py) declares explicit kwargs without `**kwargs` (`VAR_KEYWORD`).
- **Top-Level Rejection**: In `chat_completions.py`, non-dict overrides and non-`extra_body` keys fall into `api_kwargs[k] = v`. Passing unrecognized top-level keys raises `TypeError: Completions.create() got an unexpected keyword argument`.
- **Arbitrary Body Field Contract**: Custom or provider-specific request body keys MUST be passed under `request_overrides["extra_body"]` to reach the wire safely without triggering SDK keyword errors.

## 3. SDK Final HTTP JSON Projection ([`openai/_base_client.py`](../../.venv/lib/python3.11/site-packages/openai/_base_client.py#L501-L509))
- **Options Mapping**: `api_kwargs["extra_body"]` maps to `options["extra_json"]` in `make_request_options`. In `_build_request`, `json_data = _merge_mappings(json_data, options.extra_json)`.
- **Shallow Merge & Wire Precedence**: `_merge_mappings` executes `{**obj1, **obj2}`. `extra_body` (`obj2`) strictly takes precedence over typed top-level fields (`obj1`), shallowly overwriting wire keys (e.g., overriding `temperature` or replacing `response_format`).
- **Null & Omit Outcomes**:
  - `extra_body=None`: omitted by `make_request_options`; `json_data` remains untouched.
  - Sub-key `{"key": None}` in `extra_body`: serialized to wire JSON as `"key": null`.
  - Sub-key `{"key": Omit()}`: stripped from the wire JSON payload by the `_merge_mappings` comprehension.
- **Non-Object Failure**: Passing a non-mapping (`list`, `int`, `str`) for `extra_body` causes `_merge_mappings` to raise `TypeError: '<type>' object is not a mapping`.

## 4. Vercel AI Gateway Hook & Core Gate
- **Core Gate** ([`run_agent.py:8221`](../../run_agent.py#L8221)): `_supports_reasoning_extra_body()` matches host `ai-gateway.vercel.sh` and returns `True` unconditionally, bypassing model-family prefix and catalog checks.
- **Hook Implementation** ([`plugins/model-providers/ai-gateway/__init__.py:16-28`](../../plugins/model-providers/ai-gateway/__init__.py#L16-L28)):
  - `build_extra_body()`: unoverridden default returning `{}`.
  - `build_api_kwargs_extras()`: when `supports_reasoning=True`, emits `extra_body["reasoning"] = dict(reasoning_config)` if config is present; otherwise defaults to `{"enabled": True, "effort": "medium"}`. Emits empty `top_level` dict `{}`.
  - Passes un-clamped effort strings (e.g. `"ultra"`) directly; non-mapping truthy configs raise `TypeError`/`ValueError` via `dict()`.
- **Transport Interplay**: Profile reasoning is seeded into `extra_body` at stage 2. If `request_overrides` supplies `extra_body.reasoning`, the user dict shallow-replaces the profile's reasoning block before SDK dispatch.
