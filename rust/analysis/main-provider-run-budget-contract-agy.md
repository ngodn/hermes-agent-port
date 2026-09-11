# Main-provider run-budget contract

AGY was assigned the independent Python contract and source-executed fixture
lane. Its wrapper produced a broad draft generator, then exited while its own
background task was still running, before it wrote this report or the JSON
corpus. Primary review stopped the blocked draft after more than three minutes
and replaced its repeated full-agent construction with `AIAgent.__new__` test
doubles containing only fields read by the live timeout methods.

The retained contract executes the real Python
`_normalize_run_budget_seconds`,
`AIAgent._compute_non_stream_stale_timeout`, and
`_derive_stream_stale_timeout` functions. It does not copy the timeout formula
into a simulator.

## Frozen behavior

- Null, booleans, non-numeric values, NaN, and non-positive values disable the
  run budget. Positive integers, floats, numeric strings, and positive infinity
  are accepted.
- The turn clock must exist before the cap applies. A configured budget with no
  started clock leaves the ordinary timeout unchanged.
- The buffered cap is `max(60, remaining * 0.5)` and can only lower the timeout.
- Context scaling happens before the run-budget cap.
- Default and reasoning-floor timeouts yield to the cap.
- Model, provider, and `HERMES_API_CALL_STALE_TIMEOUT` settings are explicit
  and never yield to it.
- A plain local endpoint with the implicit default remains unbounded. A local
  reasoning model retains its finite reasoning floor, which can then be capped.
- Streaming timeout derivation is independent of the run budget.

## Executed evidence

The checked-in corpus contains 32 cases:

- 15 normalization inputs, retaining each raw JSON value and its Python type
- 14 buffered timeout cases covering fresh, elapsed, expired, explicit,
  context-scaled, and local routes
- 3 streaming cases computed both without a budget and with an expired budget

The generator writes in under one second, a second write retains the same
SHA-256 digest, and `--check` proves byte-for-byte parity. The focused Python
suite `tests/agent/test_run_budget.py` passes 28 tests.

Artifacts:

- `rust/tools/gen_main_provider_run_budget_goldens.py`
- `rust/tools/main-provider-run-budget-goldens.json`
