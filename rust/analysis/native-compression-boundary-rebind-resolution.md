# Native compression-boundary rebinding resolution

Status: settled and wired into production paths on 2026-09-09.

## Scope

This checkpoint closes the external-memory session-switch seam after a full
compression publication. It also keeps the byte-stable conversation client
alive when a rotating publication changes the physical SQLite session ID.

The change covers manual compression, automatic compression before a turn, and
same-turn in-place compression after a durable tool-result batch. Same-turn
rotation remains a separate deferred seam.

## Settled contract

The compression transaction is authoritative. The observer runs only after the
new transcript is durably published. Its failure is logged and cannot roll back
or hide the committed boundary.

The Python memory contract is exact:

- `new_session_id` is the published active ID.
- `parent_session_id` is the pre-compression ID.
- `reset` is `false` because compression continues the same logical
  conversation.
- `reason` is `compression`.
- `rewound=false` crosses the host protocol but is omitted from provider
  keyword arguments, matching `MemoryManager.on_session_switch`.
- In-place compression sends the same ID as both parent and child. Providers
  still need the boundary notification to discard state derived from archived
  turns.

The host updates its own session identity before calling the real
`MemoryManager`. This makes later turn callbacks use the published identity even
when no memory provider is active.

## Frozen client transfer

Rotating compression must not release and rebuild the conversation client. That
would rebuild the frozen system prompt, tools, plugin snapshot, provider route,
and extension process at an ordinary physical transcript boundary.

`ConversationAgent` therefore performs a three-step transfer:

1. Check out only an already initialized parent client and increment its pending
   count. This pins it against TTL, pressure, explicit retirement, and shutdown
   races while the observer awaits.
2. Invoke the child client's compression-boundary callback outside every cache
   lock.
3. Atomically decrement the pin and move the exact cache entry from the parent
   key to the child key after success. In-place notification keeps the original
   key.

The failure and contention rules are conservative:

- A protocol or transport failure retires the stale parent with release
  semantics. The durable child can build a fresh client on its next turn.
- If another client already occupies the child key, that client wins and the
  parent is released.
- A hard retirement requested while notification is pending follows the
  successful rekey and finalizes against the child identity exactly once.
- A missing or uninitialized cache entry is a no-op. The notification never
  creates an extension process merely to observe a boundary.

## Runtime proof

The live same-turn HTTP test starts the real Python extension child with a
temporary memory-provider plugin. It proves that a complete tool result is in
SQLite before summary I/O, the compressed transcript is committed before the
next provider request, and the provider receives the exact same-ID compression
callback with `reset=false` and no stray `rewound` keyword.

Manual and automatic rotation tests read the compression checkpoint back from
SQLite inside the observer. They also assert exact parent and child IDs. A
manual observer failure proves the committed generation survives. Cache tests
prove successful rekey without a second client build, same-ID retention,
failure retirement, and the pending hard-retirement race.

The Python host suite covers strict request validation, identity-before-callback
ordering, exact provider arguments, absent managers, provider isolation, and
JSONL dispatch. The Rust client suite covers exact request encoding, remote
errors, and strict null response decoding.

## Helper disposition

AGY ran once behind the repository auth lock and owned only the Python host
endpoint and its tests. Claude owned only the Rust client protocol. The primary
lane owned the trait, cache state machine, all caller integration, live child
proof, race test, review, documentation, validation, and publication.

One Claude draft used `reset=true` for rotation and a separate in-place reason.
Source verification rejected both. Python compression uses `reset=false` and
`reason="compression"` in both modes.

## Rejected paths and remaining work

- Releasing the parent client after every rotation was rejected because it
  breaks the per-conversation prompt-cache invariant.
- Notifying before publication was rejected because observers could adopt a
  transcript that later loses its compare-and-swap race or transaction.
- Running both helpers on the same audit or implementation was rejected as
  duplicate work. Their ownership stayed disjoint.
- Context-engine adoption, relay notifications, and the generic
  `session:compress` event are not implemented by this checkpoint.
- Transparent recovery of an extension child that dies during an internal
  same-turn notification remains part of broader extension-host recovery.
- Native plugin and external-memory managers remain larger port milestones.
