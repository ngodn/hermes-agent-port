# Auxiliary Compression Fallback Chain: Rust Configuration-Model Lane

> Primary integration note: the draft's per-entry `extra_body` field was
> removed after tracing the live Python request path. Python accepts that key
> in configuration but does not read it for an ordinary configured fallback
> request. The task-level `effective_extra_body` is forwarded instead. The
> final Rust type intentionally models only fields with a live consumer.

## Scope

This report covers only the Rust configuration-model lane for
`auxiliary.compression.fallback_chain`. It documents the authoritative Python
behavior I audited, the typed representation added to
`rust/crates/hermes-gateway/src/compression_auxiliary.rs`, and the deliberate
omissions. I did not touch `main.rs`, `native_agent.rs`, `PORT.md`, `INDEX.md`,
any Python source, generators, or golden JSON. Client resolution, provider
profiles, credential pools, and OAuth refresh stay in startup.

## Python audit

Authoritative sources read in `agent/auxiliary_client.py` and helpers:

- `_try_configured_fallback_chain` (`auxiliary_client.py:6184`): reads
  `auxiliary.<task>.fallback_chain`. If it is missing or not a list, it returns
  no fallback. It iterates entries in source order. Each iteration skips any
  entry that is not a dict, and skips any entry whose `provider` is empty after
  `str(entry.get("provider", "")).strip()`. So the two rejection rules are:
  non-object entries and entries without a nonempty provider.
- `_resolve_fallback_entry` (`auxiliary_client.py:6337`): reads `provider`,
  `model`, `base_url`, `api_key` (via `_fallback_entry_api_key`), and
  `api_mode or transport`. A literal `"auto"` model is passed straight to the
  router; it is not dropped the way the primary compression section drops
  `"auto"`.
- `_fallback_entry_api_key` -> `hermes_cli.fallback_config.resolve_entry_api_key`
  (`fallback_config.py:14`): inline `api_key` first (trimmed, nonempty wins),
  then `key_env`, then `api_key_env`. `key_env` takes precedence over
  `api_key_env`. The env name is resolved through `agent.secret_scope.get_secret`,
  which reads `os.environ` when there is no active multiplexed scope.
- `_coerce_positive_timeout` (`auxiliary_client.py:5546`) and
  `_fallback_entry_timeout` (`auxiliary_client.py:5558`): a per-entry `timeout`
  is accepted only when it is an `int` or `float` (not `bool`) and `> 0`,
  returned as a `float`. Strings, booleans, non-positive numbers, null, and
  containers all yield `None`.
- `_call_fallback_candidate_sync` (`auxiliary_client.py:5686`): when an entry
  supplies its own `timeout`, that value replaces the task-level effective
  timeout for that request (`5720-5727`). It does not go through
  `_effective_aux_timeout`, so the 300s compression floor
  (`_COMPRESSION_TIMEOUT_FLOOR_SECONDS`, `auxiliary_client.py:8884`) is not
  applied to per-entry timeouts. The floor is only added at
  `_effective_aux_timeout` (`9077-9078`) when the caller passes no explicit
  timeout, which is the task-level path, not the entry path.
- `resolve_compression_fast_lane` (`auxiliary_client.py:8971`) via
  `_fast_lane_config_fields` (`8939`): when called with `route_config=<entry>`,
  it reads only `provider`, `model`, `reasoning_effort`, and
  `max_output_tokens` from the entry. `reasoning_effort` is parsed by
  `hermes_constants.parse_reasoning_effort`; `max_output_tokens` becomes a
  positive int cap (booleans rejected, `int(True) == 1` guarded).
- `normalize_api_mode` parity is the module's existing `normalize_api_mode`
  helper, which matches `hermes_cli/config.py` canonicalization
  (`chat_completions`, `codex_responses`, `anthropic_messages`,
  `bedrock_converse`), case-insensitive, unknown strings passthrough.

### One finding worth stating plainly

The per-entry `extra_body` field is not consumed anywhere in the ordinary
Python summary fallback path. `_compression_fast_lane_controls`
(`auxiliary_client.py:9020`) receives the task-level `effective_extra_body`,
not the entry's `extra_body`, and it passes `route_config=<entry>` only into
`resolve_compression_fast_lane`, which never reads `extra_body` off
`route_config`. The helper draft initially retained the field because the task
brief listed it. Primary integration removed it rather than add a speculative
interface with no live Python consumer.

## What I implemented

In `rust/crates/hermes-gateway/src/compression_auxiliary.rs`:

1. `FallbackChainEntry` (new `pub(crate)` struct, `#[derive(Clone, Debug,
   PartialEq)]`) with fields `provider`, `model`, `base_url`, `api_key`,
   `key_env`, `api_mode`, `timeout`, `reasoning_config`, and
   `max_output_tokens`. The primary integration removed the draft-only
   `extra_body` field for the reason above.
2. `FallbackChainEntry::from_value` parses one element, returning `None` for
   non-object entries and for entries with an empty provider, reusing the
   module's existing `text`, `positive_integer`, `normalize_api_mode`,
   `reasoning_effort::parse_value`, and `python_value::truthy` helpers so
   coercion matches the primary `Config`.
3. `FallbackChainEntry::direct_api_key` mirrors `Config::direct_api_key`:
   inline key first, then `key_env` looked up against the supplied environment
   then dotenv, trimmed with Python whitespace semantics, empty filtered out.
   No provider profiles or pools are touched, matching the brief.
4. `entry_timeout_seconds`, a dedicated helper that is faithful to
   `_coerce_positive_timeout`: accepts only a positive finite number, rejects
   booleans, non-positive numbers, and strings, and never applies the 300s
   floor. This is intentionally stricter than the task-level `timeout_seconds`,
   which floors and accepts numeric strings.
5. `Config` gains `pub fallback_chain: Vec<FallbackChainEntry>`, populated in
   `Config::from_value` from `section.get("fallback_chain")` as an array,
   mapped through `FallbackChainEntry::from_value` in source order. A missing
   or non-array value yields an empty `Vec`. The existing primary `Config`
   behavior is otherwise unchanged.

### Deliberate coercion choices

- Provider is lowercased at parse time. Python keeps the entry's original case
  in the dict, but every routing consumer (`resolve_compression_fast_lane`,
  `BackendIdentity`) lowercases before comparing, so this is behavior
  preserving and matches the primary `Config` provider convention. Only the
  cosmetic display label in Python keeps original case.
- `model` keeps a literal `"auto"` for entries. The primary section drops
  `"auto"` to `None`; entries do not, because Python passes the entry model
  straight to the router.
- `api_mode` wins over the `transport` alias, matching
  `entry.get("api_mode") or entry.get("transport")`.
- Per-entry `timeout` strings are rejected (Python `_coerce_positive_timeout`
  rejects strings). This differs from the primary `timeout_seconds`, which
  parses numeric strings, and the difference is intentional.

### Not implemented here (owned elsewhere or deferred)

- Client construction, provider-profile resolution, credential pools, OAuth
  refresh: owned by startup, out of this lane.
- Failure-scoped candidate skipping (`FailureScope`, `should_skip_candidate`),
  minimum context-window filtering (64,000 tokens), and secondary main-agent
  fallback: these are execution-time behaviors, not configuration parsing, so
  they are not part of the configuration-model lane.
- Responses, Anthropic Messages, and Bedrock transports: still deferred in
  native Rust; the entry only records the normalized `api_mode` string.

## Commands and results

```
$ rustfmt --edition 2021 crates/hermes-gateway/src/compression_auxiliary.rs
rustfmt OK
```

```
$ cargo test -p hermes-gateway compression_auxiliary
running 12 tests
test compression_auxiliary::tests::fallback_chain_defaults_empty_and_ignores_non_list ... ok
test compression_auxiliary::tests::defaults_inherit_main_route_and_apply_compression_timeout_floor ... ok
test compression_auxiliary::tests::fallback_entry_preserves_auto_model_and_normalizes_provider ... ok
test compression_auxiliary::tests::fallback_entry_api_mode_precedence_and_canonicalization ... ok
test compression_auxiliary::tests::fallback_entry_credential_precedence ... ok
test compression_auxiliary::tests::fallback_chain_keeps_source_order_and_rejects_invalid_entries ... ok
test compression_auxiliary::tests::fallback_entry_timeout_is_independent_and_unfloored ... ok
test compression_auxiliary::tests::fallback_entry_retains_reasoning_and_output_cap ... ok
test compression_auxiliary::tests::route_selection_matches_python_config_boundaries ... ok
test compression_auxiliary::tests::hard_cap_requires_exact_concrete_non_reasoning_route ... ok
test compression_auxiliary::tests::fast_lane_certification_matches_source_executed_python_corpus ... ok
test compression_auxiliary::tests::task_config_resolution_matches_source_executed_python_corpus ... ok

test result: ok. 12 passed; 0 failed; 0 ignored; 0 measured; 1792 filtered out
```

The 4 pre-existing tests still pass; the 8 draft tests cover empty and non-list
containers, source-order parsing with rejection of non-object and no-provider
entries, `"auto"` model preservation and provider normalization, independent
unfloored timeout coercion (integer, fractional, and rejected string, boolean,
zero, negative, null, omitted), `api_mode` over `transport` precedence with
canonicalization, credential precedence (inline over env, `key_env` over
`api_key_env`, env trimming), and retention of reasoning config and output cap
with the boolean-cap guard.
