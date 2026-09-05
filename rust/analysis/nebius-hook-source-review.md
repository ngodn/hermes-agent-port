# Nebius Token Factory Hook Source Review

Read-only audit of [`plugins/model-providers/nebius-token-factory/__init__.py`](../../plugins/model-providers/nebius-token-factory/__init__.py), [`run_agent.py`](../../run_agent.py) (`_supports_reasoning_extra_body`), and [`agent/reasoning_effort.py`](../../agent/reasoning_effort.py).

## 1. Dispatch & Core Gate Interaction
- **Host Matching**: Direct Nebius (`api.tokenfactory.nebius.com`) returns `False` in `run_agent.py::_supports_reasoning_extra_body` (non-OpenRouter, non-whitelisted host).
- **Supports Flag**: `supports_reasoning=False` is passed in context; profile's `_model_supports_reasoning_effort(model)` is strictly authoritative. If routed via OpenRouter with reasoning capabilities advertised, `supports_reasoning=True` bypasses the model check.

## 2. Model Allowlist vs. Upstage Deny-List
- **Strict Allowlist**: Checks flat name (`rsplit("/", 1)[-1].lower()`) against markers: `deepseek-r1`, `deepseek-v4`, `deepseek-reasoner`, `gpt-oss`, `glm-5`, `kimi-k2`, `minimax-m2`, `qwen3`.
- **Disallowed & Empty**: Unlisted models (e.g. `Llama-3.3`, `Hermes-4-70B`, `gpt-oss/llama`), empty string, or `None` return `False` -> emit `({}, {})`.
- **Contrast with Upstage**: Upstage uses a **deny-list** (`solar-mini`, `syn-pro`), defaulting `None`/empty and unknown models to reasoning-capable. In Nebius, `None`/empty strictly disables reasoning when `supports_reasoning` is `False`.

## 3. False vs. Falsey & Disable Semantics
- **Identity Check (`enabled is False`)**: Only literal boolean `False` disables reasoning. Non-boolean falsey values (`0`, `""`, `None`) do not match `is False` and remain enabled.
- **Falsey Effort Fallback**: `raw_effort or "medium"` coerces falsey effort inputs (`None`, `""`, `0`, `False`, `[]`) to `"medium"`.
- **Explicit Disable Keywords**: `effort in {"none", "off", "disabled"}` safely returns `({}, {})`.
- **Contrast with Upstage**: Upstage lacked effort keyword checks; `{"effort": "none"}` erroneously clamped to `"low"` (turning reasoning on). Nebius correctly turns reasoning off.

## 4. String Coercion & Input Resilience
- **Safe Coercion**: `str(raw_effort or "medium").strip().lower()` coerces ints (`123` -> `"123"`), booleans, lists, and dicts without crashing.
- **Contrast with Upstage**: Upstage used `(raw or "").strip()`, raising `AttributeError` on non-string truthy values.

## 5. Custom Efforts and Clamping Ladder
- **Canonical Clamping**: Clamps against `NEBIUS_EFFORTS = ("low", "medium", "high")`. Upper ladder values (`xhigh`, `max`, `ultra`) clamp monotonically down to `"high"`; `minimal` clamps to `"low"`.
- **Bespoke Efforts**: Values outside `EFFORT_LADDER` (e.g. `"custom"`, `"hyperthink"`) pass through `clamp_effort` untouched and emit verbatim on the wire (`{"reasoning_effort": "custom"}`).
- **Contrast with Upstage**: Upstage forced non-ladder custom values to `"high"` and dropped `minimal` to off. Nebius emits custom strings verbatim and clamps `minimal` to `"low"`.

## 6. Catalog URL & Fetch Precedence
- **Explicit Endpoint**: Declares `models_url="https://api.tokenfactory.nebius.com/v1/models?verbose=true"`.
- **Precedence Tier**: For default base URL, Tier 2 fetch hits `models_url` directly (preserves `?verbose=true` without appending `/models`). For custom proxy `base_url`, Tier 1 overrides to `{custom_base}/models`.
- **Contrast with Upstage**: Upstage leaves `models_url=""`, defaulting to standard `{base_url}/models`.
