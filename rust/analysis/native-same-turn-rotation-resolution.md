# Native same-turn rotation resolution

## Outcome

Rotation-mode full compression now commits inside a live native tool turn. A
completed tool batch is durable on the parent before publication, the next
provider request reads the compacted child transcript, and every later tool row,
assistant reply, usage write, memory callback, hook event, and route lookup uses
the child identity.

The frozen conversation client is moved to the child cache key instead of being
rebuilt. Its system prompt, ordered tool schema, plugin snapshot, provider route,
extension host, and compression counter therefore remain stable across the
physical session boundary.

## Implemented seam

`TurnSession` is the single mutable physical-session authority for an admitted
logical turn. It owns the current `SessionEntry`, the committed session lineage,
and the process-local transcript lease. Its publication method delegates the
SQLite plus routing transaction to `SessionStore::publish_compression`, advances
the shared identity only after that commit succeeds, and aliases the held lease
mutex to the child.

HTTP and push ingress clone the same `TurnSession` into the agent task. After the
model loop ends, they read its current ID before persisting the final assistant
reply and before post-persist finalization. The durable turn lease needs no
physical rebind because SQLite resolves every compression child back to the same
lineage root.

`ConversationAgent` installs itself as the compression observer for its frozen
inner client. A successful boundary notification moves the exact initialized
client cell from parent to child. If a post-commit observer fails, the physical
rotation remains committed and the stale cell is retired. Finalization searches
the committed lineage from newest to oldest, which also covers multiple
rotations followed by a later notification failure without leaking an
intermediate cache entry.

The native tool loop now reads the shared session identity before each durable
tool write and maintenance pass. Rotation and in-place modes share the same
checkpoint, summary, anti-growth, cooldown, breaker, durable adoption, and
post-compression usage-sentinel logic. Only their publication branch differs.

## Additional correction

The old external-memory capture removed the original history by a fixed length.
That length becomes invalid when compression replaces the prefix with a shorter
handoff, causing tool calls and results to disappear from `turn_complete`.
Current-turn capture now anchors on the latest matching user payload and uses the
old prefix length only as a fallback.

The first full workspace run also caught an eager `Option::take()` regression.
A tuple pattern evaluated the take even when no `TurnSession` existed, releasing
the uncoordinated HTTP and push lease before the detached turn finished. The
transfer is now nested under the presence check, and both cancellation ownership
tests pass again.

## Proof

The live integration test uses the public HTTP endpoint, real SQLite storage, a
local model server, a real Python extension host, a temporary memory provider,
and a real user hook. It runs both in-place and rotation modes. The rotation case
performs one tool call on the parent, compresses, performs a second tool call on
the child, returns a final answer, and then sends another HTTP turn.

It proves:

- summary I/O sees the first durable tool result before publication;
- the parent closes with `end_reason = 'compression'`;
- the route points to a distinct child before the next provider request;
- the compacted request omits archived source detail and retains the frozen
  system prompt bytes;
- the second tool row and final assistant reply are written to the child, never
  the parent;
- main usage for the rotating turn is attributed to the child;
- memory and hook callbacks receive the exact parent and child IDs;
- external memory receives the post-rotation tool trace under the child ID;
- the following HTTP turn reuses the same frozen client without another factory
  build.

Focused tests additionally prove exact-snapshot rejection leaves the turn on its
parent, two consecutive rotations alias the same held process-local lease, and a
failure after the second cache boundary releases the client from the first child
key during finalization.

## Helper split and review disposition

AGY ran once behind the repository auth lock and owned only the authoritative
Python state-transition contract. Claude independently owned only the Rust seam
and stale-reference map. The lanes did not duplicate work. The primary lane
verified both reports against source, designed and integrated `TurnSession`,
added the live and failure tests, fixed the memory-capture and cancellation
issues, and owns final validation and publication.

Claude recommended putting route publication, lease rebinding, cache
notification, and retirement behind one authority. The implementation keeps the
transaction and lease rebind in `TurnSession`, while cache notification remains
an async observer call immediately after commit because the publication method
is deliberately synchronous and performs no network I/O. Parent retirement is
already encoded by `ConversationAgent` rekey or release behavior, so a second
retirement call was unnecessary.

AGY confirmed that the durable lease is lineage-rooted, callback failures after
publication are nonfatal, main-turn completion belongs to the child, and the
next provider request must wait for the child transcript and identity to become
visible. Those requirements are all covered by the production path and live
test.
