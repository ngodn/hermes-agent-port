# Managed catalog loading and refresh

Implemented in `managed_catalog.rs`, with inline tests. `ManagedCapabilities::from_packaged`
uses the shared process catalog; explicit refresh methods update the catalog used by
subsequent capability checks. Existing callers can still supply a preloaded catalog.
The curated JSON is embedded from `rust/tools/managed-catalog.json`, so the Rust
binary does not need the Python tree at runtime.

## Evidence

- `gen_managed_catalog_goldens.py --check` executes the reference dataclasses and
  loader extracted from `hermes_cli/local_runtime/catalog.py`. It checks 43 inputs
  and verifies the embedded asset against the Python packaged catalog.
- The inline loader test compares all projected fields and constructor defaults,
  unknown-field removal, rejection cases, numeric strings with Unicode digits and
  underscores, Python string/bool coercion, and dictionary construction.
- Real local HTTP tests check the User-Agent, successful replacement, identical
  payload success, forced refresh, six-hour throttling, expiry, and failure retention.
- A blocked HTTP response proves background refresh leaves the old snapshot readable
  and suppresses another ordinary fetch while the first request is outstanding.
- A filesystem integration test resolves a real packaged model's projector capability
  before and after creating its asset in a temporary Hermes root.

Refresh keeps successful data in memory only. Attempt reservation precedes I/O and
also throttles failed attempts. Callers explicitly schedule background refresh;
vision checks do not unexpectedly start catalog downloads.

## Remaining scope

This is the curated managed-runtime catalog, not the cloud `models.dev` catalog.
Hardware recommendation, downloads, supervisor lifecycle, provider discovery, and
full runner integration remain separate port work.

Known compatibility limits: JSON integers have Rust/serde bounds rather than Python
arbitrary precision, non-finite floats are rejected, dictionary pair keys must be
strings, and malformed collection fields require arrays rather than arbitrary
Python iterables. The first non-forced refresh is eligible immediately; Python's
zero initial monotonic timestamp can suppress it during the machine's first six
hours of uptime. Redirect and timeout behavior uses reqwest rather than urllib and
has not been exhaustively matched across all transport failures.

Validation logs: `takeover-managed-catalog-workspace-tests.log`,
`takeover-managed-catalog-clippy.log`. Source audit:
`managed-catalog-source-review.md` (Gemini); reference code and executable fixtures
remain authoritative.

## Implementation review disposition

Gemini's `managed-catalog-implementation-review.md` identified the per-lookup deep
copy. Snapshots now hold `Arc<Value>`; a refresh swaps the pointer while existing
readers retain their immutable snapshot. Packaged instances also share one
process-wide catalog and retry window, addressing the isolated-instance finding
against the earlier constructor. Preloaded fixture catalogs remain independent.

The suggestion to reject Python coercions was not adopted: this port preserves
reference behavior. Strongly typed selection/download helpers remain future work
with their actual consumers. The packaged asset drift check already exists in the
generator's `--check` path. The reported initial monotonic-clock difference remains
a compatibility limit, not a claim of equivalent behavior.
