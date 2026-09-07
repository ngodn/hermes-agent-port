# Discord audio attachment integration, 2026-09-06

Reference: `plugins/platforms/discord/adapter.py`, the addressed-message audio
branch around line 8438 and `_cache_discord_audio` around line 8136. The
[official message resource](https://docs.discord.com/developers/resources/message)
documents attachment URLs/content types and voice messages with no text content.

The native MESSAGE_CREATE extractor now retains empty-content events when they
contain an audio attachment, while still dropping bot messages. Preparation runs
inside the Gateway read loop before sending to Dispatcher. Audio attachments
are downloaded in source order and cached into the configured profile; existing
text is preserved. Startup provides the same profile and inbound size setting
used by the Telegram path.

Audio MIME classification follows the Python branch, including case sensitivity,
parameter stripping and the five-extension fallback allowlist. Sixteen cases
execute the actual AST-extracted assignment and allowlist from that branch.
`gen_audio_cache_goldens.py` now checks both these cases and the 981 shared
container-extension cases.

The shared `cache_audio_response` checks advertised length and cumulative chunks,
then uses the container-aware profile cache. Telegram now uses the same reader.
Both clients disable redirects; the shared reader rejects non-success responses,
including 3xx (reqwest's error_for_status alone does not reject them).

The Discord integration test passes a MESSAGE_CREATE WebSocket frame through
the actual read loop, mapping a Discord CDN hostname to a local HTTP server in
the test client. It checks two cached files in attachment order, byte content,
sniffed extension, ignored image attachments, empty message content and sequence
tracking. A bot event performs no downloads. A response streamed without a
Content-Length exercises cumulative size rejection. Redirects preserve captions
and produce no cached path. Captured requests contain no Authorization header.
Tests cover rejected lookalike CDN hosts, userinfo and non-HTTP schemes.

Current boundary: download URLs must use cdn.discordapp.com or
media.discordapp.net. Signed query parameters are preserved. The native downloader
does not send a bot token to attachment hosts. The Python SDK-read plus generic
SSRF-gated URL fallback is not fully ported: other attachment origins, refresh of
expired signed URLs and fallback retries remain work. Failure handling currently
adds a generic note; Python preserves the remote URL in its richer event model.
Exact fallback representation needs that event integration rather than passing a
remote URL as a local `audio_paths` entry. Broader Discord group/thread/mention,
reconnect/resume and other-media parity remain unfinished.

The existing Telegram integration exercises shared cached bytes through real
STT HTTP and Dispatcher; the Discord test targets the new Gateway/download
boundary. This is not a claim of complete Discord adapter parity.

Validation: 1,252 workspace tests passed (one core, 1,251 gateway), two ignored
by default. Clippy with warnings denied, formatting, fixture regeneration and
diff whitespace checks pass. Local logs: `/tmp/hermes-discord-audio-workspace.log`
and `/tmp/hermes-discord-audio-clippy.log`.
