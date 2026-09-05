# Live image capability lookup

Codex checked the Python lookup and its immediate dependencies after Gemini's
audit. Raw output is retained in `audit-live-capability.agy.log`; this document
replaces the draft where its details were incorrect or unverified.

## Implemented boundary

`image_routing.rs` now includes inference URL/key resolution from an explicit
turn runtime and config. Its 1,164 source-executed fixture cases cover runtime
precedence, provider aliases, legacy lists, non-object config, Python coercion,
and whitespace. The fixture is stable under Python hash seeds 7 and 41.
Workspace validation passed with 1,015 tests and one existing ignored bridge
test. Formatting and Clippy with warnings denied passed. Logs are
`takeover-endpoint-workspace-tests.log` and `takeover-endpoint-clippy.log`.

Python constructs dictionary candidates in a set. Rust searches the requested
provider and its alias, then the configured provider and its alias. Conflicting
dictionary candidates therefore have a documented deterministic order rather
than exact equivalence to every Python process. Legacy list order is preserved.

Runtime URL selection checks provider identity; runtime API key selection does
not. This is preserved behavior, not proof that a selected key belongs to the
resolved endpoint. The runtime is supplied explicitly and is not process-global.

## Lookup order (now implemented in LiveVisionLookup)

Source: `agent/image_routing.py::_lookup_supports_vision`.

1. If requested_provider is absent, borrow it from the turn runtime only when
   normalized provider and model identities both match.
2. Apply the existing config capability override. A definite false also wins.
3. Return unknown when provider or model is empty.
4. Try managed local runtime capabilities before the cloud catalog.
5. Query models.dev with network allowed on a cold cache.
6. Resolve URL/key, default missing Ollama URL to localhost:11434/v1, and probe
   eligible Ollama servers. Unknown results fall back to text routing.

The managed, catalog, and Ollama stages each have defensive exception boundaries.
They must not turn a failed lookup into a positive capability assertion.

## Local probe dependency

Update: `local_probe.rs` now implements detection, show, and their caches.
See [verification](local-probe-verification.md) for proof and remaining limits.
Endpoint locality and the Ollama fallback are now implemented; see
[locality verification](endpoint-locality-verification.md). Provider-prefix
resolution below still needs the real provider registry.

Source: `agent/model_metadata.py`.

Keep endpoint classification, URL normalization, detection, its cache, and
Ollama /api/show together in one focused module. Inline tests should exercise
real HTTP against temporary local servers and source-derived response cases.

- Detection normalizes whitespace/trailing slashes, rewrites an anchored
  lowercase http(s) localhost host to IPv4, and removes a final /v1.
- Probe order is LM Studio /api/v1/models, Ollama /api/tags, llama.cpp /v1/props
  (then /props on non-200), and vLLM /version. Each leg has a two-second timeout.
  Preserve each response predicate; HTTP 200 alone is sufficient only for
  the LM Studio leg.
- Successful memory verdicts last 3,600 seconds; negative memory verdicts
  last 300 seconds. These are not lifetime caches despite the stale Python
  function docstring.
- Positive disk results live in cache/local_endpoint_probes.json for 300
  seconds. Negative results are never written to disk.
- Connect timeouts suppress further probes for that host:port for 30 seconds.
  Read timeouts must not mark a host unreachable. No network call occurs while
  consulting the cache.
- /api/show requires Ollama detection first, sends the resolved bearer key
  when non-empty, and has a three-second timeout. A non-empty capabilities
  list without vision returns false before considering legacy model_info.
- Prefix stripping depends on the provider-profile registry and Ollama tag
  pattern. Do not replace it with unconditional splitting on a colon.
- Local suffixes are exactly .docker.internal, .containers.internal, and
  .lima.internal. Python also accepts unqualified hostnames before IP parsing,
  private/loopback/link-local addresses, and Tailscale CGNAT. Its malformed
  dotted-address fallback and Python-version IP semantics need fixture coverage.

## Managed runtime and catalog dependencies

Update: the immediate managed vision stage is implemented in
managed_capabilities.rs with explicit root and catalog inputs. See
[verification](managed-capability-verification.md). Curated managed catalog
loading/refresh is also implemented, including a shared packaged constructor;
see [catalog verification](managed-catalog-verification.md). Cloud registry
caching is now implemented; see [verification](cloud-catalog-verification.md).
Cloud capability/context metadata and overrides are now implemented; see
[metadata verification](cloud-metadata-verification.md). The combined lookup and native registration are now implemented; see
[registry verification](provider-registry-verification.md). Discovery, provider
hooks, supervisor lifecycle, and runner construction remain.

Source: `hermes_cli/local_runtime/capabilities.py`.

Only staged models qualify. Live /props wins; its modalities.vision field uses
Python truthiness, not a boolean-only type check. If unknown, the catalog and
actual projector file determine capability. Runtime paths, staging/split-file
rules, state liveness, endpoint matching, and provider registry are separate
source dependencies that still require full inspection and porting.

Source: `agent/models_dev.py`.

The catalog is more than a cached vision flag. Preserve provider mapping,
exact/case-insensitive/cloud-suffix model matching, explicit model_overrides,
and _default overrides only on catalog misses. modalities.input lists override
the older attachment flag, including an empty list.

Cache work must preserve four-hour freshness, five-minute failure backoff,
stale-data refresh, fetch serialization, registry validation, and ETag coupling.
Conditional GET requires a servable registry. A 304 without one clears the
sidecar and applies backoff. Network fetch uses five-second connect and
ten-second read timeouts. Re-read fetch_models_dev and its helpers when
implementing the full cache state machine.

## Corrections to the helper draft

- hosted_room_peer::urlsplit is private, not a reusable public API.
- config_file.rs does not implement the claimed atomic JSON write pattern.
- Generic .internal, .local, and .localhost suffixes are not in the source list.
- Managed props uses bool(value), not a boolean-only check.
- Catalog overrides and model matching cannot be omitted from capability lookup.
- Do not duplicate the completed endpoint resolver in a new local_endpoint file.

After these components exist, implement VisionCapabilityLookup and supply it
to the session routing wrapper. Then connect attachment preparation and the
resolved session model to Dispatcher. Prepared-part HTTP transport is already
tested; rich adapter downloads and live provider routing are still incomplete.
