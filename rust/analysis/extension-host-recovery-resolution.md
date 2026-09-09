# Extension host recovery resolution

## Outcome

The conversation-scoped Python extension host now recovers after a transport
failure without rebuilding the Rust conversation client, system prompt, or tool
schema. The request whose outcome became ambiguous still fails. The worker
starts a fresh child, repeats only initialization, and serves later requests in
their original queue order.

Recovery is accepted only when the new child reports the exact frozen plugin
and memory capability snapshot from the first initialization. A changed memory
provider, registered plugin catalog, available plugin tools, memory tools, or
memory exposure flag kills the replacement process and leaves the host
unavailable. This preserves the provider-visible tool prefix for the lifetime
of the conversation.

The worker retains the original process launch configuration and private
initialization payload. A respawn therefore repeats environment clearing and
reuses the selected profile's secret snapshot over stdin. A successfully
acknowledged compression `session_switch` advances the recovery session ID.
Remote errors and malformed non-null switch acknowledgements do not.

## Failure and teardown policy

- Remote application errors do not restart the child.
- EOF, protocol framing errors, response-ID mismatches, oversized responses,
  I/O failures, and timeouts are fatal transport failures.
- The failed request is never replayed because it may already have changed an
  external system before the response was lost.
- One immediate recovery attempt follows a transport failure. Failed recovery
  attempts are rate-limited for five seconds.
- `flush_pending` begins teardown. A transport failure during flush,
  `session_end`, or `shutdown` never starts a new provider process.
- Every rejected or failed child is killed as a process tree.

## Proof

The local executable fixture writes a side effect and exits before replying.
The first tool request fails, the second request succeeds through a new child,
and the side-effect log proves the first request was not replayed. The recovered
child reports the compression-rebound session ID, the original profile-secret
payload, and absence of a foreign profile secret after environment clearing.

A second fixture changes its advertised tool surface on restart. Rust rejects
it and kills the replacement. A teardown fixture dies during memory flush and
proves no replacement process starts. A focused state test proves that only an
exact JSON-null switch acknowledgement updates the recovery identity. The
existing real plugin and memory-provider integration test now also proves that
a timed-out plugin call is followed by a usable recovered host.

Full workspace validation is 1,724 passed with two expected ignores (1,723
gateway plus one core test). The selected Python extension-host pre-compression
and session-switch protocol suites are 101 passed. Formatting, Clippy with
warnings denied, and `git diff --check` pass.

Claude's read-only review found no correctness or security blocker. It identified
the missing respawn-isolation and teardown-failure tests, both added before
publication. Its remaining notes concern the pre-existing memory callback
timeouts and a currently unreachable `reset=true` metadata refresh path.

## Rejected paths

- Replaying the failed tool or memory call was rejected because response loss
  cannot distinguish failure-before-side-effect from failure-after-side-effect.
- Accepting a changed tool surface after restart was rejected because it would
  mutate frozen conversation capabilities and break prompt caching.
- Rebuilding the whole conversation client was rejected because it would also
  rebuild immutable prompt and provider state.
- Recovering during teardown was rejected because provider initialization can
  itself have observable side effects.

## Terminal checkpoint preparation

AGY mapped only the authoritative Python terminal runtime contract. Claude
separately mapped only the existing Rust construction and execution seams. Both
reports agree that native terminal registration must wait for a native command
approval floor, session-scoped cwd and environment persistence, process-tree
termination, bounded output spill handling, and unambiguous ownership of the
single `terminal` tool name. A shallow local shell wrapper would bypass current
security behavior, so it was not registered in this checkpoint.

## Remaining work

The compatibility host still runs Python and does not count as a native plugin
or memory manager. Those managers, terminal approval and process runtimes,
native built-in tools, context-engine adoption, same-turn compression rotation,
and overflow recovery remain.
