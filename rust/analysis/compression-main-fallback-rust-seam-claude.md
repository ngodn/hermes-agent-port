# Rust seam: top-level main-model fallback chain in auto-mode compression

Scope: design the narrowest production seam for consulting the top-level
`fallback_providers` / legacy `fallback_model` chain after
`auxiliary.compression.fallback_chain` when compression runs in provider auto
mode. This is a Rust-only seam lane. The Python contract and its goldens are
owned by the parallel agy lane
(`rust/analysis/compression-main-fallback-chain-contract-agy.md`,
`rust/tools/compression-main-fallback-chain-goldens.json`); this document does
not re-derive that contract, it only cites the parts that pin a Rust interface.

Baseline for every "before" reference below is commit `b069a54111` (HEAD),
which ships the task chain only. Note for the reader: the working tree already
contains an in-flight implementation of this exact seam (another lane is
landing it live), so the symbol names used here map to real code you can read
today. Where the working tree and this design agree I say so plainly rather
than inventing an alternative; where a real judgment call exists I flag it.

## 1. What the Python contract forces onto the Rust interface

The runtime path a live compression summary takes is `call_llm(task="compression")`
in `agent/auxiliary_client.py`. In auto mode its fallback ladder is:

1. main provider request,
2. `_try_configured_fallback_chain` (the per-task
   `auxiliary.compression.fallback_chain`), at most one candidate,
3. `_try_main_fallback_chain` (top-level `fallback_providers` / `fallback_model`
   via `hermes_cli/fallback_config.py::get_fallback_chain`), at most one
   candidate,
4. `_try_payment_fallback` and the built-in discovery chain (not ported).

Two contract facts are load-bearing for the Rust seam:

- Mutual exclusion at the request layer. In `call_llm` (around
  `agent/auxiliary_client.py:11115`) `_try_main_fallback_chain` is only reached
  when `_try_configured_fallback_chain` returned no client. If the task chain
  produced any eligible candidate, that candidate runs and the main chain is
  never consulted, even when the task candidate then fails its request. So
  across both fallback tiers combined, at most one runtime candidate executes.

- The main-tier entry gets no per-entry request tuning.
  `_call_fallback_candidate_sync` (`agent/auxiliary_client.py:5686`) resolves a
  per-entry timeout through `_fallback_entry_timeout`, whose regex
  (`agent/auxiliary_client.py:5525` `_fallback_chain_entry`) matches only
  `fallback_chain[<i>]` labels. Main-chain candidates carry
  `fallback_providers[<i>]` labels, so the per-entry timeout resolves to `None`
  and the task-level deadline stands. The same `_fallback_chain_entry` miss
  feeds `_compression_fast_lane_controls` an empty `route_config`, so the
  per-entry `max_output_tokens` cap and the per-entry reasoning fast-lane never
  apply either. Main-chain entries inherit the task-level timeout and the
  task-level `extra_body` / reasoning unchanged.

`get_fallback_chain` itself (`hermes_cli/fallback_config.py`) requires both a
nonempty `provider` and `model` per entry (unlike the task chain, which keeps
provider-only entries), accepts a single dict or a list of dicts, merges
`fallback_providers` then `fallback_model` in that order, and deduplicates by
`(provider, model, base_url)` lowercased with `base_url` stripped of a trailing
slash.

`_try_main_fallback_chain` (`agent/auxiliary_client.py:6363`) skips any entry
whose lowercased provider is in `{failed_provider, main_provider, "auto"}`, and
applies the 64,000-token compression context floor
(`_task_minimum_context_length`, `agent/auxiliary_client.py:6121`). That skip is
provider-wide and provider-name only. It is not the model/base-url-aware
predicate the task chain uses.

## 2. Reusable parsing and provider resolution

Parsing. `main_fallback_chain(root: &Value) -> Vec<FallbackChainEntry>` belongs
next to the task parser in `compression_auxiliary.rs`. It reuses the existing
`FallbackChainEntry::from_value` after enforcing the two extra `get_fallback_chain`
rules the task parser does not: provider-and-model both present, and
`(provider, model, base_url)` dedup across the two source keys in order. The
dict-or-list coercion mirrors `_iter_fallback_entries` (a bare object is a
one-element list). This reuses every scalar coercion already proven against the
task goldens (`text`, `normalize_api_mode`, `positive_integer`,
`entry_timeout_seconds`, `reasoning_effort::parse_value`, `truthy`) and the
`direct_api_key` credential lookup, so there is one coercion path, not two.

Provider resolution. The whole resolution surface is already
`build_native_compression_client` in `main.rs` (profile lookup and inherit-main,
`custom_provider_config::named` for keyed/legacy providers, base-url and key
resolution, `api_mode` gating to `chat_completions`, `models_dev`
context-window lookup, extra-header projection). The seam reuses it unchanged by
adding a third `CompressionRouteConfig` variant. No new resolution code is
introduced, which is the main reason this seam is narrow.

## 3. Where the frozen route plan encodes tier order

The plan lives in `CompressionRoutes` (`native_agent.rs:450` at HEAD, one
`fallbacks: Vec<NativeAgentClient>`). The seam splits that into two named
tiers:

```
struct CompressionRoutes {
    primary: Option<NativeAgentClient>,
    task_fallbacks: Vec<NativeAgentClient>,   // auxiliary.compression.fallback_chain
    main_fallbacks: Vec<NativeAgentClient>,   // top-level fallback_providers/fallback_model
    main_first: bool,
    initial_failure: Option<(BackendIdentity, FailureScope)>,
}
```

Tier order is materialized in `summarize_history_with_memory`
(`native_agent.rs:1268`) when it builds the `routes` vector. Auto mode is
`main_first == true`, set from `!compression_policy.needs_separate_client` at the
`with_compression_routes` call site (`main.rs:912` at HEAD). The auto-mode plan
becomes: `Main`, then `task_fallbacks`, then `main_fallbacks`. The explicit-mode
plan is unchanged and never appends `main_fallbacks`, matching the Python
`is_auto` guard. An `AuxiliaryKind` enum (`Primary` / `TaskFallback` /
`MainFallback`) is the clean way to carry the tier through `PlannedRoute` so the
per-tier skip rule can branch on it.

The order is frozen at construction: `main_fallback_chain` is read once during
`build_agent_client_for_home`, resolved to clients, and stored. Nothing in the
turn loop re-reads config, which preserves the existing "route clients never
carry their own plan" invariant (they get `compression_routes:
Default::default()`), so a main-fallback route cannot itself recurse into
another fallback chain.

## 4. Preventing more than one runtime candidate from executing

Use a single shared gate, not one per tier. The HEAD traversal already has
`configured_fallback_attempted`. The seam keeps exactly one such flag and lets
both `TaskFallback` and `MainFallback` share it:

- at the top of a fallback-tier route, `if configured_fallback_attempted { continue; }`,
- the 64K context skip and the provider-repeat skip both `continue` without
  setting the flag (an ineligible route does not consume the one shot),
- the flag is set to `true` only immediately before the request fires.

This is exactly the Python mutual-exclusion contract from section 1: if any
task-chain candidate is eligible it runs and sets the flag, so no main-chain
candidate runs even if the task candidate fails; if no task candidate is
eligible, the flag is still false when the first eligible main-chain candidate
is reached, so it runs. One shared flag therefore delivers both properties at
once: at most one runtime candidate across the two tiers, and task-chain-wins
ordering. A second independent `main_fallback_attempted` flag would be wrong: it
would let a failed task candidate fall through into a main candidate, which the
request layer never does.

The working tree implements precisely this shared-flag design
(`native_agent.rs` traversal, `AuxiliaryKind::TaskFallback | MainFallback`
sharing `configured_fallback_attempted`). I reviewed it against the contract and
it holds; I am not proposing a different structure.

Main-tier skip predicate. The task tier keeps
`compression_auxiliary::should_skip_candidate(candidate, failed, scope)`
(model/credential scoped, base-url aware). The main tier must instead skip on
provider name alone: skip when the candidate provider equals the failed
provider or the main conversation provider. In auto mode `Main` runs first, so
`first_failure` is populated from the main route and its provider is the main
provider, which also covers the `"auto"` element of the Python skip set: an
entry with provider `"auto"` or `"main"` resolves through
`build_native_compression_client`'s inherit-main path to the main provider
identity and is caught by the main-provider comparison. Do not try to fold this
into `should_skip_candidate`; keeping it a small inline predicate preserves that
function's model/credential-scope meaning.

## 5. How top-level per-entry controls differ from task entries

The differences are contained entirely in the new
`CompressionRouteConfig::MainFallback { entry, task }` accessors in `main.rs`,
so the resolver body stays shared:

- `timeout()` returns `task.timeout`, never `entry.timeout`. Mirrors
  `_fallback_entry_timeout` not matching `fallback_providers[<i>]` labels.
- `reasoning_config()` returns `None`. No per-entry fast-lane certification.
- `certified_output_cap()` returns `None`. No per-entry `max_output_tokens` cap.
- `request_extra_body()` returns the task-level `extra_body` (with the
  `claims_fast_lane` reasoning strip, since a main entry is never certified),
  matching Python passing `effective_extra_body` through unchanged with an empty
  `route_config`.
- `direct_api_key()` reuses `entry.direct_api_key` so an inline `api_key` or a
  `key_env` / `api_key_env` name still resolves per entry. This is the one
  per-entry control the main tier does keep, matching
  `fallback_config.resolve_entry_api_key`.

Interface-growth flag. Reusing `FallbackChainEntry` for main entries carries
three fields the `MainFallback` variant deliberately ignores (`timeout`,
`reasoning_config`, `max_output_tokens`). That is a shallow-looking but
defensible reuse: it keeps a single parser and a single golden-tested coercion
path, and the route-config layer is the correct place to drop the fields. The
alternative, a thinner `MainFallbackEntry`, buys a tighter type at the cost of a
second parser and duplicated coercion, and would need its own goldens. Keep the
reuse, but the inert fields must be documented at the parser and at the
`MainFallback` accessors so nobody later wires `entry.timeout` into the main
tier believing it is live. The integration test in section 7 is what pins this:
it feeds a main entry `timeout`, `reasoning_effort`, and `max_output_tokens` and
asserts none of them reach the wire.

`with_compression_routes` grows by one `Vec` parameter. That is minimal,
non-speculative growth given the single production call site; a config struct
would be over-engineering here.

## 6. Preserving secret scope and prompt byte stability

Secret scope. The main-fallback build loop must run inside the same
`build_agent_client_for_home` block as the primary and task loops, reusing the
same `dotenv` snapshot and the same `environment` closure
(`secret_scope::get_secret(name, None)`). That closure already fails closed when
multiplexing is active without a scope. Because each entry's `key_env` /
`api_key_env` resolves through that one closure, a main-fallback entry reads
credentials in the active profile's scope exactly like every other route, and no
new scope surface is created. This matches `resolve_entry_api_key` in the Python
helper, which routes `key_env` through `agent.secret_scope.get_secret` for the
same reason. Do not read env directly in the new loop.

Prompt byte stability. The compression prompt is assembled once by
`compression_prompt::build_with_memory` at the top of
`summarize_history_with_memory` and the same `&prompt` is handed to every
route's `summarize_history_on`. Appending a tier adds routes, not prompt
variants, so the prompt bytes and therefore the prompt-cache prefix are
identical across main, task, and main-fallback routes. The seam touches route
selection only; it must not touch prompt assembly.

## 7. Local HTTP integration tests that prove production wiring

Unit level, in `native_agent.rs` alongside
`compression_route_plan_preserves_before_main_and_after_main_order`: extend the
axum-per-route harness with a main-fallback server and assert the auto-mode
order `Main` then task then main, and that a `main_fallbacks` entry whose
provider equals the main provider is skipped (duplicate-main case). Add a case
proving the shared one shot: an eligible task fallback that returns an unusable
response must stop the ladder before any main-fallback server is hit.

Parser unit tests in `compression_auxiliary.rs`: merge order
(`fallback_providers` before `fallback_model`), `(provider, model, base_url)`
dedup, single-dict coercion, and provider-or-model-missing rejection. Wire these
to the agy goldens (`compression-main-fallback-chain-goldens.json`) once that
lane lands, the same way the task parser binds to
`compression-fallback-chain-goldens.json`.

End-to-end level, in `main.rs` alongside
`compression_routes_preserve_explicit_and_auto_fallback_order`: drive
`build_agent_client_for_home` with `model.provider = "custom"`,
`auxiliary.compression.provider = "auto"` (so `needs_separate_client` is false
and `main_first` is true), and `fallback_providers = [duplicate-main-entry,
real-entry-with-own-base_url-and-key]`. Point each provider at its own local
axum server, make main and the duplicate return unusable summaries, and the real
main-fallback return a usable one. Assert:

- the main-fallback server is hit exactly once,
- it received the entry's own `Authorization` bearer and `model`,
- the task-level `extra_body` marker is present on the wire,
- a per-entry `timeout`, `reasoning_effort`, and `max_output_tokens` on that
  entry do not produce `max_tokens` / `max_completion_tokens` / `reasoning` on
  the wire (proves the section 5 per-entry differences),
- the duplicate-main entry never reaches its server.

The working tree already contains this end-to-end test (`main.rs` around the
`fallback_providers` fixture, asserting `main_fallback_guard.len() == 1`,
`Bearer main-fallback-key`, `body["route_marker"] == "auto-task"`, and absent
`max_tokens` / `reasoning`). It is the right shape and should be kept as the
production-wiring proof.

## 8. Recommended implementation sequence

1. Parser. Add `compression_auxiliary::main_fallback_chain(root)` reusing
   `FallbackChainEntry::from_value` plus the provider-and-model requirement,
   dict-or-list coercion, and `(provider, model, base_url)` lowercased dedup
   across `fallback_providers` then `fallback_model`. Unit-test merge, dedup,
   singleton dict, and rejection. Document the inert fields.
2. Route config. Add `CompressionRouteConfig::MainFallback { entry, task }` with
   the section 5 accessors (`timeout = task.timeout`, `reasoning = None`,
   `certified_output_cap = None`, `request_extra_body = task-level`,
   `direct_api_key = entry`).
3. Plan storage. Split `CompressionRoutes.fallbacks` into `task_fallbacks` and
   `main_fallbacks`; add the `main_fallbacks: Vec<NativeAgentClient>` parameter
   to `with_compression_routes`.
4. Traversal. Add `AuxiliaryKind::MainFallback`; append `main_fallbacks` after
   `task_fallbacks` only when `main_first`; keep the single shared
   `configured_fallback_attempted` gate; branch the skip predicate so the main
   tier uses provider-name equality against the failed and main providers; keep
   the 64K context floor.
5. Startup. After the task loop, and only when
   `!compression_policy.needs_separate_client`, iterate
   `main_fallback_chain(user_config)`, skip a literal `provider == "auto"`
   entry, build each through `CompressionRouteConfig::MainFallback` with the same
   `dotenv` and `environment`, collect into `main_compression_fallbacks`, and
   pass it into `with_compression_routes`. Include the collection in the
   "install routes only if any exist" guard.
6. Tests. Add the unit route-order and shared-one-shot tests, the parser tests,
   and the end-to-end HTTP test from section 7.
7. Parity. Bind the parser tests to the agy goldens once
   `compression-main-fallback-chain-goldens.json` exists, and reconcile the skip
   set, dedup key, and 64K floor against that oracle.

## 9. Open item for the contract oracle

The one genuine semantic call is the shared-versus-independent one shot
(section 4). I recommend the shared flag because the live compression path is
the `call_llm` request layer, where the task and main chains are mutually
exclusive. The resolution-layer path `_resolve_auto_route`
(`agent/auxiliary_client.py:6625`) tries both chains when the primary client
cannot even be built, which is a different situation the Rust port collapses by
resolving all clients at startup. The agy lane should confirm that the comp
route executor is meant to follow the request-layer semantics; if it is instead
meant to follow resolution-layer semantics, the gate would need to split. Every
other behavior here (dedup key, provider-wide main-tier skip, 64K floor,
per-entry control suppression) is a direct read of the current runtime and does
not need adjudication.
