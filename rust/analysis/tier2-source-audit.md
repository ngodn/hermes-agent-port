# Tier 2 Source Audit: Inbound Attachment Classification & Pipeline Seams

Codex review: classification predicates and the four-bucket loop were checked
against source and 217 executed Python cases. The implemented Rust result is
`MediaClassification`, not the proposed name below. Line references are
navigation hints, not a generated index. Model routing, enrichment, and sandbox
translation remain untested integration dependencies.

## 1. Attachment Classification Rules in `gateway/run.py`

Inbound classification inspects `MessageEvent` attachments by per-file MIME first, falling back to message-level `MessageType` only when the attachment MIME is empty:

- `_event_media_type_at(event, index)` (run.py:3616-3623):
  Safely indexes `event.media_types`. Returns `""` if `media_types` is absent or `index >= len(media_types)`.
- `_event_media_is_image(event, index)` (run.py:3626-3638):
  If `mtype` is non-empty, checks `mtype.startswith("image/")`. Only falls back to `event.message_type == MessageType.PHOTO` when `mtype` is empty.
- `_event_media_is_audio(event, index)` (run.py:3641-3647):
  If `mtype` is non-empty, checks `mtype.startswith("audio/")`. Only falls back to `event.message_type in {MessageType.VOICE, MessageType.AUDIO}` when `mtype` is empty.
- `_event_media_is_stt_input(event, index)` (run.py:3649-3657):
  Critical gating rule: if `event.message_type in {MessageType.AUDIO, MessageType.DOCUMENT}`, returns `False` unconditionally. Otherwise returns `True` if `event.message_type == MessageType.VOICE` or `_event_media_type_at(event, index).startswith("audio/")`.
- `_event_media_is_video(event, index)` (run.py:3660-3666):
  If `mtype` is non-empty, checks `mtype.startswith("video/")`. Only falls back to `event.message_type == MessageType.VIDEO` when `mtype` is empty.

## 2. Inbound Partitioning & Parity Cases

In `_prepare_inbound_message_text` (run.py:20579-20601), media attachments are partitioned into four buckets:
1. `image_paths`: populated when `_event_media_is_image(event, i)` is true.
2. `audio_file_paths`: populated when `_event_media_is_audio(event, i)` is true AND `event.message_type in {MessageType.AUDIO, MessageType.DOCUMENT}`. These bypass STT and receive contextual prompt notes (run.py:20697-20704).
3. `audio_paths` (STT pipeline): populated when `_event_media_is_audio(event, i)` is true, NOT in `{AUDIO, DOCUMENT}`, NOT `_pending_stt_prepared`, and `_event_media_is_stt_input(event, i)` is true.
4. `video_paths`: populated when `mtype.startswith("video/") or (not mtype and event.message_type == MessageType.VIDEO)`. These receive contextual prompt notes (run.py:20716-20723).
5. Document fallback (run.py:20739-20778): any attachment not classified into images, audio, or video falls through. If MIME is missing or `application/octet-stream`, extension matching against `_TEXT_EXTENSIONS` overrides to `text/plain`, else `mimetypes.guess_type`.

Key parity invariants:
- Mixed MIME isolation: A non-image file uploaded in a `MessageType.PHOTO` message is not routed to vision if its MIME is not `image/*`. It falls through to document handling.
- Audio file vs voice note distinction: Audio attachments with message type `AUDIO` or `DOCUMENT` are treated as static file attachments, never sent to STT.
- STT memoization: `_pending_stt_prepared` checks `hasattr(event, "_gateway_pending_stt_text")` (run.py:20526). If present, `_prepare_inbound_message_text` uses the cached transcription and skips STT execution.
- Transcript echo deduplication: `_echo_pending_stt_transcripts_once` (run.py:28002-28042) tracks `_gateway_pending_stt_echoed` as an integer count, slicing `transcripts[already_echoed:]`. This allows appending subsequent voice notes without duplicate echoes while preserving identical repeated phrases.

## 3. Identification of Non-Pure Dependencies in Tier 2

`rust/analysis/run-py-map.md` initially characterized Tier 2 as pure transformations. Actual source inspection reveals several non-pure side effects and external dependencies:
1. Network I/O in image routing: `_decide_image_input_mode` (run.py:20610, 27677-27748) offloads to `asyncio.to_thread` because it performs blocking HTTP calls: fetching `models.dev` on cache miss and querying local Ollama `/api/show`.
2. External vision model calls: `_enrich_message_with_vision` (run.py:20649-20652) invokes `agent.auxiliary_client.scoped_runtime_main` to execute `vision_analyze`.
3. External STT services: `_enrich_message_with_transcription` (run.py:20655-20658, 27994-27997) calls external transcription tooling/APIs.
4. Outbound adapter network I/O: `_echo_pending_stt_transcripts_once` (run.py:28035) calls `adapter.send(source.chat_id, ...)` to post transcript bubbles in real-time.
5. Shared session state mutation: `self._consume_pending_native_image_paths(session_key)` (run.py:20540, 20966) and `session_state.persistent.native_image_paths = list(image_paths)` (run.py:20617-20619) directly mutate `SessionState.persistent.native_image_paths` (existing in `session_state.rs:130`).
6. Sandbox path translation: `to_agent_visible_cache_path` (run.py:20695, 20714, 20768) translates host cache paths to `/root/.hermes/cache/*` container mounts for Docker environments.

## 4. Current Rust State and Division of Labor

Existing Rust foundation:
- `platform_base_types.rs`: `MessageType` (L30-75) and `MessageEvent` (L118-222) with `media_urls`, `media_types`, `media_text_inlined`.
- `session_state.rs`: `SessionState` (L148-152) and `PersistentState.native_image_paths` (L130).
- `session_registry.rs`: `SessionRegistry` (L32-142) with thread-safe `with_session` and `peek`.

Division of labor:
- Claude owns ONLY the pure classification helpers in `inbound_media.rs`:
  `event_media_type_at`, `event_media_is_image`, `event_media_is_audio`, `event_media_is_stt_input`, `event_media_is_video`, and an `InboundMediaClassification` partitioning helper.
- Codex generates differential tests against Python output vectors.

## 5. Next Bounded Implementation Step After Classification

Once classification helpers in `inbound_media.rs` are verified, the next bounded implementation step is the deterministic context note formatting pipeline:
1. `_build_media_placeholder` (run.py:3668-3688): pure formatting of fallback text placeholders when events lack text.
2. `_build_document_context_note` (run.py:3690-3738): pure markdown note generation for document attachments based on MIME and inline flags.
3. Audio and video file attachment context notes (run.py:20696-20705, 20715-20724): deterministic string formatting for non-STT media.
4. Keep all external I/O (STT, vision LLM, Ollama probing, Docker path translation, adapter echoes) behind explicit trait seams for later turn-runner integration.
