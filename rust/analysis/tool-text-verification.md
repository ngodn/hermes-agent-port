# Tool-template marker cleanup

The native loop removes bare ASCII bracketed markers from assistant content
alongside a validated tool batch, following `_STALE_MARKER_RE` in the Python
conversation loop. It runs after batch filtering and before the assistant replay
is appended. Ordinary text, non-ASCII markers, and plain final answers remain
unchanged. All-invalid name-retry batches bypass this cleanup.

17 fixtures execute the actual Python regex and run through the native tool
loop. They verify the next model step sees the expected assistant content,
including Python whitespace behavior and exact marker boundaries. Workspace:
1,206 tests passed, one existing bridge test ignored. Clippy and formatting pass.

This removes protocol scaffolding from replay. The native loop now retains visible answers from batches containing only
`memory`, `todo_list`, `skill_manage`, or `session_search`. A later substantive
batch invalidates that answer even when its assistant content is empty. Empty
or reasoning-only final responses can recover the retained answer without
rewriting prior replay messages or introducing a synthetic user message.

`visible_response.rs` follows the ordered string cleanup in
`agent_runtime_helpers.strip_think_blocks`: reasoning tag variants, closed tool
XML, boundary-gated named functions, and truncated protocol tails. Its 156
fixtures execute the actual Python function and pattern definitions. An inline
native-loop test covers eight recovery/invalidation sequences and checks final
events, request count, and preserved replay. This path accepts strings, matching
the native response parser; Python's list/dict coercion remains outside it.

Post-tool nudging now follows `conversation_loop.py`'s recent-five-message
tool-result check, once-per-stall guard, and inline-thinking exclusion. A valid
new tool batch resets the guard. The retry appends `(empty)` assistant content
and the exact reference user nudge, both marked as private recovery messages.
Housekeeping answer recovery takes precedence. Four scripted sequences check
bounded retries, reset after new work, role order, and stable request prefixes.
A local HTTP test executes a real native tool call, receives an empty provider
response, and recovers on the third request. It verifies that request projection
strips internal metadata, preserves the previous message prefix, and retains the
tool schema for continued work.

The current Dispatcher persists only the user/final answer, so these private
retry messages are not stored. Full transcript persistence will also require
Python's terminal scaffolding cleanup and interrupted-turn repair.

Partial-stream recovery, thinking prefill, general empty-response retry and
provider fallback, preview/status metadata, and stream muting remain pending.
Normal native tool-loop final delivery now uses the same cleanup before
emitting the answer, following `conversation_loop.py:8877`. The 156 source
cases also run through the loop and check final events. The HTTP recovery test
includes reasoning and tool XML in the final provider response and verifies
that only the visible answer is delivered. Dotted/dotless-I tag spelling follows
Python's case-insensitive behavior.

The no-tool streaming path now runs a per-response `ThinkScrubber` before
emitting SSE text deltas. It follows the upstream `agent/think_scrubber.py`
state machine: closed pairs anywhere, boundary-gated unclosed openers, partial
tag retention, orphan closer removal, and response-boundary flushing. 526
source-executed sequences compare every delta and flush result. Six SSE cases
split network input into single bytes, including multibyte visible text, split
reasoning tags, unfinished blocks, final partial prose, DONE, and newline-free
EOF. Each checks visible output and one terminal event.

Streaming covers ASCII-case reasoning tag spellings. The Python source's
Unicode-lowercase index behavior for unusual non-ASCII tag spellings still needs
comparison. Tool XML filtering during streaming is also still pending; the
upstream reasoning scrubber itself does not remove that markup.
Iteration summaries retain their narrower reference cleanup. This is not yet
complete post-tool answer recovery.
