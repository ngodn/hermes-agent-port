# Upstage Solar Hook Source Review

Read-only audit of [`plugins/model-providers/upstage/__init__.py`](../../plugins/model-providers/upstage/__init__.py), [`agent/reasoning_effort.py`](../../agent/reasoning_effort.py), and runner call paths.

## 1. Caller Hierarchy and Config Shape
- **Runner Call Chain**: [`run_agent.py`](../../run_agent.py) (`_build_api_kwargs`) forwards to [`agent/chat_completion_helpers.py`](../../agent/chat_completion_helpers.py), calling [`agent/transports/chat_completions.py`](../../agent/transports/chat_completions.py) (`_build_kwargs_from_profile`). This invokes `profile.build_api_kwargs_extras(...)`. Auxiliary/subagent calls invoke it via [`agent/auxiliary_client.py`](../../agent/auxiliary_client.py).
- **Config Shapes**: Produced by `resolve_reasoning_config` or `/reasoning` commands:
  - Enabled: `{"enabled": True, "effort": "<level>"}` (`minimal`, `low`, `medium`, `high`, `xhigh`, `max`, `ultra`).
  - Disabled: `{"enabled": False}` or continuation override `{"enabled": False, "effort": "none"}`.
  - Default / Unset: `None`.
- **Pre-Clamping**: `chat_completions.py::_reasoning_config_for_model` clamps `ultra` to `max` against `OPENAI_COMPAT_WIRE_EFFORTS` before profile dispatch.

## 2. Effort Handling Matrix
- **Model Gate**: Deny-list markers `("solar-mini", "syn-pro")` return `({}, {})`. Non-mini, empty, or `None` model default to reasoning-capable.
- **Default (Unset / None / `{}`)**: Emits `{"reasoning_effort": "medium"}`. Replaces Solar server default (`minimal` = off) with default-on for agentic tasks.
- **Disabled (`enabled: False`)**: Emits `{}`. Omitting `reasoning_effort` lets Solar apply server default `minimal` (off).
- **Minimal (`effort == "minimal"`)**: Emits `{}`. Omits field because Solar `minimal` represents reasoning disabled.
- **Explicit Levels (`low`, `medium`, `high`)**: `clamp_effort` against `SOLAR_EFFORTS = ("low", "medium", "high")` emits verbatim.
- **Ladder Overflows (`xhigh`, `max`)**: `clamp_effort` takes nearest weaker supported level, clamping to `high`.
- **Unknown / Bespoke Levels**: Bespoke values outside `EFFORT_LADDER` (e.g., `hyperthink`) bypass clamp, then fall into `mapped = "high"` (#62650 precedent: run at full strength rather than downgrade).
- **`{"effort": "none"}` Anomaly**: If passed without `enabled: False`, it misses line 75 and line 86; `clamp_effort("none", SOLAR_EFFORTS)` clamps to `"low"` instead of turning reasoning off.

## 3. Malformed Input Resilience
- **Non-Dict Config**: `None`, list, or int safely caught by `not isinstance(reasoning_config, dict)`, returning default `medium`.
- **Empty / Whitespace Effort**: `not effort` safely caught, returning default `medium`.
- **Non-String Effort**: `(reasoning_config.get("effort") or "").strip()` crashes with `AttributeError` on non-string truthy values (e.g. `{"effort": 123}` or `{"effort": True}`) due to missing `str()` cast.

## 4. Request Merge Order and Precedence
1. `api_kwargs` initialized with `model`, `messages`, `temperature`, `timeout`, `tools`, `max_tokens`.
2. `profile.build_api_kwargs_extras(...)` returns `({}, {"reasoning_effort": ...})`.
3. `api_kwargs.update(top_level_from_profile)` sets `reasoning_effort`.
4. `profile.build_extra_body` and caller additions merge into `extra_body`.
5. User `request_overrides` merge last: any top-level key overwrites `api_kwargs`, ensuring user overrides take precedence over profile `reasoning_effort`.
