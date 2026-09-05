# Image routing verification, 2026-09-05

Codex integrated Claude's configuration routing port, Gemini's reference
extraction port, and the session-aware runner wrapper. All Rust tests are
inline in their implementation files.

| Implementation | Python comparison | What it demonstrates |
| --- | --- | --- |
| `image_routing.rs` | 392 cases | Boolean/mode coercion, provider override priority, auxiliary vision selection, lookup gates and errors |
| `session_image_routing.rs` | 28 cases | Explicit overrides, partial session resolution, fallback defaults, two exception boundaries and effect order |
| `image_references.rs` | 46 cases with real temporary files | Local existence checks, home expansion, code exclusion, Unicode regex behavior, URL quirks and deduplication |

The configuration and reference generators import the actual Python
`agent/image_routing.py`. The session generator executes the runner's actual
method body with controlled runtime operations. Capability lookups are recorded
test doubles; these tests do not prove the managed-runtime/models.dev/Ollama
lookup chain, live image decoding, auxiliary provider calls, or adapter transport.

## Corrections from independent verification

The initial configuration port used Rust `trim`, causing the Python-derived
control-separator boolean case to fail. All relevant configuration trims now
include U+001C through U+001F. Compound provider values also need Python-style
representation rather than compact JSON to preserve source lookup behavior.
The corpus includes compound and scientific-notation provider names.

Gemini accounted for Python's Unicode word classes and scoped case folding in
reference extraction. Codex replayed the filesystem corpus against the compiled
module. The source deliberately matches the image suffix prefix of some URLs;
the port preserves that behavior instead of substituting a URL parser.

## Reproduce

From the repository root:

```bash
mise exec python@3.12.13 -- python rust/tools/gen_image_routing_goldens.py --check
mise exec python@3.12.13 -- python rust/tools/gen_session_image_routing_goldens.py --check
mise exec python@3.12.13 -- python rust/tools/gen_image_reference_goldens.py --check
cargo test --manifest-path rust/Cargo.toml --workspace
cargo clippy --manifest-path rust/Cargo.toml --workspace --all-targets -- -D warnings
cargo fmt --manifest-path rust/Cargo.toml --all --check
```

Results: 965 passed, one existing ignored test; clippy with warnings denied and
formatting pass. Local logs: `takeover-image-routing-tests.log`,
`takeover-routing-workspace-tests.log`, and `takeover-routing-clippy.log`.

Next: implement real capability/runtime resolution and native content handling,
then connect rich adapter events through dispatch and the agent boundary. The
new wrapper accepts the real `SessionSource` type but still has no live runner
consumer. It must use the same session model resolution as the upcoming turn.
