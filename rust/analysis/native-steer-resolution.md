# Native conversation steering resolution

Status: implemented and validated on 2026-09-12.

## Result

The Rust gateway now owns explicit `/steer` behavior on push adapters and the
synchronous HTTP `/message` surface. The command is authorized before it can
touch a live turn. A valid busy steer is queued without cancelling provider or
tool work, while an idle steer is stripped to its payload and admitted as a
normal user turn. Busy and idle empty forms return the exact Python usage text.

Each admitted route has one generation-scoped `TurnControl` with a mutex-backed
steer buffer. Multiple submissions append with one newline in arrival order,
preview slicing counts Unicode scalar values, and `/stop` discards guidance for
the aborted generation. Natural completion atomically retires the registration
and returns any leftover guidance.

## Tool-boundary delivery

The native tool loop drains guidance only onto a tool result created by the
current turn. It never inserts a synthetic user row and never edits an older
conversation-prefix row. This preserves message-role pairing and the immutable
prompt prefix.

The post-batch drain targets the final tool result after history maintenance.
A second drain immediately before the next provider call catches guidance that
arrived after that boundary. String content receives the exact Python marker as
a suffix. Structured content receives a final text block, including Python's
different leading-newline behavior at post-batch and pre-API boundaries.

If persistence refuses the amendment, the in-memory row stays unchanged and
the drained guidance is restored behind any newer steer that arrived during
the write attempt. The provider never sees a marker that durable replay cannot
reproduce.

## Durable compare-and-swap

`SessionDb::amend_native_tool_tail` is a narrow content-only mutation. Its short
`Immediate` transaction verifies the optional lineage turn lease, a live
session, identical before/after tool identity, a nonempty stable tool-call ID,
and the newest active row's exact previous content. The guarded update changes
only `messages.content`; row identity, timestamps, counters, tool metadata, and
session activity remain unchanged. Existing FTS update triggers reindex the
replacement atomically. Stale, non-tail, ended-session, wrong-lease, and wider
field rewrites fail closed.

## Turn completion by surface

Push delivery sends the current answer and completes post-persist finalization
before releasing turn ownership. A leftover steer is then converted into a
clean message with no copied media, content parts, or message ID and is run as
the next ordinary turn.

Synchronous HTTP cannot launch an unseen follow-up after returning its current
response. `MessageResponse` therefore exposes optional `pending_steer` only
when guidance remains. The field is omitted from ordinary responses, keeping
the existing JSON shape stable for all other calls.

## Evidence

AGY produced a 57-case source-executed Python corpus covering normalization,
FIFO accumulation, drain and stop behavior, string and structured markers,
pre-API injection, turn-finalizer leftovers, and exact acknowledgement bytes.
Claude independently audited the Rust persistence and ownership seam. Their
artifacts are [native-steer-contract-agy.md](native-steer-contract-agy.md),
[native-steer-goldens.json](../tools/native-steer-goldens.json), and
[native-steer-persistence-claude.md](native-steer-persistence-claude.md).

Rust tests cover both ingress surfaces, late push promotion, stop precedence,
post-batch and pre-API drains, structured content, restoration after a failed
write, the SQLite compare-and-swap guards, FTS reindexing, rollback, and a real
local HTTP model plus blocking tool proving persistence before the next model
request.

The Python gateway's implicit plain-text `busy_input_mode=steer`, pre-admission
startup-sentinel fallback queue, and general busy-message FIFO remain separate
gateway work. This checkpoint begins once the route's turn-control registration
exists and does not claim the earlier startup interval.
