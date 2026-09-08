# Provider usage helper disposition

AGY owned the bounded source-mapping lane for `agent/usage_pricing.py` and
produced an initial Rust parser. The helper was useful for locating provider
keys, native API-mode differences, trailing SSE usage chunks, and relevant
Python regression tests. The primary agent then source-reviewed and replaced
the oversized draft before integration.

## Accepted findings

- `CanonicalUsage` keeps fresh input, output, cache-read, cache-write,
  reasoning, and request-count buckets.
- Chat Completions reads prompt/input and completion/output fallbacks, then
  subtracts cache read and write tokens from the prompt total.
- Anthropic Messages treats top-level `input_tokens` as fresh input.
- Codex Responses subtracts cached and cache-write tokens from total input.
- Chat cache details take precedence over provider-specific top-level
  fallbacks used by DeepSeek, Kimi/Qwen, and Anthropic-compatible proxies.
- Reasoning tokens come from output details before completion details.
- Streaming usage can arrive in a final SSE object with an empty `choices`
  array. Non-streaming usage lives on the response root.

Authoritative references:

- `agent/usage_pricing.py:73-109`
- `agent/usage_pricing.py:1048-1076`
- `agent/usage_pricing.py:1297-1450`
- `tests/agent/test_usage_pricing.py`
- `tests/run_agent/test_partial_stream_finish_reason.py:916-952`

## Rejected parts of the draft

The initial helper implementation was not accepted as written. It rejected
`true` counters even though Python's `int(True)` is `1`, added unsupported
top-level cache/reasoning fallbacks, mixed Chat and Codex detail precedence,
and exposed a large unused convenience API. Those choices would have silently
changed accounting behavior.

The accepted module is `rust/crates/hermes-gateway/src/provider_usage.rs`. It
keeps the narrow API required by the gateway, matches Python boolean and
numeric coercion within Rust's saturating `u64` boundary, and has focused tests
for all three native API modes plus streaming and non-streaming extraction.

## Integration outcome

The primary agent wired the accepted parser into every native provider call:

- Streaming main turns request and consume usage chunks, except on the native
  Gemini host where Python does not request stream usage.
- Non-streaming tool-loop and compression calls consume root usage.
- Main-turn deltas persist to session totals and the per-model ledger.
- Compression deltas persist only to the auxiliary `compression` task bucket.
- Session totals and the per-model main row update in one SQLite transaction.

This report records helper contribution and disposition. It is not a claim
that the helper draft was merged unchanged.
