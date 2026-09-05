# Inbound media and pending-state port, 2026-09-05

Codex integrated bounded work from Claude Opus 4.8 (medium) and Gemini 3.8
Flash (high). Both were launched through project wrappers with the requested
permission bypass. Python source bodies are the behavioral reference.

Tests now live inside their implementation modules, following the user's layout
preference and the existing inline `golden_corpus` convention in `config_loader.rs`.
Python generators and fixture data stay in `rust/tools/`.

## Evidence and scope

| Contract | Implementation | Verification |
| --- | --- | --- |
| Placeholder attachment order and MIME precedence | `media_context.rs` | 28 Python placeholder cases |
| Inlined, cached-text, and binary document notes | `media_context.rs` | 40 Python document cases |
| Audio/video attachment notes | `media_context.rs` | 4 input pairs, both outputs checked byte for byte |
| Attachment display names | `media_context.rs` | 14 source-executed cases, including Unicode categories and POSIX basename behavior |
| Pending transcript reuse, retry, null cache, echo gates, clarify and combined flow | `pending_stt.rs` | 27 transitions executed from Python source and replayed in Rust |
| Native image consumption | `session_registry.rs` | Cross-session isolation and simultaneous consumers |
| Pending event merge | `pending_messages.rs` | 166 Python merge cases and cache-invalidation integration tests |
| Cache staging, legacy layouts, and sandbox paths | `cache_paths.rs` | 224 mappings generated through real Python imports |
| Sender and reply context | `inbound_text_context.rs` | 144 context cases and 30 normalization cases |
| Transcription enrichment | `transcription_enrichment.rs` | 38 Python scenarios, comparing output and provider call order |
| Vision enrichment and context sanitizer | `vision_enrichment.rs` | 51 Python scenarios comparing output and provider call order |

The context builders accept already-resolved display names and agent-visible
paths. Path mapping and filename sanitization are tested separately. Actual speech providers, model
capability resolution, and live gateway adapter wiring remain outside these
tests. The new pending state travels alongside the MessageEvent, not in plugin
metadata or process-global storage.

## Corrections made during independent validation

- Claude's pending-path test expected a PDF sibling on a VOICE event to be
  excluded. The implementation returned both paths, as Python does. Corrected
  the test; adding an audio-MIME gate would have changed the source behavior.
- The original callback signature could only supply a String. A source-driven
  null-cache trace showed it could not represent a prepared null result.
  Changed the callback output to Option<String>; the outer cache Option still
  distinguishes unprepared state from a prepared result.
- Rust `str.trim()` retained U+001C through U+001F where Python `str.strip()`
  removed them. The clarify trace failed on both output content and the empty
  transcript filter. Added these separators to the trimming predicate.

Echo count advances before sends, including failed sends. This is the existing
Python behavior, not a delivery guarantee. Merging another note must clear the
derived transcript cache while keeping that count; replacing the pending event
must replace its STT state as well.

The combined transcribe/echo wrapper now mirrors the real helper's fallback:
transcription or routing failure returns the caller text with no transcripts,
while a successfully prepared cache remains reusable if routing failed. Its
send factory allows callers to resolve thread metadata after transcription.
The live adapter/router implementation still belongs to runner integration.

## Reproduce

Vision continuation: 930 workspace tests passed, one existing test ignored.
Clippy with warnings denied also passed. Logs: `takeover-vision-workspace-tests.log`
and `takeover-vision-clippy.log`. `gen_vision_goldens.py --check` uses mise Python
3.12.13 and executes the actual vision method plus the source memory sanitizer.
The sanitizer explicitly accounts for Python IGNORECASE's dotted/dotless I
behavior as well as its extra ASCII whitespace separators. Real vision calls
and native image encoding remain separate integration work.

Previous organization run: 911 tests passed, one existing Python-bridge test ignored.
Log: `takeover-organized-tests.log`. Formatting and clippy with warnings denied
also passed (`takeover-organized-clippy.log`). The 12 inbound-text tests passed
again after simplifying their repeated-string fixtures.

From the repository root, use mise's installed Python 3.12.13:

```bash
mise exec python@3.12.13 -- python rust/tools/gen_media_context_goldens.py --check
mise exec python@3.12.13 -- python rust/tools/gen_pending_stt_goldens.py --check
mise exec python@3.12.13 -- python rust/tools/gen_pending_message_goldens.py --check
cargo test --manifest-path rust/Cargo.toml --workspace
cargo clippy --manifest-path rust/Cargo.toml --workspace --all-targets -- -D warnings
```

The cache-path and sender-context generators use real imports through the shared
Hermes Python 3.11.15 venv at `/home/eins0fx/.hermes/hermes-agent/venv/bin/python`.
Run `gen_cache_path_goldens.py` and `gen_inbound_text_goldens.py` with that interpreter
and `--check`. Run `gen_transcription_goldens.py --check` with mise Python 3.12.13.

The runner oracles execute extracted Python functions and expressions. They do not
import and run GatewayRunner as a whole. Refactors that move the selected
functions require updating the extraction, and live provider/adapter testing
belongs to the later runner integration milestone.

Raw helper logs live locally in `rust/analysis/port-media-context.agy.log`,
`port-pending-stt.claude.json`, `port-pending-messages.claude.json`, and
`pending-stt-review.agy.log`. Distilled review: `pending-stt-review.md`.
