# Pending STT State: Parity and Integration Review

Codex follow-through: the composite helper and PendingMessage event/state bundle
identified below are now implemented. The transition oracle was extended from
22 to 27 steps for composite fallback and cache reuse; pending merges also pass
166 Python cases. Live adapter/provider integration remains open. The findings
below describe the earlier snapshot reviewed by Gemini.

Reviewer: Gemini 3.8 Flash high. Scope: `pending_stt.rs`, `pending_stt_parity.rs`,
and `gen_pending_stt_goldens.py`. Audited against `gateway/run.py:27963-28086`,
`gateway/run.py:20952-20965`, and `gateway/platforms/base.py:2892-2914`.

## Verdict

The slice faithfully mirrors Python's cache and echo mechanics across 22 replayed
transitions. The corrections previously applied by Codex (VOICE+PDF filter parity,
`Option<String>` cached null representation, and U+001C..U+001F whitespace stripping)
align the implementation with Python AST execution. No algorithmic parity bugs were
found in the four ported functions.

## Integration Limitations and Contract Boundaries

1. Composite helper unported (`gateway/run.py:28043-28086`):
   Real gateway callers (`run.py:11262`, `11548`, `19606`, `32049`, `32331`, `32619`)
   do not call `_transcribe_pending_audio_event_once` directly; they route through
   `_transcribe_and_echo_pending_voice`. That helper manages audio guards, thread
   metadata resolution, non-fatal transcription exception fallback to `(text, [])`,
   and empty-transcript fallback (`enriched_text or text`). In Rust, callers must
   compose `transcribe_pending_audio_event_once` and `echo_pending_stt_transcripts_once`
   manually until a composite wrapper is added.

2. State lifecycle decoupled from `MessageEvent`:
   Python attaches `_gateway_pending_stt_*` attributes directly to `event`. Rust models
   transient state in `PendingStt`. Pending queues and `merge_pending_message_event`
   (`base.py:2958, 2978`) cannot automatically invalidate or preserve state without an
   explicit pairing (such as `PendingMessage { event, stt }`) in the queue owner.

3. Preparation check boundary in inbound text preparation:
   In `gateway/run.py:20526-20531` and `20597`, `_pending_stt_prepared` gates both
   audio classification and message text replacement. `inbound_media::classify_media`
   takes `pending_stt_prepared: bool`, but does not substitute `event.text`. The runner
   must query `PendingStt::is_prepared()` to pass the flag and apply cached text.

4. Inherited clarify cache pollution (`gateway/run.py:20957-20959`, `pending_stt.rs:223`):
   `prepare_clarify_reply_text` invokes transcription with `user_text = Some("")`.
   If a voice message carries a caption and is evaluated for clarify, but execution
   later falls through to the normal turn, the cached transcription retains the empty
   caption. This faithfully matches Python behaviour.

5. Diagnostic label omission (`gateway/run.py:28010`, `pending_stt.rs:198-200`):
   `echo_pending_stt_transcripts_once` logs echo failures as generic debug traces,
   omitting Python's `log_context` parameter ("Voice-interrupt", "Voice-drain").

## Test Oracle Limits

`gen_pending_stt_goldens.py` AST-extracts the four runner methods and invalidator into
an isolated `Runner` harness. It validates state machine transitions under mocked STT
and echo callbacks. External speech providers, queue ownership, and live adapter
transports remain outside this verification slice.
