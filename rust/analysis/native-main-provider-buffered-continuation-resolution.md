# Native buffered continuation and tool truncation

Date: 2026-09-11

## Result

The ordinary native chat-completions path now handles explicit output-length
termination across both request modes. Tool-enabled buffered responses use the
same semantic continuation protocol as the existing no-tools stream: the
frozen route receives the original turn, each assistant fragment, and Python's
exact continuation prompt, with the output cap raised progressively. The user
receives one whitespace-safe stitched answer.

Successful continuation no longer collapses the stitched answer into one
durable assistant row. SQLite records the exact semantic sequence instead:

```text
user -> assistant fragment -> user continuation nudge -> assistant suffix
```

Every fragment/nudge batch is committed in one immediate transaction after a
live-session and turn-lease check. The batch must contain nonempty alternating
assistant/user pairs. Invalid input, a stale lease, an ended session, or an
invalid transcript phase writes no rows. Buffered tool turns commit these rows
before any recovered tool call can execute. Streaming commits them before its
terminal stop. Gateway persistence then stores only the final provider suffix,
while delivery still contains all fragments. This keeps later provider replay
byte-faithful without duplicating the prefix.

## Truncated tool calls

A `finish_reason="length"` response with tool calls takes a different lane.
The incomplete assistant response is discarded, the message list is unchanged,
and no tool executes. Rust retries the same request four times, for five total
attempts, with output caps `8192`, `16384`, `32768`, and `32768` from the default
4096 base. A larger caller cap remains intact.

Only the recovered response contributes usage. If all retries truncate, the
user receives `Response truncated due to output length limit` as a failed,
delivery-only outcome. When an earlier tool completed in the same turn, the
runtime first persists that terminal assistant text after the tool result. This
matches Python's interrupted-tool repair and prevents the next user message
from creating an invalid `tool -> user` boundary. A broken tool-call payload is
never written to SQLite.

## Ownership and cache safety

The existing `ChatModel::step` and native tool loop remain the single execution
engine. A continuation wrapper carries the semantic prefix into that loop, which
persists and adopts it before handling the recovered final answer or tool call.
No global retry state or second agent loop was added.

Turn-local continuation disposition uses fresh shared state for each admitted
turn. Fallback route clones share only that turn's state. The final durable
projection is keyed by physical session ID, consumed after gateway persistence,
and cleared during finalization. External-memory completion receives the same
suffix in its transcript while retaining the fully stitched reply as its
separate response value. Concurrent conversations therefore cannot exchange
suffixes or delivery-only markers.

The frozen system prompt, tool schema, and route plan do not change. Continuation
messages are genuine conversation semantics, not prompt regeneration. Provider
projection strips stored `finish_reason` metadata from the wire while retaining
it in SQLite for lifecycle fidelity.

## Independent helper split

AGY owned the Python tool-truncation behavior lane. Its source-executed generator
produces 81 cases across 11 sections, including eligibility, preemption, retry
progression, cap growth, execution prohibition, partial-stream distinctions,
dropped-tool prompts, terminal repair, accounting, and provider boundaries.

Claude first owned the separate timeout configuration and liveness lane. It
located Python's request and stale-timeout inputs, proved that Rust currently has
no main-request timeout owner, and specified the next public tests without
mixing that work into this checkpoint. Claude then performed a bounded review of
this implementation and found no high-severity defect. The primary lane fixed
its external-memory suffix and router-rewrite persistence findings before
commit; the rest are explicit deferrals or accepted fail-closed behavior.

## Verification

- Source-executed tool-truncation corpus: 81 cases across 11 sections.
- Focused Python tool-truncation and interrupted-sequence suites: 14 passed.
- Rust public-seam tests cover buffered and streaming continuation persistence,
  exact second-request history, suffix-only final persistence, five-attempt tool
  truncation, unchanged retry messages, progressive caps, router-rewritten
  zero-retry refusal, zero unsafe tool execution, successful recovery, and
  durable tool-tail repair.
- The full workspace and static checks are recorded in `PORT.md` after final
  validation.

## Deliberate limits

Thinking-only length exhaustion and its one-shot reasoning disable, repetition
rejection, dropped-stream tool-name prompts, partial-stream stub distinctions,
local Ollama GLM stop correction, and main-request inactivity deadlines remain.
Non-chat transports retain their own later port lanes.

The weighted full-port estimate is **58.15%**, still reported as about 58%.
Native agent core moves from 78% to 79%; gateway, tool/RPC, and state/search
inputs stay unchanged. This increase reflects live buffered continuation,
exact durable continuation replay in both request modes, and safe tool-call
truncation recovery, not the helper corpus by itself.
