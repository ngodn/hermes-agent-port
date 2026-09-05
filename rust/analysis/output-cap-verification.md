# Native output caps and profile request defaults

Native startup now resolves the output cap from HERMES_MAX_TOKENS and
model.max_tokens using the gateway resolution block followed by AIAgent's init
fallback. The resolver also accepts an already-resolved runtime default, for the
provider runtime resolver to supply when that integration is ported.

Caps reach streaming and tool requests. The wire field matches Python's
URL-first selector: direct OpenAI, Azure OpenAI and Copilot subdomains use
max_completion_tokens; recognized OpenAI model families use it on other hosts
too. Other routes use max_tokens. URL paths and lookalike hostnames do not make
an endpoint native. Vendor prefixes are stripped before the model-family check.

Profile default_max_tokens now supplies a cap when the caller has none. Fixed
profile temperatures are projected, and explicit omission removes temperature.
Request hooks run after these defaults, preserving the Python precedence for
profile fields. Message history is not rewritten.

## Evidence

The generator executes the actual gateway/init cap-resolution blocks and the
AIAgent selector methods with their real utils.py helpers. There are 132 parameter
selection cases and 364 resolution cases, covering environment precedence,
invalid/empty settings, zero, booleans, runtime defaults, numeric coercion,
Unicode digits, host spoofing and vendor prefixes. Tests remain inline.

The startup HTTP test adds eight real local requests covering streaming and tool
modes, model.max_tokens integer/string inputs, invalid input omission and the two
wire parameter names. The temporary environment excludes an inherited
HERMES_MAX_TOKENS value and restores it on exit. Inline profile-default tests
cover fixed/omitted temperature and explicit zero overriding a profile cap.

Validation: 1,088 workspace tests passed, one existing bridge test ignored.
Clippy with warnings denied, formatting, diff whitespace and Python 3.12.13
source-generator --check pass. Logs: output-cap-tests.log and
output-cap-clippy.log.

## Remaining scope

Ephemeral per-call cap consumption, Gemini thinking-budget cap expansion,
Anthropic fallback caps, custom per-model get_max_tokens hooks and request
extra_body/override merging remain open. Native startup does not yet provide the
custom-provider runtime max_output_tokens fallback to this resolver. The runtime
argument is tested against Python, but that caller integration is not claimed.
Python integer coercion remains bounded by serde_json's integer range in Rust.
No live paid inference was invoked.

## Review follow-up

Claude resumed after the user confirmed the rate-limit reset and completed the
native provider integration review. Its two concrete findings were addressed in
this slice. The SSE consumer now buffers raw bytes until a line is complete,
preventing loss of multibyte characters split by network chunks. An inline test
first failed on a split within 你 (streaming-unicode-before.log), then passed at
every byte split for CJK, emoji and accented text, with and without a final newline.
The same consumer handles the real reqwest stream.

Generic credential resolution now prefers saved dotenv values per candidate,
matching the already-ported profile resolver. Two additional startup HTTP requests
prove a rotated generic key wins over a stale shell export in streaming/tool modes.
The environment is restored and readers share the global test lock.

Review evidence: native-provider-integration-claude-review.md and
output-cap-source-review.md. Both helpers completed; Claude reported the requested
claude-opus-4-8 model. Other scope findings remain tracked above and in earlier
provider verification notes.

Follow-up: Gemini thinking-budget cap expansion is now wired and verified in
[gemini-thinking-verification.md](gemini-thinking-verification.md).
