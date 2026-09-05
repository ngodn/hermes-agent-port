# Local endpoint probe verification

`local_probe.rs` implements server detection, Ollama vision querying, and the
memory/disk caches used by those operations. It uses real async HTTP requests
and an explicitly supplied gateway home. Tests stay inline in this module.
The rich runner has not yet connected this component to image routing.

## Reference comparison

`gen_local_probe_goldens.py` executes the Python probe functions with recorded
HTTP responses and controlled cache boundaries. The 42 fixtures cover 17
detection scenarios, 11 vision responses, and 14 URL normalization cases.
Rust replays the HTTP scenarios against local servers and compares results,
request ordering, bearer headers, and the actual `/api/show` JSON payload.

Separate real HTTP tests cover disk reopening after the server stops and
negative verdicts staying out of disk storage. A raw TCP test returns truncated
HTTP 200 bodies to verify that failed reads do not identify LM Studio or cause
the legacy `/props` retry. Inline cache tests cover the different positive and
negative memory TTLs, host/port isolation, expiry, string timestamps, and
Python's aborted write when stale-entry pruning encounters an invalid timestamp.

Workspace validation: 1,037 tests passed, one existing bridge test ignored.
Clippy with warnings denied and formatting passed. Logs are
`takeover-local-probe-workspace-tests.log` and `takeover-local-probe-clippy.log`.

## Corrections made during integration

Claude supplied the component and its initial inline tests; Gemini reviewed the
Python source. Codex added the Python oracle and corrected the implementation:

- HTTP 200 detection predicates use Python membership semantics for JSON
  objects, lists, and strings, rather than requiring object keys.
- Redirects are disabled. Detection and show use separate clients with their
  respective two-second and three-second connect/read limits. A client build
  failure returns an unknown result rather than changing client policy.
- Body-read failures remain transport failures, including LM Studio's
  otherwise status-only detection.
- Python whitespace and timestamp coercion are preserved for covered cases.
- Cache writes use unique, exclusively created temporary files, owner-only
  permissions for new files, and cleanup on write/rename errors.

Timeout behavior was checked against
[HTTPX's phase-specific timeout documentation](https://www.python-httpx.org/advanced/timeouts/)
and [reqwest 0.12.28 ClientBuilder](https://docs.rs/reqwest/0.12.28/reqwest/struct.ClientBuilder.html).
HTTPX also defaults to
[not following redirects](https://www.python-httpx.org/quickstart/#redirection-and-history).

## Remaining dependencies and limits

The caller must still apply endpoint-locality eligibility and provider-profile
prefix stripping. This component accepts an already resolved bare model name;
it must not be called indiscriminately for arbitrary remote providers. Managed
runtime capability, models.dev caching/overrides, and the final session lookup
remain unfinished.

The HTTP tests use loopback servers. They do not prove behavior against every
deployed backend or reproduce a real dropped-SYN connect timeout. Cache tests
verify suppression/expiry; the transport distinguishes connect timeouts using
reqwest's error classification. HTTPX write/pool timeout phases and cookie or
compressed-response behavior are not fully matched by these clients yet.

Blackhole host keys currently use reqwest URL parsing. Unusual or malformed
URL spellings can differ from Python's urllib parser. The small cache writer
does not yet port all of utils.atomic_json_write's symlink, ownership, and
platform-specific replacement behavior. These remain part of the full port,
not claims covered by the passing probe fixtures.
