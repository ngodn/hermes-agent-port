# Native Auxiliary Compression Fallback Chain Resolution

## Outcome

Native full-compression summaries now honor the ordered
`auxiliary.compression.fallback_chain` configuration in production startup and
in the live summary request path. The route plan is resolved once with the
conversation's profile-scoped configuration and credentials, then held
immutably by the frozen native client.

Explicit auxiliary mode attempts its auxiliary route first, selects at most
one eligible configured fallback, then uses the main conversation model as its
safety route. Auto mode attempts the main model first, then selects at most one
eligible configured fallback. This matches the current Python runtime rather
than interpreting the word `chain` as permission to accumulate every
candidate's request timeout.

## Configuration and resolution

`compression_auxiliary.rs` owns the typed fallback entry and Python-compatible
coercion for provider, model, endpoint, direct or env-backed credentials,
transport alias, independent timeout, reasoning declaration, and output cap.
Invalid containers and entries are skipped in source order. A provider-only
entry remains representable but is skipped during client resolution because
the Python resolver requires a model before it can construct a candidate.

Startup resolves each structurally usable entry through the existing provider
profile and custom-provider configuration surfaces. Only native
`chat_completions` routes are admitted. Unsupported transports and unavailable
credentials are logged with redaction and skipped without preventing the main
conversation client from starting.

Per-entry timeouts accept only positive JSON numbers and do not inherit the
300-second compression floor. Missing or invalid entry timeouts retain the
task-level timeout, including that floor.

## Selection safety

The frozen plan applies the Python failure scope before choosing a candidate:

- HTTP 401 and 402 failures invalidate the provider credential surface, so
  sibling models on that provider are skipped.
- Timeouts, rate limits, unusable summaries, and other request failures
  invalidate only the exact provider and model deployment. A sibling model or
  a distinct explicit endpoint remains eligible.
- A primary route that could not be built seeds a credential-scoped failure,
  so the chain does not repeat the unavailable provider.
- Known candidate context windows below 64,000 tokens are skipped. Explicit
  model overrides and the durable models.dev cache are consulted without
  starting network work during synchronous native client construction.
- Unknown context windows remain eligible, matching Python.

Only the first eligible configured candidate is called. If that call fails or
returns an unusable truncated summary, explicit mode moves directly to the
main safety route. Later configured entries are not called during the same
summary attempt.

Each request is tool-free, independently timed, and recorded in the auxiliary
usage ledger. Route clients clear their own route plan before provider I/O, so
fallback recursion is impossible. Runtime and startup errors are redacted
before logging.

## Request controls and prompt caching

Fast-lane output and reasoning controls are certified independently for the
resolved fallback provider and model. A candidate cannot inherit the primary
route's non-reasoning declaration. A certified candidate receives the Python
canonical `{"enabled": false, "effort": "none"}` shape. If the task claims a
fast lane but the candidate does not certify it, a task-level `reasoning` key
is removed before the request.

The ordinary Python fallback request does forward the task-level
`effective_extra_body`, so Rust preserves that behavior. Python does not read
`fallback_chain[].extra_body` in this path, so the final Rust entry deliberately
does not expose a dead per-entry field.

The summary prompt is built once per compression attempt and reused byte for
byte across the selected routes. The route plan never changes the system
prompt, conversation tool schema, stored transcript, or earlier context. It
therefore preserves the per-conversation prompt-cache contract.

## Helper lanes and review disposition

The helper work was split by independent ownership:

- AGY audited the Python selection contract and generated the deterministic
  corpus.
- Claude implemented only the Rust configuration-model draft.
- A later serialized AGY run reviewed the integrated production path.
- The primary lane verified both reports against source, integrated the code,
  and owned all cross-file decisions and validation.

The adversarial review correctly found the uncertified reasoning leak,
multi-candidate timeout accumulation, failure-scope gap, context-window gap,
and startup log redaction gap. Those findings were fixed.

Two review findings were rejected after line-level source checks. Python passes
task-level `effective_extra_body` to `_call_fallback_candidate_sync`, so keeping
task request extensions is correct. Python does not consume the entry's own
`extra_body`, so adding that field would be speculative. The checked-in helper
reports are retained as review evidence, with the final disposition recorded
here.

## Validation

- The source-executed fallback corpus regenerates byte for byte.
- Rust tests consume all six entry, timeout, API-mode, credential,
  failure-scope, and traversal sections of that corpus.
- Live local HTTP tests cover explicit and auto ordering, independent request
  controls, exact-route skipping, credential-surface skipping, the 64k floor,
  one-candidate boundedness, recursion prevention, and main-route fallback.
- Selected Python fallback, fast-lane, budget, and stall tests pass.
- Full workspace tests, formatting, Clippy with warnings denied, Ruff, and diff
  hygiene are recorded in `PORT.md` after the final validation run.

## Remaining related work

This checkpoint does not claim the top-level main `fallback_providers` chain,
built-in auxiliary provider discovery, credential-pool rotation, OAuth refresh,
non-chat auxiliary transports, or the stall-fence route pinning lifecycle.
Those remain separate runtime checkpoints. Dynamic models.dev network refresh
continues to happen in the existing asynchronous conversation setup; fallback
construction only reads its durable snapshot.
