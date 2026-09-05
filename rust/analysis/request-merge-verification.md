# Native request overrides and Vercel profile

Provider request hooks now return separate top-level and extra-body maps. The
native client applies caller overrides after profile defaults and hooks, then
flattens extra_body into the final HTTP JSON object. This preserves Python's
shallow replacement of nested objects and extra-body precedence over overlapping
typed fields. Null extra_body is omitted; invalid non-mapping values error before
network I/O unless a populated profile map supersedes them during assembly.

The source SDK contract is the installed OpenAI Python 2.24.0 implementation,
checked against the [official request builder](https://github.com/openai/openai-python/blob/main/src/openai/_base_client.py).
A source generator executes Hermes' override loop and that SDK's _merge_mappings;
no manually approximated recursive merge is used as the oracle.

Vercel AI Gateway now registers its profile and reasoning hook natively. Its hook preserves the supplied reasoning config. The chat transport now applies
the shared wire clamp first, so ultra reaches the hook as max. A missing config defaults
to enabled/medium. The native route gate recognizes the unconditional Nous and
Vercel hostname cases from the Python runner; other catalog-based route decisions
remain open. Aliases, attribution headers and default auxiliary model come from
the actual Python profile declaration.

Claude implemented the custom-provider body selector in custom_request_config.rs.
It matches normalized URL, optional custom:<name> identity and model catalog,
retaining the reference fallback order. Native startup supplies the legacy-shaped
custom_providers list and carries the selected body into streaming/tool requests.
The existing explicit runtime endpoint remains authoritative.

## Evidence

All tests are inline. Source fixtures cover 32 Vercel hook cases, 24 custom request
selection cases and 20 Hermes/SDK merge cases. Additional selector tests check URL
normalization and matching order. Six local HTTP requests verify Vercel defaults,
wire effort clamping and caller replacement in streaming/tool modes. The HTTP
client resolves the genuine Vercel hostname to a local listener and disables proxies,
so the real hostname gate is exercised without public DNS or paid inference.
Two startup HTTP requests verify custom:lab settings become actual body fields
while prior messages remain unchanged.

Validation: 1,095 workspace tests passed, one existing bridge test ignored.
Clippy with warnings denied, formatting and diff whitespace pass.
Regeneration checks:

- Python 3.12.13: gen_vercel_goldens.py --check and gen_custom_request_goldens.py --check
- Checkout .venv Python with OpenAI SDK 2.24.0: gen_request_merge_goldens.py --check

Logs: request-merge-tests.log, request-merge-clippy.log.
Helper reports: custom-request-config-claude.md, extra-body-merge-source-review.md.

## Remaining scope

The custom selector accepts normalized legacy entries; unified providers config
normalization, custom endpoint/credential runtime resolution and runtime-supplied
request overrides still need integration. Native invalid non-array entry lists are
treated as empty, unlike Python's possible TypeError for non-iterables.
Vercel's sequence-of-pairs config conversion accepts string keys; arbitrary Python
mapping keys are outside the JSON configuration boundary.

This change does not implement all SDK request options or reject every unknown SDK
keyword. Custom config additions enter through extra_body, where arbitrary body
fields are supported. Transport options such as extra_headers, extra_query and
timeout require their own native handling. Profile build_extra_body overrides,
caller extra_body_additions, native Gemini filtering, prompt-cache-key construction
and the remaining route capability resolver are still open. Existing Upstage and
Nebius hooks retain their top-level behavior.

Follow-up: gemini-thinking-verification.md records the transport normalization step
that was missing from the initial Vercel wire integration. Raw hook fixtures remain
valid; the live HTTP expectation now reflects pre-hook ultra-to-max clamping.
