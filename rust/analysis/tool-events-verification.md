# Native tool event correlation

The native tool loop now supplies the decoded object arguments in start events,
assigns monotonically increasing per-turn indexes, and measures actual execution
duration for completion events. Start and finish share an index even when tool
execution fails. The counter survives later model iterations and resets for a
new turn. Timing starts after sending the start event, excluding channel
backpressure, and uses Instant to avoid clock adjustments.

The contract is documented in gateway/stream_events.py and mirrored by
hermes-core/src/stream.rs. The change applies to the native tool loop. The
Python bridge mapping still uses its older index placeholders and needs its
own invocation identity tracking when that bridge protocol is extended.

## Validation

An inline scripted-model test runs two tool calls, including one failure, then
a third call in a later iteration. It checks arguments, indexes, completion
status, measured duration and resetting across two turns. It failed before the
change because arguments were absent; all start/finish indexes and timings
were previously zero. A short real synchronous tool delay provides a lower
bound on execution time without relying on a maximum scheduling latency.

Workspace: 1,144 tests passed, one existing bridge test ignored. Clippy with
warnings denied and formatting pass. Logs: tool-events-before.log,
tool-events-tests.log and tool-events-clippy.log.

## Next tool-runtime contracts

Python make_tool_result_message in agent/tool_dispatch_helpers.py adds name,
tool_name, normalized call IDs, timestamps, untrusted-content framing, upstream
elision notices and risk metadata. The native result currently carries only
role, call ID and content. Port the constructor and its real helper dependencies
before connecting browser/MCP tools; do not approximate framing or mutate old
results after they enter the conversation. Durable tool history is also pending.
