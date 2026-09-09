# Adversarial review: main-provider static API-key pool checkpoint

Scope reviewed: the working-tree diff on `rust-rewrite` for
`rust/crates/hermes-gateway/src/{native_agent.rs, main.rs, credential_pool.rs, custom_provider_config.rs}`,
cross-checked against the live Python oracle (`agent/error_classifier.py`,
`agent/agent_runtime_helpers.py`, `agent/credential_pool.py`,
`hermes_cli/runtime_provider.py`), the contract lane
(`rust/analysis/main-provider-pool-contract-agy.md`), and the golden corpus
(`rust/tools/main-provider-pool-goldens.json`, 73 cases, and its generator
`rust/tools/gen_main_provider_pool_goldens.py`). I read the current working tree,
not the stale git snapshot. I edited only this file.

## Primary disposition

The review below records the implementation as Claude inspected it. The
primary lane fixed F1 by skipping the same-key retry for Python's exact
usage-limit context signals, then added streaming and tool-loop HTTP coverage.
It fixed F2 with replace-on-merge header semantics and a duplicate-value test.
It corrected F3 in the contract. The primary lane also added 13 source-executed
raw HTTP classifier cases, bringing the current corpus to 86 cases across 11
sections. Those generated expectations are consumed directly by the Rust
classifier test.

The wiring matches the seam design: `MainPoolCredential` holds a `PoolLocator`, a
shared `Arc<Mutex<ActiveMainCredential>>` cursor, a `fallback_base_url`, and a
route-header projection; both transports funnel through one private
`send_main_request` that classifies pre-body status, rotates once per entry inside
a bounded budget, persists before retry, and re-sends byte-identical bodies. Most
of the invariants hold. Two defects are worth fixing before this ships, one of
them a header-handling regression that reaches the non-pool path too, plus one
documentation mismatch.

---

## Findings

### F1. Medium. Rust reimplements the error classifier and misses the usage-limit fast-path, so a usage-limit 429 that carries a reset window burns a doomed retry instead of rotating.

Files: `rust/crates/hermes-gateway/src/native_agent.rs:1022` (`main_pool_failure`),
`native_agent.rs:1140-1141` (429 arm), `native_agent.rs:1571-1592`
(`send_main_request` RateLimit branch).

Two linked problems.

First, the structural gap. The golden corpus never exercises the Rust classifier.
Every retry-transition and cooldown case in the generator passes
`classified_reason=FailoverReason.X` and a prebuilt `error_context` straight into
`recover_with_credential_pool` (see `gen_main_provider_pool_goldens.py:339, 480,
502, 643, 654, 667, 679, 694, 709`). The Python runtime derives that reason from a
2263-line classifier (`agent/error_classifier.py::classify_api_error`). The Rust
runtime derives it from `main_pool_failure`, roughly 120 lines of substring
markers. Nothing in the golden set feeds a raw `(status, body, headers)` tuple
through `main_pool_failure`, so its agreement with `classify_api_error` is asserted
only by the author's own 9-case unit test at `native_agent.rs:3746`. This is the
single highest-risk surface in the checkpoint, because a classification slip turns
directly into rotating a healthy key, benching a live credential, or failing a turn
that Python would have recovered.

Second, a concrete divergence that falls out of that gap. Python's rate-limit
recovery has a usage-limit fast-path: even under `FailoverReason.rate_limit`, if the
error context reason/message names a usage limit it rotates immediately with no
same-key retry (`agent_runtime_helpers.py:1303-1314`, and the golden
`usage_limit_reached_immediate_rotation` at
`gen_main_provider_pool_goldens.py:679`). The Rust RateLimit branch has no such
check. It always grants the one same-key retry unless the store already shows the
key exhausted (`native_agent.rs:1571-1592`).

Failing scenario: provider returns HTTP 429 with body
`{"error":{"message":"Usage limit reached. Try again in 1 hour."}}` (or the same
body plus a `Retry-After` header). Python: `classify_api_error` sees a usage-limit
pattern plus a transient signal (`error_classifier.py:1407-1441`,
`_USAGE_LIMIT_TRANSIENT_SIGNALS` includes `"try again"`), classifies `rate_limit`,
then `recover_with_credential_pool` matches `"usage limit reached"` in the context
message and rotates on attempt 1 (one request to the failed key). Rust:
`main_pool_failure` classifies 429 with `transient == true` as `RateLimit`
(`native_agent.rs:1140` requires `!transient` for the Billing arm, so it falls to
`:1141`), then `send_main_request` re-sends the identical body to the same
just-rejected key before rotating (two requests to the failed key), delaying
failover by one round trip and one wasted call against a credential the provider
already said is over quota.

Smallest safe fix: in the RateLimit branch at `native_agent.rs:1571`, before the
same-key retry, compute the usage-limit signal from `main_error_context` (reason or
message containing `usage_limit_reached`, `gousagelimit`, `usage limit reached`, or
`usage limit has been reached`) and skip the retry (fall through to rotate) when it
is present, mirroring `agent_runtime_helpers.py:1303-1314`. Separately, given the
classifier is unverified, add golden coverage that drives raw `(status, body,
headers)` through `main_pool_failure` and compares to `classify_api_error`, or at
minimum expand the unit test to the billing/usage/upstream boundary bodies the
Python patterns enumerate.

### F2. Medium. Header application switched from insert to extend, so a route or custom header that shares a name with a profile default is now sent twice instead of overriding, leaking the stale default to the endpoint.

Files: `rust/crates/hermes-gateway/src/native_agent.rs:1399-1400`
(`with_provider_profile`), `native_agent.rs:1408` (`with_extra_headers`),
`native_agent.rs:1514` (`headers_for_main_route`).

The checkpoint changed `with_extra_headers` from a per-entry
`self.provider_headers.insert(name, value)` loop to
`self.provider_headers.extend(parse_header_map(headers)?)`, changed
`with_provider_profile` to `self.provider_headers.extend(...)`, and builds the
per-route header set with `headers.extend(extra.clone())`. In `http` 1.5.0 (the
pinned version, `rust/Cargo.lock`), `impl Extend<(HeaderName, T)> for HeaderMap`
calls `self.append(k, v)` for each pair
(`http-1.5.0/src/header/map.rs:2201-2224`), which adds an additional value for an
existing name rather than replacing it. The old `insert` replaced. So when a header
name appears in both a profile default and a later `with_extra_headers` call (or in
both the profile defaults and a route's `extra_headers` inside
`headers_for_main_route`), the wire request now carries both values.

Failing scenario: a base profile declares `default_headers` including `X-Title`,
and the user's `custom_providers` entry for the same base URL sets
`extra_headers: {"X-Title": "My Title"}` intending to override it. Old behavior:
the request carries `X-Title: My Title`. New behavior: the request carries both
`X-Title: <profile default>` and `X-Title: My Title`. The value the operator meant
to suppress still reaches the endpoint, and duplicate-header handling is
provider-dependent (comma-join, first-wins, or reject). The same doubling hits the
hardcoded OpenRouter attribution headers at `main.rs:903-912` if a profile or
custom entry names `HTTP-Referer` or `X-Title`. This affects both the pool path
(via `headers_for_main_route`) and the non-pool main path (via
`provider_headers`), so it is not confined to the new feature.

This is not caught by the new test: it configures `custom_providers` with distinct
`X-Route-Token` values and no overlapping profile defaults, so append and insert
produce the same single-value map.

Smallest safe fix: keep the fields separate as the refactor intends, but restore
replace semantics on merge. Replace the three `.extend(...)` calls with a small
helper that inserts each pair (`for (k, v) in map { target.insert(k, v); }`), so a
later source overrides an earlier one exactly as `insert` did before.

### F3. Low. Contract prose contradicts its own golden on explicit base URL; the Rust code follows the golden, so this is a documentation defect, not a code bug.

Files: `rust/analysis/main-provider-pool-contract-agy.md:49` and `:74` and `:77`;
golden `explicit_base_url_with_provider_default` in
`rust/tools/main-provider-pool-goldens.json`; `main.rs:1068`.

The contract prose (section 2.1 item 1b and the section 2.3 bullet) states that an
explicit base URL supplied without an explicit API key "overrides the endpoint
while allowing the API key to be resolved from the credential pool or environment"
and "uses explicit URL and pool credential." The executable golden for that exact
case shows the opposite: `{"has_pool": false, "source": "explicit", "api_key":
"sk-ds-env-key"}`, that is, an explicit base URL disables the pool and resolves the
key from the environment. The Rust gate `explicit_runtime = config.llm_api_key.is_some()
|| config.llm_base_url.is_some()` (`main.rs:1068`) turns the pool off whenever a base
URL is set, which matches the golden. No code change is needed. The prose should be
corrected to match the executable evidence (I did not edit it, per the task
constraints).

---

## Areas checked with no defect found

- Startup precedence and provider gate. `main.rs:1055-1122` resolves in the order
  explicit key, then pool runtime, then profile/env key, with `base_url` taken from
  the selected entry and falling back to `fallback_base_url`. `pool_supported`
  excludes `""`, `auto`, `custom`, `anthropic`, `openai-codex`, `xai-oauth`, `nous`,
  and `custom:*` (`main.rs:1071-1074`), matching the `SEEDING_REQUIRED` boundary at
  `credential_pool.rs:828`. The disabled-provider fail-fast at `main.rs:1005-1015`
  produces an error containing "disabled", satisfying `disabled_provider_fails_fast`.
  The 13 startup goldens describe `resolve_runtime_provider` outcomes and the Rust
  build resolution mirrors them, including `active_pool_selected_over_env` (source
  from the entry), per-entry base URL override, and empty/exhausted pool falling back
  to env.

- Exact failed-key attribution. `send_main_request` reads the dispatched route and
  passes both `route.api_key` and `route.credential_id` into
  `mark_exhausted_and_rotate` (`native_agent.rs:1611-1625`), never `pool.current()`.
  This is the #79156 wire-key-wins discipline; the failed entry is identified by the
  key actually sent, and `install_replacement` compare-and-swaps on both id and key
  (`native_agent.rs:1467-1479`) so a concurrent rotation cannot clobber a different
  active credential.

- Persistence before retry. Both `credential_is_exhausted` and
  `mark_exhausted_and_rotate` run inside `tokio::task::spawn_blocking` and complete,
  including the `check_write` after `drop(pool)`, before the loop issues the retry
  (`credential_pool.rs:1006-1024`, `native_agent.rs:1573-1626`). The new test asserts
  `last_status == "exhausted"` is on disk when the second endpoint first sees a
  request, which pins persist-before-retry for the 401 and 429 cases.

- Ordinary versus pre-exhausted 429 request counts. Ordinary 429: attempt 1 retries
  the same key (`retried_429.insert` returns true, `continue`), attempt 2 rotates
  (`native_agent.rs:1589-1592`). Pre-exhausted 429: `credential_is_exhausted` returns
  true, the retry is skipped, and it rotates immediately
  (`native_agent.rs:1571-1592`, `credential_pool.rs:1006-1024`). The test's
  `expected_first == 2` for the 429 case and `1` for 401 confirms the counts on the
  happy provider. This matches `ordinary_rate_limit_attempt_1/2` and
  `pre_exhausted_rate_limit_immediate_rotation`.

- Bounded full-pool traversal. The per-entry `attempts` map caps any single
  credential id at two sends (`native_agent.rs:1543-1556`), and the post-rotation
  guard rejects a replacement equal to the failed entry
  (`native_agent.rs:1622-1624`); a drained pool returns `None` and surfaces the
  original error (`native_agent.rs:1620-1621`). There is no inner loop that can drain
  the pool in one request, and rotation reloads the store each time so an
  already-exhausted sibling is skipped by selection.

- Auth, billing, upstream classification structure. Setting aside F1, the 429 arm
  ordering (overload to Unrelated, aggregator to UpstreamRateLimit, usage/billing
  without transient to Billing, else RateLimit) mirrors `error_classifier.py:1364-1441`,
  and `_classify_402`'s `usage_limit && transient => rate_limit` else billing is
  reproduced at `native_agent.rs:1137-1138`. Entitlement 403s (grok, org-not-allowed)
  route to Unrelated so the pool is not mutated, matching
  `agent_runtime_helpers.py:1327-1383`; the OAuth providers those checks target are
  excluded from `pool_supported` anyway.

- Clone and concurrent-turn behavior. The cursor is a shared
  `Arc<Mutex<ActiveMainCredential>>`; `NativeAgentClient::clone` shares it, so a
  rotation in one tool round or turn is visible to the next. Turns within a session
  are serialized by the turn lease, and `install_replacement`'s compare-and-swap
  tolerates the cross-conversation store race (last-writer-wins on an idempotent
  exhaustion mark).

- Cancellation and blocking I/O. No lock is held across an await: the `active` mutex
  is taken only inside the synchronous `route()` and `install_replacement()`, and the
  store I/O runs on `spawn_blocking`. A drop after persist but before retry leaves the
  store consistent (failed key exhausted, cursor advanced), and a mid-stream drop
  after a 200 is not seen by `send_main_request` at all.

- Fresh clients and per-entry endpoint changes. Every rotation builds a new
  `reqwest::Client` (`fresh_main_http_client`, `native_agent.rs:1461`) and the request
  URL is derived from the resolved route each iteration (`native_agent.rs:1558`), so a
  per-entry `base_url` takes effect on the retry.

- Header route isolation. `headers_for_main_route` recomputes from
  `provider_default_headers` plus only the custom-provider `extra_headers` whose
  `route_identity` matches the resolved base URL (`native_agent.rs:1503-1516`), so a
  rotation to a different endpoint drops the prior route's proxy headers. This is the
  correct anti-leak behavior; the only header defect is the append-versus-replace
  regression in F2, which is orthogonal.

- Stable bodies and prompts, tool-round carryover. The body is built once by the
  caller and passed by reference to `send_main_request`, which re-sends the identical
  serialized bytes on the retry (`native_agent.rs:3151-3160` streaming,
  `:3659-3681` step). The test asserts the first-endpoint body equals the
  second-endpoint body and that `tools` and the first message are identical across the
  tool retry, guarding against prompt rebuild or tool reordering.

- Streaming pre-body only. Recovery fires solely on the pre-body HTTP status;
  `send_main_request` returns a success-status `Response` and `forward_sse` consumes
  the stream afterward, so a mid-stream failure after a 200 cannot trigger a rotation
  and cannot double-emit text.

- Final error propagation. When the pool is `None`, when the failure is Unrelated or
  UpstreamRateLimit, or when the pool drains, `send_main_request` returns
  `main_http_error(label, status, &text)` with the same "native agent [step] HTTP
  {status}: ..." shape and 300-char truncation as the pre-checkpoint inline errors
  (`native_agent.rs:1008-1015`), so the caller-visible error is unchanged.

- Deferred OAuth, non-chat, and fallback boundary. `pool_supported` structurally
  excludes the OAuth and custom providers, the seam is `/chat/completions` only, and
  recovery rotates keys of the already-resolved provider without switching providers
  or activating a fallback chain. This matches the stated out-of-scope boundary.

---

## Prose versus executable-evidence mismatches

- F3 above: contract 2.1(1b) and 2.3 claim explicit base URL keeps the pool; the
  golden and the code disable it.

- Contract 5.1 lists "Usage Limit Reached" as immediate rotation on attempt 1. The
  Rust code honors this only when the body has no transient signal (Billing arm).
  With a transient signal present it retries once first (F1). The golden that would
  have caught this injects `classified_reason` and `error_context` directly, so it
  passes against Python but does not constrain the Rust classifier.

- The seam doc's section 4 open item (whether a pooled entry may name a foreign host)
  is still open in the code: `headers_for_main_route` sends `provider_default_headers`
  to whatever `base_url` the entry carries, with no same-host gate. This is benign if
  the contract lane guarantees pool entries are same-provider regional mirrors, which
  the current goldens assume; it is a latent leak if that assumption is ever relaxed.
  Not a defect today, flagged for the record.

---

## Prioritized verdict

1. F1 (Medium): add the usage-limit immediate-rotation check to the RateLimit branch,
   and close the classifier verification gap (raw-body goldens or an expanded unit
   test). This is the load-bearing correctness and credential-safety surface and it is
   currently unverified against the Python oracle.
2. F2 (Medium): restore replace-on-merge header semantics; the insert-to-extend switch
   silently duplicates any overriding header and leaks the stale value, on both the
   pool and non-pool main paths.
3. F3 (Low): fix the contract prose to match the explicit-base-URL golden. No code
   change.

Everything else in the checkpoint (attribution, persist-before-retry, bounded
traversal, cursor sharing, cancellation safety, fresh clients, byte stability,
pre-body-only streaming recovery, and the deferred boundary) holds up against the
Python sources and the golden corpus. No em dash characters were used in this
review.
