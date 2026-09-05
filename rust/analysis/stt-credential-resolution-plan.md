# STT credential resolution dependency audit

The next runtime integration must preserve `_resolve_openai_audio_client_config`
in `tools/transcription_tools.py`, including its selection gates and lazy effect
order. It cannot reuse the model endpoint credential ladder unchanged.

100 executable Python scenarios now live in `tools/stt-credential-goldens.json`.
The generator executes the actual resolver and local/private URL predicate with
recorded direct-key and managed-gateway calls. All credentials and endpoints in
these fixtures are synthetic. Selection-error text is supplied by a small stub;
removed-provider notes and enabled-but-unavailable entitlement details require
additional cases when those dependencies are ported.

| Stored selection | Allowed resolution |
| --- | --- |
| nous | Managed openai-audio gateway only, even when a direct/config key exists |
| Any other explicit selection | Config key, keyless private/local endpoint, then direct voice/OpenAI key; no managed fallback |
| Never configured | Config key, keyless private/local endpoint, direct key, then managed gateway |

Important source findings:

- Selection reads raw config.yaml, not schema-merged config. Otherwise the
  seeded stt.provider default would incorrectly suppress legacy discovery.
- Legacy use_gateway: true selects nous regardless of the provider name.
- A configured remote base_url without a config key does not accompany a
  fallback environment key: that key uses OPENAI_BASE_URL. Pairing it with the
  custom remote endpoint would change the reference and expose credentials.
- The dedicated VOICE_TOOLS_OPENAI_KEY wins over OPENAI_API_KEY. Both go through
  profile-aware secret resolution; only the latter can use the openai-api pool.
- Multiplexed profile scopes are authoritative. Global env and credential-pool
  fallback must not occur after a scoped miss.
- The existing Rust local_probe::is_local_endpoint is not interchangeable with
  STT's _is_local_or_private_url: it accepts bare names and extra endpoint
  patterns that STT does not. A separate faithful predicate or a shared lower
  IP-classification primitive is needed.
- Managed URL construction uses urljoin(origin-with-trailing-slash, "v1").
  It must retain an origin path, not just concatenate a host and /v1.

Next implementation steps:

1. Port raw selection parsing and the STT locality predicate against fixtures.
2. Reuse secret_scope for direct reads; port the authoritative-scope/pool gate
   and the voice-key override priority.
3. Port managed entitlement/token/URL dependencies and selected-provider errors.
4. Implement the lazy resolver against the 100 call-order fixtures, extend them
   for removed-provider and entitlement failures, and construct TranscriptionHttp
   with the resolved pair plus existing language configuration.
5. Connect the resulting backend to rich inbound attachment processing. The
   current HTTP/enrichment tests do not prove dispatcher construction.

The selection algorithm and HTTP client construction are now implemented;
the live credential effects and dispatcher construction remain incomplete.

Implemented follow-up: `tool_backend_selection.rs` reads raw config files and
parses selection without introducing schema defaults. 216 source-executed
comparisons cover STT, browser, web and TTS selection keys, value coercion and
legacy gateway intent. A real-file test verifies missing/malformed config,
explicit local selection, gateway override and non-mutation of saved YAML.
STT locality now shares only the Python IP classification primitives with
model discovery. 257 source cases verify its stricter URL parsing and private
address rules, including rejection of model discovery's CGNAT and bare-host
shortcuts.

`transcription_http::AudioCredentials::resolve` now consumes raw selection and
STT settings with lazy direct/managed effects. All 100 source cases check both
the returned pair/error and effect order. `from_openai_config` constructs the
transport with this pair and language settings; four real multipart uploads
exercise that constructor. Explicit provider failures do not invoke managed
fallback, and environment credentials retain the default destination rather
than a custom remote URL without its own config key.

The effect boundary supplies the final managed vendor URL and account-aware
unavailable note. Its concrete pool/entitlement implementation is next;
these tests do not establish live managed authentication or URL construction.
Credentials are currently typed strings; malformed non-string credential config
does not have Python exception parity. The source removed-backend registry is
currently empty, and future entries will require the shared error policy.

`tool_credentials.rs` now implements the shared voice-provider secret ladder:
explicit config, scoped secret, multiplex gate, scoped environment/file read,
then plain/custom provider pools. Dedicated voice overrides never use a pool.
`ProfileAudioCredentials` connects this lookup to the STT resolver. Pool peeking
and managed account operations remain caller-supplied effects; this does not
claim a native credential-pool implementation.

576 source-executed fixtures check returned secrets and read order, including
blank values, custom pools and pool errors. A real temporary .env plus installed
task scopes verifies file reads, authoritative scoped misses, voice/OpenAI
priority, custom-pool fallback and the STT resolver consumer. Secret scope's
existing shared test lock protects the process-wide multiplex flag.

One source qualification: `_scoped_credential` catches all scope-read exceptions
and falls back to the raw environment. This port retains that behavior for an
unscoped multiplex read. An installed scope's miss does not raise and remains
authoritative. Full runner integration must install the profile scope before
calling this helper. The helper therefore must not be described as providing
unscoped multiplex isolation on its own.

The persisted pool read layer now exists in `auth_store.rs`, with 324 Python
shadowing cases and 38 auth-store load cases exercised on temporary files.
See [auth-store verification](auth-store-verification.md). Native `load_pool`
hydration/seeding and `peek` availability rules remain necessary before this
storage reader can replace the pool callback.

Cooldown TTLs and reset/error parsing now have 609 Python comparisons in
`credential_pool.rs`. Entry hydration and the availability loop still need to
consume these policies before `peek` can back the STT callback. Remaining
timestamp qualifications are recorded in the auth-store verification document.
The shared ISO parser now covers calendar/week/compact forms and fractional
offsets against 1,572 CPython cases; timezone-aware cases also exercise the
credential deadline consumer.

Persisted rows now decode through `PooledCredential`, and its disk serializer
applies the borrowed-secret policy. 698 dataclass and 159 sanitizer comparisons
plus a real store-to-entry-to-cooldown test cover this boundary. Live-source
rehydration, seeding/pruning and availability selection remain before the STT
pool callback can use these rows.

The source upsert operation now rehydrates borrowed keys by fingerprint and
preserves their cooldown unless the token rotated. 220 source cases plus the
serialized-reference reload test verify this operation and its disk-change
signal. The pool loader still needs source discovery, suppression/pruning,
priority normalization and availability selection around this operation.

Stale-source pruning and Anthropic source-priority normalization now have 380
source comparisons. A real store/reload test checks ordinary environment-source
retention and explicit pruning. Source discovery, suppression and availability
selection remain the next parts of the pool loader.

Environment-source seeding now reads profile files/scope and suppression,
updates entries through the real upsert operation and returns active sources.
112 source seeding cases, 47 helper cases and a temporary-profile reload test
verify that path. Native auth-registry construction, singleton/custom sources,
provider-specific endpoint hooks and availability selection remain before the
complete pool loader can supply STT.

Custom-pool aliases and custom/model-config seeding now have 78 source cases
and a real YAML-to-runtime-entry test. The input contract is the compatible
provider-list view. Port its v12/legacy normalizer before general config can
construct this source path; singleton discovery and availability remain next.

The merged compatibility normalizer now exists and is consumed by custom
credential seeding and native custom request settings. 297 source cases plus
real YAML/HTTP consumers verify ordinary config forms. Named lookup now has
221 source comparisons, and bare-provider-slug request-body routing and provider
output-limit precedence pass native HTTP tests. Full named endpoint/key/transport resolution and remaining
URL/error qualifications are recorded in the auth-store verification document.
