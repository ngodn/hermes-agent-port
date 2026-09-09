# Full-Compression Auxiliary Configuration Oracle

Deterministic, source-executed Python oracle for the four decisions that shape a
full context-compression summary request: auxiliary configuration resolution,
the summary output cap, the summary temperature, and the one-shot fallback to
the main model after an auxiliary failure.

- Generator: `rust/tools/compression-auxiliary-config-oracle.py`
- Corpus: `rust/tools/compression-auxiliary-config-goldens.json` (129 cases)
- Report: this file

The generator imports the real modules and drives the genuine decision
functions. It never reimplements the logic. For the one inline decision (the
fallback predicate) it lifts the exact boolean assignments and `if` test
expressions out of the source via AST and executes them verbatim against
synthetic exceptions and compressor state. Patches are confined to the config
loader, `os.environ`, and pure client-construction seams, all restored after
each case.

Run `python rust/tools/compression-auxiliary-config-oracle.py` to write the
corpus and `--check` to regenerate and compare.

## Authoritative functions executed

All citations are current local source.

| Decision | Function | Location |
| --- | --- | --- |
| Task config resolution | `_resolve_task_provider_model` | `agent/auxiliary_client.py:8696-8870` |
| Config read seam | `_get_auxiliary_task_config` | `agent/auxiliary_client.py:8887-8928` |
| Credential env read | `_scoped_key_env` | `agent/auxiliary_client.py:1699-1719` |
| Output-cap certification | `resolve_compression_fast_lane` | `agent/auxiliary_client.py:8971-9006` |
| Cap field parsing | `_fast_lane_config_fields` | `agent/auxiliary_client.py:8939-8968` |
| Wire cap forwarding + param name | `_build_call_kwargs` | `agent/auxiliary_client.py:9331-9448` |
| Model-specific param name | `auxiliary_max_tokens_param` | `agent/auxiliary_client.py:8150-8176` |
| Fixed/omit temperature | `_fixed_temperature_for_model` | `agent/auxiliary_client.py:948-967` |
| Failure classification + fallback predicates | inline in `ContextCompressor._generate_summary` | `agent/context_compressor.py:5641-5791` |
| Non-retryable class | `_is_summary_access_or_quota_error` | `agent/context_compressor.py:210-245` |
| Streaming-close class | `_is_connection_error` | `agent/auxiliary_client.py:4732` |

## Section 1: auxiliary.compression resolution (20 cases)

Drives `_resolve_task_provider_model(task="compression")`. The oracle patches
`hermes_cli.config.load_config_readonly` to feed a fixed `auxiliary.compression`
block and patches `os.environ` for the `key_env` path (the `_scoped_key_env`
fallback used off the profile secret scope). Records the resolved
`(provider, model, base_url, api_key, api_mode)` tuple.

Exact coverage:

- Precedence: explicit arg beats config beats `auto` (`explicit_provider_arg_*`).
- Credentials: inline `api_key`, `key_env`/`api_key_env` resolved from env,
  missing env fails to `None`, direct `api_key` wins over `key_env`.
- `model: auto` and whitespace-only values normalize to absent.
- api_mode passthrough for `anthropic_messages` and `codex_responses`.
- Return-shape rules: bare base_url plus api_key becomes `custom`; a first-class
  provider plus base_url keeps the provider identity; the `openai` direct-api
  alias rewrites to `custom` with `api.openai.com/v1` (or the user base).

Two behaviors worth flagging for the port (both are the genuine source output,
captured as named cases):

- `bare_base_url_no_key_falls_to_auto`: a config with only `base_url` (no
  api_key, no provider) does NOT create a custom endpoint. It falls through to
  `("auto", ..., None, None, ...)` and the base_url is dropped
  (`agent/auxiliary_client.py:8855-8868`). A custom route requires base_url plus
  api_key, or base_url plus a named provider.
- Legacy summary-model note: the default construction site passes
  `summary_model_override=None` (`agent/agent_init.py:2852-2858`), so
  `ContextCompressor.summary_model` is `""` and the model is resolved entirely
  through `call_llm` task routing. The per-compressor `summary_model` pin is the
  legacy override; when set it takes precedence and it is the gate for Section 4.

## Section 2: summary output cap (13 fast-lane + 9 wire cases)

The compression summary request deliberately sends NO `max_tokens` by default
(`agent/context_compressor.py:5500-5521`, comment at 5511-5519: the output cap
must never truncate a summary). A cap is applied only through the opt-in fast
lane, and only forwarded on the wire for specific providers.

Fast lane (`resolve_compression_fast_lane`): a cap is honored only when the
operator selected a concrete provider and model, certified the route as
non-reasoning, and that exact route is the one Hermes will call. The oracle
covers certified cap, model drift, provider drift, reasoning-on, `auto`
provider/model, boolean cap as config drift (`int(True)` is never a cap), zero
and negative caps, string cap parsing, requested-model override match, and a
certified route with no cap field.

Wire forwarding (`_build_call_kwargs` + `auxiliary_max_tokens_param`): the oracle
patches `_current_custom_base_url`, `_read_nous_auth`, and clears
`OPENROUTER_API_KEY` so the recorded parameter reflects only the
provider/model/base_url under test. Findings captured as cases:

- No cap supplied: nothing is added (`no_cap_default_omitted`).
- Plain `custom` route: an explicit cap is DROPPED, including an
  `api.openai.com` host (`plain_custom_cap_dropped`,
  `openai_host_custom_gpt5_cap_not_forwarded`). Forwarding is gated by
  provider/base class (Anthropic-compat, Nous-on-messages, NVIDIA NIM, MoA,
  native Gemini, OpenRouter, managed-local at `agent/auxiliary_client.py:9436-9444`),
  not by host alone.
- OpenRouter forwards the cap. The param NAME is
  `max_completion_tokens` for a GPT-5-family model by name
  (`openrouter_gpt5_by_name_uses_max_completion_tokens`) via
  `model_forces_max_completion_tokens`, and `max_tokens` otherwise.
- NVIDIA NIM forwards `max_tokens`.

## Section 3: summary temperature (7 cases)

Drives `_build_call_kwargs` (which calls `_fixed_temperature_for_model`) and
records both the fixed-temperature verdict and the resolved wire value. The
compression call passes no `temperature`, so the outcome is the model policy:

- Kimi / Moonshot models return the `OMIT_TEMPERATURE` sentinel, so the key is
  stripped and the server default is used (`kimi_*` cases).
- Arcee Trinity thinking models force `0.5`, overriding any caller value.
- Generic models with no caller temperature omit the key; a caller-supplied
  value is kept.

## Section 4: one-shot main-model fallback (80 cases)

The failure classifier and both fallback predicates live inline in
`ContextCompressor._generate_summary`
(`agent/context_compressor.py:5641-5791`). The oracle extracts them by AST:

- The nine classification assignments at lines 5669-5731 (`_status`, `_err_str`,
  `_is_model_not_found`, `_is_timeout`, `_is_json_decode`, `_is_streaming_closed`,
  `_is_empty_content`, `_is_truncated_summary`, `_is_access_or_quota_error`).
- The no-provider early-return test (`agent/context_compressor.py:5653`).
- The fast-path fallback test (`5747-5752`).
- The generic catch-all fallback test (`5781-5785`).

They are compiled straight from source and executed against
`vars(agent.context_compressor)` so every helper is genuine. The matrix crosses
20 distinct failures with 4 compressor states (distinct summary model, same
model, no override, distinct-but-already-fell-back).

Verified findings:

- Same model or no override: both fallback predicates are False. A same-model
  summary failure never retries on the main model; it goes to cooldown. This is
  the load-bearing distinction for the port.
- Distinct model, retryable class (model-not-found, 503, timeout, 429, JSON
  decode, streaming close, empty content, truncated summary): fast path True.
- Distinct model, access/quota/auth (401/402/403, `insufficient_quota`,
  `out of credits`, `no api key was found`): fast path False (these are not in
  the retryable set), but the generic catch-all still arms one main-model retry.
  The error is separately flagged non-retryable via
  `_is_summary_access_or_quota_error` (sets `_last_summary_auth_failure`), which
  is what makes an abort stick if the main-model retry also fails.
- Already fell back: both predicates False. The `_summary_model_fallen_back`
  flag enforces the one-shot guarantee (set by `_fallback_to_main_for_compression`
  at `agent/context_compressor.py:5131-5160`).
- `rate_limited_429` classifies as `_is_timeout` (retryable), even though
  `_is_summary_access_or_quota_error` treats a rate-limit reason as retryable
  (returns False) rather than a permanent auth/quota class.

Ordering note for the port (NOT reflected by evaluating predicates in
isolation): in the real method the branches short-circuit in this order:
1) no-provider RuntimeError returns early with a long cooldown; 2) fast-path
retryable fallback; 3) generic catch-all fallback; 4) cooldown ladder. The
oracle reports each predicate independently, so `no_provider_configured` shows
`generic_fallback: true` in the corpus even though the early return means it is
never reached. Rust must preserve the short-circuit order.

## Exact oracle coverage vs deferred runtime coverage

Exact (decided by the functions this oracle executes, safe to port against the
corpus):

- The full `auxiliary.compression` to `(provider, model, base_url, api_key,
  api_mode)` resolution and precedence.
- Fast-lane cap certification and the parsed cap value.
- Whether a supplied cap is forwarded and under which parameter name.
- The fixed/omit/default temperature verdict.
- The failure classification and the two fallback-eligibility predicates,
  including the same-model versus distinct-model gate and the one-shot guard.

Deferred to Rust live integration tests (network construction or ordering that
this oracle intentionally does not execute):

- Actual client construction and the `auto` discovery chain:
  `resolve_provider_client` (`agent/auxiliary_client.py:6798`), `_resolve_auto`
  (`6660`), `_resolve_auto_route` (`6465`), `_resolve_single_provider` (`6446`),
  and the credential/pool/OAuth resolvers (`_resolve_api_key_provider` at 3180,
  `_resolve_nous_runtime_api` at 3045, `_resolve_xai_oauth_for_aux` at 3082,
  `_resolve_custom_runtime` at 4039). These pick the live key and endpoint.
- The full `call_llm` request path (`agent/auxiliary_client.py:10296`),
  including timeout resolution and the compression timeout floor
  (`_COMPRESSION_TIMEOUT_FLOOR_SECONDS`, `agent/auxiliary_client.py:8884`), and
  the pinned-route stall override (`_pinned_summary_call_kwargs`,
  `agent/context_compressor.py:5533`).
- The exception short-circuit ORDER inside `_generate_summary` and the actual
  recursion into a main-model retry plus the cooldown ladder
  (60/300/900 seconds for timeouts, 30 for json/streaming/empty/truncated, else
  60; `agent/context_compressor.py:5807-5823`). The oracle proves which branch a
  given failure matches, not the surrounding control flow.
- The end-to-end wire body from `_build_call_kwargs` beyond the cap/temperature
  keys (tool stripping, extra_body, reasoning config), which needs a live
  two-endpoint HTTP test.

## Parity traps for the Rust port

- A bare `base_url` with no api_key and no provider falls to `auto`, not
  `custom`. Do not treat base_url alone as a custom endpoint.
- The summary call omits `max_tokens` by default. A hard cap is applied only via
  the certified fast lane and forwarded only for the listed provider classes.
- `int(True)` is 1 in Python; a boolean `max_output_tokens` is config drift and
  must yield no cap.
- The same-model fallback gate: routing compression to a different model through
  `auxiliary.compression` config does NOT set `summary_model` on the compressor,
  so the compressor-level main-model fallback stays disabled. That fallback arms
  only when a caller explicitly pins a distinct `summary_model`. Config-routed
  model failures are handled by `call_llm`'s own failover, not by this predicate.
- Access/quota/auth failures still get one generic main-model retry when a
  distinct summary model is pinned; they are not short-circuited out of the
  fallback, only flagged so a repeat failure aborts.

## Test runs

Generator: writes 129 cases; `--check` returns OK against the checked-in corpus.

Directly relevant Python suites (via `.venv/bin/python -m pytest`):

- `test_fast_compression_lane`, `test_arcee_trinity_overrides`,
  `test_auxiliary_main_first`, `test_auxiliary_openrouter_max_tokens`,
  `test_compression_fallback_budget`, `test_compressor_truncated_summary_guard`,
  `test_compression_stall_fallback_78981`,
  `test_auxiliary_compression_timeout_floor`,
  `test_unsupported_temperature_retry`: 113 passed.
- `test_auxiliary_client`, `test_context_compressor`,
  `test_auxiliary_config_bridge`: 355 passed.
