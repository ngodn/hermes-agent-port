# Output Cap & Parameter Resolution Source Review

Read-only audit of [`run_agent.py`](../../run_agent.py), [`utils.py`](../../utils.py), [`gateway/run.py`](../../gateway/run.py), [`agent/agent_init.py`](../../agent/agent_init.py), and [`agent/transports/chat_completions.py`](../../agent/transports/chat_completions.py).

## 1. Wire Parameter Selection (`_max_tokens_param` & `utils.py`)
- **URL-First Wire Routing**: Emits `{"max_completion_tokens": value}` when URL matches direct OpenAI (`base_url_hostname == "api.openai.com"`), Azure OpenAI (`base_url_host_matches(url, "openai.azure.com")`), or GitHub Copilot (`hostname == "api.githubcopilot.com"` or `hostname.endswith(".githubcopilot.com")`); otherwise emits `{"max_tokens": value}`.
- **Apex Host Pitfall**: Copilot check requires subdomain (e.g. `api.` or `other.`), returning `False` for bare `https://githubcopilot.com/v1`. Host parsing strips trailing dots and handles scheme-less URLs via `//`.
- **Model Name Fallback**: `model_forces_max_completion_tokens(model)` strips Unicode whitespace/control chars (`\x1c..\x1f`), takes the final segment via `rsplit("/", 1)[-1]`, and matches prefixes: `gpt-4o`, `gpt-4.1`, `gpt-5`, `o1`, `o3`, `o4`. Nested paths (`prefix/gpt-5/not-openai`) evaluate to `False`.
- **Request Cap Extraction**: `_requested_output_cap_from_api_kwargs` probes `("max_output_tokens", "max_completion_tokens", "max_tokens")` in order, returning the first positive integer.

## 2. Gateway Resolution vs. Agent Init Disparities
- **Gateway Waterfall (`gateway/run.py:3344`)**: `HERMES_MAX_TOKENS` env -> `model_cfg.max_tokens` -> `runtime.max_output_tokens`.
- **Bool & Type Pitfalls**: In gateway, `isinstance(False, int)` is `True`, so `bool` configs evaluate to `0` or `1`. Non-int strings (`'123'`) and floats are ignored. Env var allows `0`, negatives, and underscores (`1_024`).
- **Runtime Fallback Bypass**: `runtime.max_output_tokens` requires `int > 0` and only triggers when `max_tokens is None`. Falsey non-null values (`0`, `False`, `-1`) prevent fallback.
- **Agent Init Fallback (`agent/agent_init.py:2433`)**: Triggers only when `agent.max_tokens is None`. Explicitly rejects `bool`, coerces numeric strings/Arabic-Indic (`' ١_٢ '` -> `12`), and rejects `<= 0`. If gateway set `0`/`False`, agent_init skips validation.
- **Session Rehydration**: Gateway `/model` switches persist `max_tokens` in session state overrides and rehydrate it on reload.

## 3. Chat Completions Transport Precedence
- **Output Cap Precedence**: `request_overrides` > `ephemeral_max_output_tokens` > user `max_tokens` > `profile.get_max_tokens(model)` > `anthropic_max_output` > omitted. Ephemeral, user, and profile caps format via `max_tokens_fn`.
- **Temperature Precedence**: `request_overrides["temperature"]` > `profile.fixed_temperature` (`OMIT_TEMPERATURE` strips key; fixed value overrides) > `params["temperature"]` > omitted.
- **Legacy Fallback Defect**: When `provider_profile` is `None`, legacy path ignores `fixed_temperature`, `omit_temperature`, and `params["temperature"]` completely; only `request_overrides` can emit temperature.

## 4. Gemini & Ephemeral Cap Edge Cases
- **Gemini Thinking Floor**: `_raise_gemini_thinking_max_tokens` checks `gemini*` prefix and active thinking (`includeThoughts` or budget/level). Clamps cap to `max(requested, 65535)` (`GEMINI_DEFAULT_MAX_OUTPUT_TOKENS`), preventing thought exhaustion and retry abortion.
- **Native Gemini Adapter**: Native REST adapter defaults missing, invalid, or `<= 0` caps to `65,535`, and elevates thinking requests to `65,535`. Note: accepts `max_tokens`; `max_completion_tokens` is discarded into `**_`.
- **Compressor Headroom**: `agent_init.py:2849` assigns `_compressor_max_tokens = 65,535` for Gemini when unset, ensuring `pct * (window - max_tokens)` reserves the native output budget rather than `0`.
- **Ephemeral Cap Lifecycle**: Stored on `agent._ephemeral_max_output_tokens`, consumed and reset to `None` in one shot. Used in conversation loop for: (1) truncated tool-call retry doubling (`cap * 2^retries`, max 32k), (2) context-overflow prompt shrink (`safe_out = max(1, avail - 64)` leaving `context_length` intact), (3) finish_reason `"length"` continuation doubling (`cap * 2^retries`, max 32k).
