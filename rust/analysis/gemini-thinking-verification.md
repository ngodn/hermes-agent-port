# Gemini thinking caps and chat wire normalization

The native request path now applies the chat transport's reasoning normalization
before provider hooks and output-cap projection. It clamps ultra to max while
preserving other config fields and leaving the stored session configuration
unchanged. Vercel's raw hook still passes its input through, but the actual HTTP
request now carries the normalized effort. The earlier raw-hook fixture was valid;
the previous live ultra expectation omitted this transport step and was corrected.

Gemini output-cap raising is connected to both streaming and every tool request.
For matching Gemini models with a nonempty thinking config, it performs the Python
integer coercion and default/headroom rules before choosing the cap's wire field.
A disabled config still reaches cap validation; small positive caps stay unchanged
when thinking is disabled, while invalid/nonpositive caps use the reference default.
Larger user caps remain unchanged, including unsigned JSON integers above i64::MAX.
Caller request overrides are merged afterward and retain final precedence.

Claude ported the cohesive thinking helpers into gemini_thinking.rs: config building,
camel/snake normalization, headroom detection and effective cap calculation. The
REST-oriented helpers are available for the later native Gemini transport, while
raise_output_cap is the live consumer now. Main-agent integration replaced the
helper's initial i64 saturation with exact i64/u64-backed JSON values.

## Evidence

Source-executed fixtures cover 64 Gemini cases and 51 pre-hook normalization cases.
The Gemini generator extracts the actual functions and default cap constant from
Python. Additional inline tests cover disabled config behavior, passthrough and
unsigned integer preservation. Rust tests remain beside their implementations.

A startup HTTP regression first failed with max_tokens=1024 instead of 65535
(gemini-headroom-before.log). It now passes across 14 local requests, covering both
streaming and tool paths, enabled/disabled thinking, zero/larger caps, Gemma exclusion
and the exact google/ prefix rule. Existing Vercel HTTP tests also verify ultra
becomes max before its hook emits reasoning. No public paid inference was used.

Validation: 1,102 workspace tests passed, one existing bridge test ignored.
Formatting, Clippy with warnings denied, diff whitespace and both generators'
--check commands under Python 3.12.13 pass. Logs: gemini-thinking-tests.log and
gemini-thinking-clippy.log. Helper evidence: gemini-thinking-claude.md and
wire-reasoning-source-review.md. The Gemini audit's incorrect Upstage default was
corrected against the actual source (medium).

## Remaining scope

Native Gemini REST request/response translation, reasoning-field projection on its
OpenAI-compatible endpoint, ephemeral reasoning-off/cap lifecycle, mandatory-reasoning
retry state and the complete native conversation loop remain open. These helpers do
not claim those integrations. Integer coercion beyond serde_json's i64/u64 range
still differs from arbitrary-precision Python integers. Cap raising only runs when
a cap was selected, matching the existing chat transport call sites; this does not
imply native REST's default-cap behavior is already wired.
