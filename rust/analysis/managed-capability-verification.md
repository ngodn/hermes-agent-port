# Managed local capability verification

managed_capabilities.rs now reads staged models and runtime state from an
explicit shared Hermes root. It queries /health and /props over real async
HTTP, then uses the supplied local catalog and projector files when the live
server cannot answer. image_routing::lookup_managed_vision connects provider
and URL resolution to this stage.

## Evidence

gen_managed_capability_goldens.py executes the Python staging, catalog data
classes/parser, and managed capability function on temporary filesystem trees.
Its 50 cases cover 14 staging layouts and 36 live/catalog/projector combinations.
The Python capability oracle controls the live result; the corresponding Rust
tests obtain it through actual loopback HTTP requests and compare call order.

The fixtures include incomplete and complete splits, Unicode total counts,
zero-part totals, directories matching *.gguf, hidden files, duplicate model
IDs, and the special .gguf filename. Tests preserve live false over catalog
true and verify that unstaged models make no capability request.

Additional HTTP tests verify the health request with a dead PID, refusal to
claim that endpoint, a starting server with a live PID, case-sensitive custom
endpoint matching, Python truthiness for props values, bearer forwarding, and
raw model query behavior. Staged-file tests and HTTP tests are inline in the
implementation file.

Workspace validation: 1,048 tests passed, one existing bridge test ignored.
Formatting and Clippy with warnings denied pass. Evidence logs are
takeover-managed-workspace-tests.log and takeover-managed-clippy.log.

## Implementation and remaining scope

Models and assets use <root>/models and <root>/models/assets. State comes from
<root>/runtimes/llamacpp/server.json. The caller resolves the shared root; the
component does not read process-global profile configuration.

The full managed supervisor, boot/readiness flow, downloads, and final live
session capability lookup remain unfinished. Curated catalog loading and refresh
are now implemented; see [catalog verification](managed-catalog-verification.md).
This component accepts supplied catalogs or the shared packaged catalog. The accepted PNG/JPEG MIME list is
available, but runtime selection of image transcoding policy still needs wiring.

POSIX PID checks use signal zero and treat permission denial as an existing
process. Other platforms currently use the Python-style optimistic fallback;
a native Windows liveness implementation and cross-platform tests remain.
Unusual malformed state values, URL encodings, and redirect cycles are not
exhaustively equivalent to Python's urllib/psutil behavior.

Claude reached its session limit before producing code. Codex implemented and
verified the module directly; Gemini supplied the accompanying source review.
The failed helper invocation is retained in port-managed-capabilities.claude.json.
