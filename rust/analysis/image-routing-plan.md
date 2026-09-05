# Image routing continuation plan

Source: `agent/image_routing.py` and its consumers in `gateway/run.py`.
Gemini supplied an initial audit; Codex checked the core dependency boundaries
and replaced the draft's unverified example APIs with the concrete steps below.
Raw helper output remains in `audit-image-routing.agy.log`.

## Current boundary

Update: configuration decisions, reference extraction, and the session-aware
wrapper are now implemented. See [routing verification](image-routing-verification.md).
Native content construction and prepared-part transport are also implemented;
see [image loading](native-image-verification.md) and
[HTTP transport](structured-content-verification.md). The live capability
lookup and rich adapter pipeline remain.

- `inbound_media::classify_media` returns `MediaClassification`, including image
  paths. It is compiled and tested but has no live Dispatcher consumer yet.
- `SessionRegistry::consume_pending_native_image_paths` atomically consumes the
  session buffer. `PersistentState.native_image_paths` holds that buffer.
- `vision_enrichment::enrich_message_with_vision` now ports the text path,
  including `sanitize_context`, through an explicit `VisionBackend` boundary.
- `dispatch.rs` receives `hermes_core::Message`, which carries optional prepared
  text/image parts. The native backend preserves those parts and structured
  history in streaming and tool requests. Platform adapters still supply text.
- Native file encoding/transcoding and the POSIX read guard are implemented,
  with HEIC/AVIF decoding still missing. Capability lookup and the live vision
  provider remain to be ported. The enrichment fixtures do not prove live
  provider integration.

## Required implementation sequence

1. Port `agent/image_routing.py` configuration decisions: `_coerce_mode`,
   `_coerce_capability_bool`, `_explicit_aux_vision_override`, and
   `_supports_vision_override`. Preserve requested-provider priority, `custom:`
   aliases, and candidate-first legacy-list lookup. Python coercion and Unicode
   whitespace behavior need differential coverage, not Rust-only examples.
2. Port runtime-aware capability resolution. `_lookup_supports_vision` checks
   config overrides, managed local runtime, models.dev, then eligible Ollama
   probes. Preserve exact provider/model matching before borrowing the session's
   requested-provider identity. Keep blocking network work off the async loop.
   Carry explicit session runtime into auxiliary vision calls.
3. Port image references, MIME sniffing, native content construction, and the
   actual `agent/file_safety.py` read guard. Do not substitute outbound
   `media::validate_media_delivery_path`, whose purpose differs. Native content
   needs byte sniffing, format conversion, skipped-file reporting, local path
   hints, and remote URL handling. Choose decoding dependencies only after
   checking supported formats against Python's Pillow/plugin behavior.
4. Extend the adapter-to-dispatch-to-agent transport to carry rich events and
   structured user content. Wire preparation, session buffers, routing and
   provider effects into that path. Preserve conversation prompt caching and
   avoid leaking session runtime through process-global configuration.
5. Exercise actual adapter and model HTTP paths with temporary homes and local
   test servers, then validate configured live providers where available. Pure
   callback fixtures remain contract tests, not evidence for this milestone.

## Draft corrections

The helper named `InboundMediaClassification::classify`, `GatewayConfigSkeleton`,
`dispatcher.rs`, and a ready-to-share `b64_encode` API. Those are not current
Rust interfaces: use `classify_media`, `config::Config`, and `dispatch.rs`;
`qqbot_crypto::b64_encode` is private and not an image transport implementation.
The draft's file-safety pseudocode and dependency suggestions were not source
ports and should not be copied into implementation.

Keep Rust tests inline in their implementation files. External Python fixtures
and generators belong in `rust/tools/`.
