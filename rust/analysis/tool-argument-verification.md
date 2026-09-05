# Native tool argument validation

The native executor no longer calls a tool with an empty object after supplied
arguments fail JSON decoding. Invalid JSON and decoded non-object arguments
produce the exact error JSON from `agent.tool_executor._parse_tool_arguments`,
paired with the original call ID. The tool is not invoked. Missing/null wire
argument fields still normalize to an empty object, matching the chat transport.

21 fixtures execute Python's actual argument guard after that transport default.
Each fixture runs through the native parser and tool loop with an invocation
counter. Valid objects execute with the expected decoded values; invalid inputs
execute zero times, emit an unsuccessful finish event, and reach the next model
step as the expected paired error result. This includes array/scalar/null JSON,
trailing commas, incomplete objects, unescaped control characters and duplicate
keys. Workspace: 1,187 tests passed, one existing bridge test ignored.

Scope: this ports execution rejection, not the full historical argument-string
repair pipeline. The native replay fallback for malformed JSON is still "{}";
Python's provider-bound repair can recover some malformed strings instead.
Non-finite Python JSON numbers are outside serde_json's value representation.
Native progress events still use their existing start/finish path for rejected
calls; Python's terminal middleware hooks and incremental persistence remain to
integrate. Missing function-name recovery is also separate.

Invalid-name follow-up: unknown names now use the Python conversation loop's
error formatter. Blank names receive its terse XML/JSON-data recovery message
without the tool catalog. Other unknown names receive the sorted available
names. Name validation precedes the argument guard, matching the reference loop.
40 source-executed cases cover whitespace, malformed-looking names and sorted
catalogs. A loop test checks blank-name precedence over bad arguments, the error
result's call ID, unsuccessful completion, and subsequent sentinel-name repair.
Workspace: 1,189 tests passed, one existing bridge test ignored. Name auto-repair,
all-invalid retry counting/termination, and mixed-batch middleware are still
pending, so this is not full invalid-call recovery parity.

The later [tool-name recovery port](tool-name-repair-verification.md) now adds
name auto-repair. Retry counting and middleware integration remain.

Normalization follow-up: the native parser now applies the conversation loop's
pre-validation coercions. Dict/list values use Python-style JSON (insertion
order, spaced separators and ASCII escapes), blank strings become "{}", and
other scalar values use Python str spelling. Supplied nonblank text stays intact.
21 new fixtures execute that actual source loop, including nested data, control
characters, astral Unicode and float spelling. The 21 execution fixtures now
execute the same normalization loop before the guard; blank strings and native
objects correctly execute, while non-object decoded values remain rejected.
Workspace: 1,192 tests passed, one existing bridge test ignored.

The parser normalizes eagerly, while Python performs this stage after name
validation. The exact raw-argument identity on all-invalid-name retry paths still
needs the wider response/loop construction refactor. Whole-batch malformed-JSON
retry and truncation recovery are still pending; per-call rejection currently
handles those native failures. Non-finite values and lone Python surrogates are
not representable in serde_json's Value interface.

Malformed-batch follow-up: syntax errors now follow the loop-level retry policy.
The first two non-truncated failures retry without changing messages or invoking
any tool. The third failure adds the assistant call batch and paired recovery
results; valid siblings receive Python's skipped-call text. Errors in unknown
names within a mixed batch do not poison valid calls. Apparently truncated
arguments exit immediately with the reference truncation response, bypassing the
summary path. Valid JSON of a non-object type still reaches the executor guard.

Raw malformed strings now survive in the internal assistant replay, enabling
classification. The outgoing copy retains the previous empty-object fallback
until the full historical JSON-string repair pipeline is ported. An inline batch
regression uses tools that panic on invocation and proves retry history stability,
third-attempt pairing, and zero sibling execution. Source-executed argument cases
now classify syntax and truncation as well as the executor result.

Workspace: 1,193 tests passed, one existing bridge test ignored. The recovery
error envelope follows Python, but its parser detail currently comes from
serde_json rather than CPython JSONDecodeError. Full parser-detail parity,
non-finite JSON values, cleanup hooks and partial-result persistence remain.
