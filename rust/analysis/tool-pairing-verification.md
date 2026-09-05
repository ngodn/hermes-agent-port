# Native tool-call and result repair

The native send path now runs the Python sanitizer sequence: allowed-role
filtering, empty-message healing, malformed tool_calls normalization, blank-name
repair, missing result-ID removal, positional pairing, alias-aware deduplication,
and result-name alignment. Thinking-only removal follows, before wire projection.
Every change is made to a copy; persisted messages retain their original content.

A tool result must answer an outstanding call in the current positional run.
A user or assistant message closes that run and causes deterministic missing
result stubs to be inserted. Results displaced past that boundary are removed.
System/developer messages do not close the run in the current Python source;
the Rust implementation preserves this detail. Aliases include call_id, id,
response_item_id, composite IDs and their nonempty components. Answering an alias
consumes its group, while a later call can re-arm the same ID.

Fresh response batches also get Python's deterministic duplicate-ID suffixes
before execution. This is necessary alongside replay deduplication: two distinct
tool executions must not lose one result because the model reused an ID.

Verification:

- 259 fixtures execute the full Python sanitizer, its empty-message helper and
  the actual ID/name policy functions. The main agent generated these separately
  from Claude's Rust implementation. Expectations are stable across hash seeds.
- 44 fixtures execute Python's batch ID uniquifier, including composite IDs,
  existing suffix collisions and non-string inputs.
- Real HTTP requests exercise a displaced result, an inserted missing-result
  stub, result-name correction, invalid-role filtering and a later reused ID.
  The summary request preserves the already-repaired prefix.
- A native loop regression executes two clock calls with the same incoming ID
  and verifies both results survive replay repair under distinct IDs.

Boundaries: this covers JSON-shaped messages. SDK object mutation and exceptions
on invalid non-string result IDs are not native interfaces; Rust tolerates those
inputs by dropping invalid results. The Python helper's logging and repair
notices remain outside native event delivery. Full SQLite history, transport
checkpoint replay, and full response metadata construction remain pending.

Workspace verification: 1,185 tests passed, one existing bridge test ignored.
Clippy with warnings denied and formatting passed. Claude completed with
`is_error=false` on `claude-opus-4-8`; the validation reported here comes from
the main agent's Rust tests and source-executed fixtures.

Missing-ID follow-up: native response parsing no longer discards otherwise valid
calls solely because their ID is absent, blank, or non-string. It resolves the
explicit pairing field and composite ID using the assistant builder's precedence,
then falls back to `call_` plus the first 12 SHA-256 hex digits of
`name:arguments:index`. The same resolved ID is used for execution and replay.
Supplied IDs are whitespace-trimmed, matching the builder.

144 additional fixtures execute the actual tool-call block from
`build_assistant_message`, using the real split, derive, hash and uniqueness
helpers. The HTTP turn-limit fixture now starts every scenario with an ID-less
clock call and still completes all ordinary and summary requests. Workspace:
1,186 tests passed, one existing bridge test ignored. Malformed argument repair
and missing function-name recovery at execution remain separate work.

Execution-deduplication follow-up: after name and JSON validation, the loop now
retains only the first (name, canonical JSON arguments) pair in a batch. Replay
keeps that call's original argument spelling and ID. This differs from ID
uniquification: calls with different arguments still retain separate execution
and result IDs even when the provider reused an ID.

24 cases execute Python's actual duplicate-call filter, covering nested key order,
whitespace, Unicode escapes, integer/float distinctions, arrays and malformed raw
strings. A loop test verifies equivalent objects execute once and leave one
call/result pair; the reused-ID regression now uses distinct arguments and
continues to execute both calls. Workspace: 1,195 tests passed, one existing
bridge test ignored. Canonical comparison currently uses serde_json's numeric
representation; arbitrary-sized Python integers and non-finite JSON values remain
outside exact parity. Delegate-call concurrency caps remain separate work.

Delegation-cap follow-up: native client startup resolves
`delegation.max_concurrent_children`, then the existing environment fallback,
then ten. Invalid explicit config uses ten; nonpositive values clamp to one.
The tool loop caps delegate_task entries before duplicate suppression, preserving
other calls and aligned replay/results. 39 fixtures execute the Python resolver
and batch cap; a native loop regression verifies the execution and replay order.
Workspace: 1,198 tests passed, one existing bridge test ignored.

This is batch policy for registered delegation tools. It does not add the native
delegate_task engine, worker scheduling, child lifecycle or per-child budgets.
Runtime config reload and values beyond native integer representation remain
outside this slice.
