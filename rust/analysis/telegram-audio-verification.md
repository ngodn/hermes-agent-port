# Telegram inbound audio, 2026-09-06

Resumed from Claude's committed checkpoint `f1859c87cd`, after reading the latest
Claude transcript and the STT/credential continuation commits. The reference
download branches are in `plugins/platforms/telegram/adapter.py` around line
10356; cache behavior is `gateway/platforms/base.py::cache_audio_from_bytes` and
container detection is `tools/audio_container.py::sniff_audio_ext`.

Telegram extraction now accepts voice/audio-only messages and captions. The
poll loop prepares supported updates with `getFile` and downloads the returned
relative file path before forwarding the message to Dispatcher. The Bot API
contract was checked against the [official getFile documentation](https://core.telegram.org/bots/api#getfile).
The downloader rejects malformed/traversing file paths, disables redirects,
checks advertised length and cumulative chunk size, and avoids exposing token-
bearing request errors. This is the hosted API path, not local Bot API absolute
filesystem downloads.

`audio_process::cache_audio` validates the inbound size, sniffs the real audio
container, writes an `audio_<12 hex digits>` filename, and respects populated
legacy `audio_cache` directories. The limit comes from
`gateway.max_inbound_media_bytes`, default 128 MiB. Zero disables it. Negative
limits reject all sizes, following Python's implementation rather than its
contradictory docstring. Extremely large integer configuration values outside
Rust's i64 range still fall back to the default.

The first cache integration run exposed an incorrect test assumption: an empty
legacy directory does not win over the current directory in either reference
or Rust. The fixture now contains an existing cached file and verifies legacy
selection without changing that shared policy.

981 source-executed extension cases cover every second byte after 0xff,
truncated signatures, MP4 audio-context mapping, RIFF/WAVE versus WEBP, and
fallback-dot handling. Regenerate/check with:

```bash
mise exec python@3.12.13 -- python rust/tools/gen_audio_cache_goldens.py --check
```

The inline Telegram HTTP test exercises getFile and file download, checks
cached bytes, then sends the resulting Message through Dispatcher with a real
HTTP transcription backend. A recording agent observes the returned transcript
and original caption. Both voice and audio shapes run this path. Only final
model execution and local context mapping are test boundaries; downloaded bytes,
filesystem access, multipart upload and Dispatcher enrichment are real. Separate
failure cases cover an invalid server file path and a configured size cap.

Remaining scope: Discord/Slack audio, Telegram group/mention/thread policy,
pre-download metadata admission and exact Python error-note/reply behavior,
other attachments, pending-event routing and complete provider credential
support. Current failure handling adds a generic observation to the caption.
This does not claim complete Telegram or full audio-provider parity.

Final validation: 1,250 workspace tests passed (one core and 1,249 gateway),
two ignored by default. Clippy with warnings denied, formatting, generator
`--check` and diff whitespace checks pass. Local logs:
`/tmp/hermes-telegram-audio-final.log` and `/tmp/hermes-telegram-audio-clippy.log`.
