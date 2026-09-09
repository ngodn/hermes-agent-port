# Native compression-hook resolution

Status: settled and wired into production native conversations on 2026-09-09.

## Scope

This checkpoint activates the existing Rust `HookRegistry` for the generic
`session:compress` lifecycle event. It covers manual compression, automatic
pre-turn compression, and same-turn full compression because all three already
converge on `AgentClient::notify_compression_boundary` after SQLite publication.

It does not adopt Python context engines or the process-global relay session
coordinator. Those components do not own the native Rust transcript or relay
scope today, so notifying standalone instances would manufacture false state.

## Settled event contract

The native event uses the exact five-key Python payload:

- `platform` is the conversation platform captured during client construction.
- `session_id` is the active ID after publication.
- `old_session_id` is the archived parent for rotation and the empty string for
  in-place compression.
- `in_place` reports the publication mode.
- `compression_count` is a one-based, conversation-local counter shared by all
  clones of the frozen client and retained across cache rekeys.

The memory provider notification is awaited first. The hook task is then
scheduled even if that memory transport returned an error, and the memory error
continues through the existing stale-client retirement path. Hook execution is
fire-and-forget, so a user handler cannot delay or roll back the committed
boundary.

Rust deliberately emits only for a durably committed boundary. The Python
source currently falls through to its generic callback after a caught
publication rollback and has already incremented its in-memory counter. No
shipped subscriber depends on that false-positive behavior. Preserving the
project's durable-observer invariant is safer than copying the apparent bug.

## Hook runtime and compatibility

Each native conversation discovers its selected profile's hook manifests once
and keeps that handler set beside the immutable prompt, tool list, plugin
snapshot, and extension host. The handlers do not enter provider requests or
system-prompt bytes.

Every handler subprocess receives a profile-scoped environment. The launcher
clears ambient values, restores only process-global names accepted by
`secret_scope::is_global_env`, installs the prepared selected-profile map, and
forces `HERMES_HOME` last. This prevents another multiplexed profile's secrets
or home identity from leaking into a hook.

Canonical Python `handler.py` files run through
`python -m hermes_cli.rust_hook_runner <handler-path> <event-type>`. The runner
loads the exact file under an isolated module name, registers it before import
for annotation and Pydantic compatibility, requires `handle(event_type,
context)`, supports synchronous and asynchronous results, and cleans the module
registration on every exit. JSON context arrives on stdin. Non-`None` return
values become compact JSON on stdout for collection-style events.

Executable, shell, and JavaScript handlers retain the compiled runtime's prior
launch modes. Every child has a 30 second ceiling, kill-on-drop protection, and
its own process group. Timeout cleanup terminates descendants as well as the
direct handler. Nonzero exits and malformed collected JSON are isolated per
handler, logged without output contents, and do not suppress later handlers.

## Runtime proof

The live same-turn test combines a local model server, real SQLite state, the
persistent Python extension child, a temporary memory provider, and an async
function-only Python hook. It proves that the memory callback has completed
before the hook runs and checks the exact in-place payload.

A separate production-construction test discovers a hook from the selected
profile home, emits through the client built by `build_conversation_client`, and
rechecks the frozen system prompt byte-for-byte on restart. A focused payload
test covers both in-place and rotation IDs plus count sharing across client
clones. Registry tests cover exact-before-wildcard order, profile isolation,
forced home identity, Python runner dispatch, nonzero and malformed-output
isolation, and descendant cleanup on timeout. The Python suite covers 26 runner
success and failure cases, including a real `python -m` process.

## Helper and review disposition

AGY stayed single-flight behind the repository auth lock. Its first lane owned
only the Python runner and focused tests. Its later review covered only the
Python hook ABI. Claude's first lane owned only Rust registry hardening. Its
later review covered only Rust process and lifecycle safety. The primary lane
owned event construction, production wiring, integration tests, process-tree
cleanup, malformed-output handling, documentation, full validation, commit,
and publication.

Both reviews found no blocking flaw. Claude's note that executable
`handler.py` files without `handle()` now fail is accepted as the canonical
Python contract, not a regression in a previously live Rust path: the Rust
registry was dormant, while `gateway/hooks.py` has always required a top-level
handler function. Its memory-error and cache-rekey test suggestions remain
nonblocking because the event block is structurally outside the memory result
branch and existing cache tests already prove exact client transfer. AGY's
claim of fully identical count semantics is narrowed by the documented Python
rollback false positive above.

## Validation

- Full Rust workspace: 1,720 passed, two ignored.
- Expanded Python hook, compression-boundary, session-switch, and memory
  compatibility suite: 157 passed.
- Focused live tests cover production discovery and same-turn post-commit
  delivery through a real Python handler.
- Rust formatting, Ruff lint and format checks, Clippy with warnings denied,
  and diff hygiene pass.

## Rejected paths and remaining work

- Routing this event through the extension host was rejected. Generic user
  hooks are independent subprocess extensions and do not need live memory or
  plugin-manager state.
- Calling a context-engine boundary endpoint alone was rejected. The host does
  not yet instantiate or drive the selected context engine through prompt,
  compression, tool, usage, and turn lifecycles.
- Calling Python's relay coordinator from a conversation child was rejected.
  It is process-global and owns no native Rust relay scope.
- Copying Python's rollback emission was rejected because observers must never
  be told that an uncommitted transcript boundary exists.
- Rebuilding the hook registry or prompt after every compression was rejected.
  Rotation retains and rekeys the same frozen conversation client.
- Repeated-compression warning delivery, the other lifecycle hook events,
  complete context-engine adoption, native relay lifecycle, transparent
  extension-host recovery, and native plugin and memory managers remain.
