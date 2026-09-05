# Cloud model metadata and context

`models_dev.rs` now exposes capability and context lookup over the live registry
cache. `image_routing::lookup_catalog_vision` calls the capability path with cold
network access permitted, matching this specific Python routing stage. Ordinary
callers can explicitly disable network access.

## Behavior and evidence

`gen_cloud_metadata_goldens.py --check` executes Python's provider/model matching,
override selection, capability extraction, and context lookup. Its 683 cases cover:

- The exact static Hermes-to-models.dev provider map and reverse override aliases.
- Exact, Unicode-lowercased, and cloud-suffix model matches.
- Explicit overrides, empty overrides, provider defaults and global defaults.
- Input modalities taking precedence over the older attachment flag.
- Python truthiness, numeric coercion, invalid positive overrides, and family values.
- Context lookup continuing past entries with unusable context, while capability
  lookup still treats those entries as known.
- Case-insensitive collisions where Python's first inserted key wins.

Four real HTTP/offline integration tests verify the catalog vision stage, refreshed
config inputs, unknown-provider overrides avoiding network, and explicit context
preceding even an allowed network fetch. All Rust tests remain inline.

## Object ordering and canonical encodings

The collision cases reproduced an incorrect vision verdict under sorted JSON maps
(`cloud-metadata-order-before.log`). The workspace now enables serde_json's
[documented insertion-order feature](https://docs.rs/serde_json/latest/serde_json/map/index.html).
The same cases then pass for registry entries and override dictionaries.

Canonical serialization remains explicitly sorted where required: hosted room logs,
policy checkpoints, idempotency status, and sorted Python fixture comparisons.
Production JSON-map removals in configuration and pairing preserve the remaining
key order. Nested canonical encoding tests confirm key-order independence and that
encoding does not mutate its input. Existing signing, checkpoint, transport, and
storage tests pass with the feature enabled.

Shared scalar coercions now live in `python_value.rs`, used by image routing and
both catalogs. This extraction retains the existing executable fixture coverage.

## Validation and remaining work

Workspace: 1,069 passed, one existing bridge test ignored. Formatting, Clippy with
warnings denied, fixture regeneration checks, and diff whitespace checks pass.
Logs: `cloud-metadata-workspace-tests.log`, `cloud-metadata-clippy.log`.
Gemini's source audit is `cloud-metadata-source-review.md`; source execution is the
reference for behavior, including Unicode matching rather than the audit's proposed
ASCII-only comparison.

The static models.dev mapping is not the dynamic provider-profile registry. That
registry, provider-prefix stripping, runtime identity borrowing, and the complete
vision waterfall still need integration. Rich model/provider listing, pricing,
override-to-catalog projection, and download/selection metadata consumers remain
separate port work. Integer values retain serde/Rust bounds rather than Python
arbitrary precision; non-finite values are not representable in the JSON interface.
