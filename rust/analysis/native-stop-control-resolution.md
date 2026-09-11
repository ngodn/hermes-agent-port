# Native cooperative stop control

## Outcome

The live Rust gateway now handles `/stop` as route-scoped control input on both
push adapters and synchronous HTTP `/message`. A busy stop returns the exact
Python acknowledgement, interrupts the active turn, suppresses its stale
reply, marks post-turn finalization unsuccessful, releases turn ownership, and
lets the next message enter normally. An idle stop returns
`No active task to stop.` without calling the model.

The command is checked by the existing slash authorization path. It never
enters the transcript, provider request, system prompt, tool schema, or frozen
conversation state.

## Runtime ownership and races

`TurnControlRegistry` owns one cancellation handle per operational route key.
Every admitted turn registers a generation-stamped handle after acquiring its
transcript lease. Registration replacement cancels the predecessor, and stale
registration drops cannot remove a newer generation.

Stop and natural completion linearize under the same registry mutex. If stop
wins, completion observes cancellation and suppresses the old reply. If
completion wins, the registration is removed first and a later `/stop` reports
the route as idle. The registry is shared by `AppState` and the push
`Dispatcher`, so HTTP and platform ingress address the same active turn.

Backends that do not implement cooperative control are canceled by dropping
their turn future. The subprocess backend enables kill-on-drop so cancellation
also terminates its child. `NativeAgentClient` opts into cooperative control
and shares the turn handle with every frozen route clone.

## Native interruption boundaries

The native chat-completions path observes stop:

- before every main request
- while waiting for response headers
- while reading buffered error or success bodies
- while consuming a streaming response
- during ordinary retry and empty-response backoff
- while a tool call is executing

A canceled provider request is not replayed. A canceled tool batch persists a
paired failed tool result for the current call and every unstarted sibling,
then persists a terminal assistant row containing `Operation interrupted.`.
This preserves tool-call pairing and strict role alternation for the next turn.
The gateway still suppresses the interrupted turn's delivery because `/stop`
owns the user-visible acknowledgement.

## Behavioral proof

Public Rust tests prove:

- active and idle stop outcomes, route isolation, replacement safety, and the
  natural-completion race
- a 30-second retry backoff wakes promptly and makes no second provider request
- a silent provider stream wakes promptly and makes no replacement request
- the current tool is canceled, sibling calls are skipped, every call receives
  a durable paired result, and an assistant terminal closes the sequence
- push and HTTP ingress emit the exact busy acknowledgement, suppress an
  already-streamed stale partial, release ownership, and accept the next turn
- `/stop` is classified as native lifecycle control instead of model text

AGY independently inspected and exercised the Python `/stop` and `/steer`
contract. Claude separately mapped Rust task ownership, route identity,
provider waits, tool boundaries, and persistence. The two helper lanes were
kept independent. Their shared conclusion was to ship `/stop` first and leave
`/steer` for its own durable tail-mutation checkpoint.

## Scope boundary

This checkpoint covers active and idle `/stop` behavior on the live Rust push
and HTTP message paths. `/steer` is deliberately separate because it must
update the already-persisted last tool-result row and retain a late steer as
the next user turn without breaking prompt caching or role alternation.

Python-only startup-sentinel acknowledgements, sibling-thread discovery,
relay routing, background-process-wide emergency stop, and other unported
ingress surfaces are not claimed here.

## Verification

- Full Rust workspace: 1,974 passed, two expected ignores
- Focused Python interrupt and gateway-stop reference suite: 15 passed
- Focused Rust stop suite: ten passed
- Rust formatting, workspace Clippy with warnings denied, and diff hygiene:
  passed

## Progress

The capability inventory moves Gateway from 67% to 68% and native agent core
from 86% to 87%. Tool/RPC and state/search estimates are unchanged. With the
stable weights:

`0.35 * 68 + 0.30 * 25 + 0.15 * 76 + 0.20 * 87 = 60.10`

The refreshed estimate is **60.10%, reported as about 60%**, with a judgment
range of 57% to 63%. This is a production capability inventory, not a file,
line, commit, or test-count ratio.
