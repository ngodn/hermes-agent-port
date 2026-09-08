# Automatic compression policy, AGY lane

AGY owned only the pure policy module in
`rust/crates/hermes-gateway/src/automatic_compression.rs`. It did not work on
SQLite, ingress, provider clients, or publication. The main integration lane
then checked the result against `agent/agent_init.py` and
`agent/context_compressor.py` before using it.

## Accepted work

- `compression.enabled`, `threshold`, `threshold_tokens`, `model_thresholds`,
  `protect_last_n`, `protect_first_n`, and `max_attempts` parsing
- longest substring model override with insertion-order tie breaking
- disabled, below-threshold, attempt-limit, externally blocked, and attempt
  decisions
- 13 table-driven policy tests

## Corrections made during source verification

- Small context windows below 512K raise the ratio to at least 75%.
- Output reservation is subtracted before ratio math.
- The 64K threshold floor is bounded by the 85% degenerate-window guard.
- The optional absolute token cap applies after those calculations.
- The string `on` is false for `compression.enabled`.
- Numeric non-boolean model overrides are accepted, including values Python
  does not range-check. String and boolean overrides are rejected.
- Python integer coercion accepts booleans and truncates positive fractional
  numbers for the count and absolute-token settings covered here.

The original helper expectations missed several of these cases. The checked
implementation and tests, not the first helper draft, are authoritative.

## Integration completed by the main lane

The policy now drives HTTP and push pre-turn compression. The live path uses
the provider-visible request estimate, resolved model context length and output
reservation, durable cooldown and breaker state, configured attempt limits,
complete-turn head/tail protection, and in-place or rotation publication.

## Remaining Python behavior

Provider-reported usage recalibration, token-budget tail selection, exact
role-collision handling for every requested protected-head count, structural
no-op backoff, the 10% final-request savings breaker, deterministic tool-result
pruning, micro-compaction, and overflow retry policy remain open.

Validation at this checkpoint: all 13 policy tests pass, the two live automatic
ingress tests pass, and the selected Python policy/guard oracle passes 26 cases.
