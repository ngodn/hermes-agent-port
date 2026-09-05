# Cloud registry cache verification

`models_dev.rs` ports the cache and refresh layer of `agent/models_dev.py`.
Callers share an `Arc<ModelsDev>` scoped to the resolved Hermes home. The registry
is an immutable snapshot; it is not yet connected to the complete vision lookup.

## Implemented behavior

- Memory and disk precede network. Fresh disk data retains its original age.
- Stale data returns immediately with five minutes of memory grace while a single
  background worker refreshes. Offline requests never start that worker.
- Cold fetches share one network request. Force bypasses freshness and failure
  backoff; `allow_network=false` takes precedence over force.
- Requests send If-None-Match only with a servable registry. Cold forced refreshes
  first hydrate from disk. A 304 refreshes memory freshness without touching the
  disk body or mtime. A cacheless 304 clears the sidecar and arms backoff.
- Invalid or unreadable disk data is quarantined and its ETag cleared. Invalid
  network data leaves the previous registry intact. Failed requests back off for
  five minutes; successful requests clear backoff.
- `models_dev.url` is read from the supplied configuration. Connect/read timeouts
  are five and ten seconds. Body and ETag use the shared atomic file writer,
  also used by local endpoint caches. The source's behavior of retaining a prior
  ETag when a response omits one is preserved.

## Evidence

`gen_cloud_catalog_goldens.py --check` executes the Python cache functions with
real temporary cache files, controlled HTTP responses, and a queued background
worker. All 60 combinations of missing/fresh/stale/future/corrupt disk data,
force, network permission, and HTTP 200/304/503 match Rust's returned data,
final data, request headers, backoff, quarantine, and ETag.

Eleven inline Rust tests exercise real local HTTP and filesystem paths. They
also cover concurrent cold requests, readable stale data during blocked I/O,
invalid network JSON, and a forced success clearing backoff after a background
failure. The full workspace passes 1,064 tests with one existing ignored bridge
test. Formatting, Clippy with warnings denied, and diff whitespace checks pass.
Logs: `cloud-catalog-workspace-tests.log`, `cloud-catalog-clippy.log`.
Gemini's source audit is `cloud-catalog-source-review.md`; executable reference
cases and checked Python code are authoritative.

## Remaining scope and limits

Provider mappings, model overrides, capability/context extraction, and the vision
catalog stage are now implemented; see [metadata verification](cloud-metadata-verification.md).
The complete live capability lookup remains next. Runtime construction must share the cache instance;
configuration changes currently require constructing it with updated settings.

This does not claim exact transport equivalence between requests and reqwest
for redirects, proxy environments, decompression, or all timeout failures. Disk
replacement preserves existing permissions and creates private temporary files;
full cross-platform parity with Python's atomic utilities remains unverified.

The Rust fetch mutex spans error-state updates as well as HTTP and successful
commits. Python's background exception handler releases and reacquires that lock;
Rust intentionally avoids an intervening successful forced refresh being followed
by stale failure state. Disk hydration also avoids replacing memory data committed
concurrently. Tokio shutdown cancels background work rather than emulating Python
thread lifecycle. No external models.dev request was needed for validation.
