# Native auth-store read verification

`auth_store.rs` ports `_load_auth_store` and `read_credential_pool` from
`hermes_cli/auth.py`. File paths are explicit so the future pool loader can
choose the active profile and optional global root without changing process
environment. No real user credentials were read or modified during validation.

The reader accepts UTF-8 BOM, preserves recognized store fields, converts the
legacy `systems.nous_portal` shape, and migrates the historical Nous portal host
in memory. Missing stores return the version-1 empty shape. Read failures
propagate; malformed JSON/UTF-8 returns empty state after a best-effort copy to
`.json.corrupt`. The original is untouched, and the warning records whether
preservation succeeded without logging content or credentials.

Pool reads use nonempty profile lists as authoritative per-provider slices.
Only missing, empty or malformed slices fall back to root rows. Global read
failures are tolerated, while profile read failures propagate. Reading all
providers preserves the source's handling of malformed entries and does not
filter, select or merge individual credential rows.

`gen_auth_pool_read_goldens.py` executes the actual Python functions. Its 324
pool fixtures cover malformed/missing sections, empty/nonempty slices, custom
providers, unknown providers and all-provider reads. Its 38 file fixtures cover
normalization, historical shape migration, stale/current URLs, field retention,
invalid field types and BOM. Inline Rust tests read real temporary files,
compare those results, check root/profile shadowing, preserve corrupt bytes,
verify originals remain unchanged and distinguish directory-read failure.

Remaining work: automatic profile/root path selection, root mtime caching,
credential entry decoding and hydration, seeding/pruning, availability/cooldown
selection, locked writes, refresh, and the STT pool callback consumer. The
corrupt copy preserves bytes and file permissions through `std::fs::copy`;
Python `copy2` additionally preserves timestamps and platform metadata.
Malformed portal values report native errors rather than exact Python exception
text. Nonstandard JSON numeric constants are not part of this fixture corpus.

Do not implement the STT pool callback by picking the first stored token.
`load_pool` seeds and sanitizes entries, and `peek` applies availability rules,
including dead credentials and exhaustion cooldown. Those contracts still need
to be ported before persisted rows can supply live STT credentials.

Cooldown policy is now implemented in `credential_pool.rs`: HTTP/billing TTLs,
explicit reset precedence, numeric/ISO deadline parsing, vendor retry-delay
extraction and error-context normalization. 609 source-executed cases verify
the five Python functions, including sole-credential behavior, billing versus
unverified billing, numeric-string/nonpositive-number differences, milliseconds,
Unicode digits, delay-pattern priority and error-field precedence.

This is the availability policy dependency, not a working credential pool.
`exhausted_until` expects hydrated numeric last_status_at, matching the Python
entry type. Full entry decoding, source synchronization, dead-entry pruning,
selection and persistence remain. Nonfinite numeric timestamps, exotic Unicode
case-fold matches and oversized integer delays are outside this corpus.

The shared gateway ISO parser now handles extended/compact calendar dates,
ISO week dates with optional weekday, arbitrary single-character separators,
compact/colon times, comma/dot fractions and signed offsets including fractional
seconds. It follows CPython's week-date separator disambiguation and zero-offset
fraction behavior. 1,572 CPython cases compare accepted values and rejections;
timezone-qualified and invalid forms also go through the actual credential
deadline parser. Naive fixtures use an explicit UTC interpretation to keep
results portable across developer and CI timezones. The generator calls the
installed CPython 3.12 implementation, not a reimplementation of its grammar.

Local-time DST folds/gaps remain a limitation: the existing gateway conversion
requires a unique local timestamp, whereas Python resolves these with fold
semantics. Nonfinite normalization and extreme-year floating-point rounding
also need dedicated coverage. These qualifications do not affect the tested
timezone-qualified provider deadlines.

`PooledCredential` now decodes the reference's declared fields and flattened
extra metadata, normalizes string status timestamps and Anthropic OAuth token
types, supplies defaults only for absent fields, and retains explicit nulls.
New IDs use six CSPRNG-generated hex characters; saved IDs are retained.
Serialization preserves the six nullable status fields and strips borrowed
secret fields through the shared `credential_persistence` module. Runtime key
and base URL selection preserve the separate Nous rules; the NAS validity check
is an explicit callback until the Nous validator is ported.

698 executable dataclass cases compare decoding/serialization and runtime key
and URL selection. 159 cases execute the full disk sanitizer, covering owned
OAuth sources, future external sources, manual keys, camel-case/dotted/hyphenated
secret names, safe metadata and fingerprint priority/coercion. All fixture
credentials are synthetic. Nous fixture validity is supplied by a deterministic
predicate, so this proves selection order, not NAS JWT validation.

`read_stored_entries` connects the auth-store reader to decoding. A temporary
file test proves stored identity retention, live-secret retention, borrowed
serialization/reload behavior, status timestamp conversion and cooldown
consumption without changing the source file. Rehydrating a sanitized reference
from its live external source, sorting/selecting available entries, and pool
write/refresh behavior remain pending. Strings containing unpaired Python
surrogates and nonfinite timestamp serialization require additional parity work.

`upsert_entry` now implements source refresh and borrowed-key rehydration.
It keeps the first source row, removes later duplicates, preserves saved IDs,
priorities and nonempty labels, merges field/extra updates, and clears the six
status fields only when the access token changes. Empty borrowed runtime keys
compare the incoming fingerprint with the persisted fingerprint, so restoring
the same secret does not resurrect an exhausted or DEAD credential.

220 cases execute Python's actual `_upsert_entry`, comparing resulting disk
rows, runtime keys and the disk-change flag. Cases cover status and token
combinations, duplicate sources, unrelated entries, new-entry defaults, extra
replacement/merge precedence and Anthropic token normalization. The file-based
entry test now reloads a sanitized reference, restores its original token with
no disk change, and rotates it while preserving identity and clearing cooldown.
Fields and extra metadata are stored separately to preserve dataclass update
semantics, including extra metadata overriding serialization fields.

This ports the update operation, not source discovery. Environment/singleton
reads, suppression gates, stale-source pruning, source-priority normalization,
availability selection and locked persistence remain. New-entry priorities
currently require bounded integers; Python's malformed/non-integer priority
behavior and cross-type scalar equality need further coverage.

Stale-source pruning and Anthropic priority normalization are now implemented.
Ordinary loads can retain absent `env:*` references, while explicit pruning can
remove them. Active sources and manual entries survive; other sources follow
the reference's ownership policy and `hermes_pkce` exception. Anthropic manual
entries lead seeded sources, whose priorities follow the source rank, previous
priority and label. The operation updates priorities without reordering rows,
including Python's last-index behavior for duplicate IDs.

380 source-executed cases compare change flags and serialized entries across
providers, source types, active/absent sources, explicit environment pruning,
input order, priority ties and duplicate IDs. The real store/reload test now
verifies that ordinary pruning retains the rehydrated environment reference,
explicit pruning removes it, and the manually stored key remains. The auth
file is unchanged throughout these in-memory operations.

Next pool-loader dependencies are source discovery, suppression gates and
availability selection, followed by locked persistence. Priority normalization
currently expects integer priorities and string labels/sources; malformed
cross-type sorting behavior is not covered by this corpus.

Environment seeding now lives in `credential_sources.rs`. It skips Copilot's
raw token path, gates registry entries on API-key auth, uses Anthropic's three
source variables in source order, honors profile-local suppression before URL
resolution, and feeds live tokens/provenance into fingerprint-aware upsert.
OpenRouter keeps its fixed endpoint and once-per-process ingestion warning.
Kimi and Z.AI endpoint behavior is an explicit provider callback, not yet a
native implementation of those discovery/probing hooks.

`ProfileEnvSource` reads real profile .env/auth files and the current secret
scope. File values take precedence, except unresolved op:// references use a
nonempty resolved scoped value. Scoped read errors propagate. This is the
reference's seeding policy, distinct from the voice-tool direct-key ladder.
Suppression reads stay profile-local and use Python membership semantics for
list, map and string markers; read failures return false like the source.

112 seeding fixtures execute the source function with recorded environment,
suppression, provenance and endpoint-hook calls. They compare active sources,
sanitized entries and disk-change flags; fresh random IDs are omitted from the
comparison. 47 additional source cases cover file/scope precedence and
suppression membership. A real temporary-profile test verifies suppression,
file-over-scope lookup, sanitized store/reload, same-key hydration without disk
churn and op:// resolution through the installed scope.

Remaining: provider auth-registry construction for the seeder, singleton/custom
source discovery, external-secret provenance collection, Kimi/Z.AI hooks,
availability selection, and a complete locked pool loader. File reads currently
use the existing native .env parser rather than Python's mtime cache; malformed
file-encoding parity remains qualified by that parser. No live user credential
or external secret-manager account was accessed in these tests.

Custom pool matching and seeding now consume the compatible provider-list view.
Name/slug/legacy-alias matches take precedence over URL fallback, and candidate
keys keep the durable slug before the legacy `custom:<name>` key. Custom entry
keys and matching model-config keys each honor suppression and feed the same
upsert operation; model fallback accepts either candidate pool identity and
retains the source's best-effort error behavior.

78 source-executed cases compare candidate keys, seeded disk rows, active
sources, change flags and suppression calls. They cover shared URLs, explicit
names that disambiguate providers, legacy aliases, API-key fallback, unknown
pools and suppressed sources. The temporary-profile test now reads a real YAML
configuration, seeds both custom-entry and model credentials, verifies their
runtime values and sanitized serialization, and checks that the YAML is not
modified.

The merged v12 `providers`/legacy `custom_providers` compatibility normalizer
still needs porting and must be supplied before this path consumes general
configuration. Current file proof uses the legacy list shape directly. The
remaining singleton sources and pool availability/persistence flow are also
still incomplete; these helpers do not yet construct the live STT pool.

`custom_provider_config::compatible` now builds the merged provider view without
mutating source configuration. It normalizes keyed and legacy entries, URL and
camel-case aliases, transport aliases, model lists/catalog markers, boolean
capabilities, limits, TLS settings and extra headers. Legacy entries retain
precedence when identities collide; malformed legacy-list shape and disabled
keyed-provider behavior match the tested source cases.

297 source-executed configurations compare the normalized result and verify
input non-mutation. `seed_custom_from_config` consumes this view, with real YAML
tests for both legacy and keyed/camel-case settings. Native startup also uses
the view for custom extra-body selection; the HTTP regression covers legacy and
keyed configurations on streaming and tool-enabled requests. These HTTP cases
now use the bare `lab` provider identity through named-provider lookup.

Normalization limitations: warning deduplication/text is not ported, and URL
acceptance currently checks template or scheme/netloc shape without Python's
full malformed-authority/NFKC exception behavior. Header fixtures use JSON
string keys; non-string YAML mapping-key coercion requires validation at the
config parser boundary. The complete STT pool loader remains pending.

## Named runtime lookup

`custom_provider_config::named` follows the Python getter's keyed-first lookup,
canonical built-in protection, saved-name/slug aliases, disabled-entry filtering,
environment-key precedence and runtime metadata projection. The caller supplies
built-in canonicalization and credential reads. Unlike the compatibility view,
the getter resolves environment references before matching each keyed entry and
does not resolve legacy entries' environment references at this stage.

221 source-executed cases compare results and credential-read order. These cover
canonical-name protection versus built-in aliases, explicit custom prefixes,
keyed/legacy forms, disabled providers and false-valued environment aliases.
Output-limit cases cover both field spellings, precedence, booleans, nonpositive
values and numeric strings. The legacy compatibility normalizer drops these
fields in the Python reference; the Rust getter preserves that behavior.
Four additional compatibility cases verify empty/whitespace provider keys.
Native startup consumes the named entry's nonempty extra body before the older
model/URL selector. Four local HTTP requests exercise streaming and tool modes
with keyed and legacy configuration, including the previously failing bare slug.

This does not yet construct the full named runtime: endpoint, model, credentials,
credential pools, command-issued tokens, transport, custom headers and model
capabilities still need their runtime consumers. Startup's canonical guard uses
registered native profiles, not the complete Python auth-provider registry. The
HTTP tests supply an explicit endpoint and key; they prove request-body routing.
Malformed non-string endpoint exception parity remains unimplemented.

Native startup now passes the named provider's `max_output_tokens` into the
existing gateway output-limit resolver. Twelve local HTTP requests verify both
streaming and tool-enabled startup with a provider default, explicit global
limit, explicit zero, environment override, invalid environment value and a
numeric-string global value. This preserves the Python gateway's precedence:
an integer global/environment choice wins, then a positive provider default,
before the later agent-level string coercion. No prompt or history mutation is
needed to configure this request limit.

Header integration is now connected. `custom_provider_config::extra_headers`
uses the effective URL and merged provider view, matching the Python client's
`normalize_route_base_url` selection instead of relying solely on a saved name.
301 source route identities and 32 header selections cover host/default-port
normalization, path and query distinctions, malformed URLs and IPv6 zones.
The native client applies selected headers after profile defaults on streaming
and tool requests. Four HTTP cases verify custom headers and Authorization
override; two verify that an explicit different endpoint retains its own bearer
credential and receives no headers from the named entry's old route.

The route parser reuses `local_probe::urlparse_hostname`, with its documented
Python URL parsing boundary. Exhaustive malformed-authority equivalence is not
proven by this corpus. Header values are validated without including them in
errors. Model switching, complete provider TLS configuration and additional
transports remain separate runtime work.

Validation: 1,238 workspace tests passed (one core and 1,237 gateway), two ignored
by default. Clippy with warnings denied, formatting, fixture regeneration and
diff whitespace checks passed. Logs: `/tmp/hermes-handoff-workspace.log`
and `/tmp/hermes-handoff-clippy.log`.
