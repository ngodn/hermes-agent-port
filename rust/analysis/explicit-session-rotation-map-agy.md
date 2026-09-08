# Explicit session rotation source audit

Gemini reviewed the live Python and Rust trees through
`rust/tools/agy.sh`. This is the retained source-grounded finding set used for
the native session-rotation checkpoint. The raw temporary output was not added
to the repository.

## Python contract

- `/reset` canonicalizes to `/new` before access checks. `/compact` maps to
  `/compress`, and `/sessions` maps to `/resume`.
- `/new` snapshots the predecessor, tears down its cached agent, rotates the
  stable route to a new session ID, promotes the old SQLite row to
  `session_reset`, and creates the child with both `parent_session_id` and
  `model_config._reset_from`.
- `/resume` is a soft switch with session listing, title resolution,
  compression-tip walking, and an ownership check that prevents cross-user or
  cross-chat transcript access.
- `/compress` performs a real auxiliary-model summary and transcript rewrite.
  It is not a slash prompt sent to the main model and cannot be represented by
  truncating history.

## Rust finding

The Rust store and conversation cache already had most reset primitives, but
raw `force_new` did not record an explicit predecessor boundary. More
importantly, both HTTP and push ingress resolved a session before waiting for
its transcript lease.

That ordering permits this race:

1. A normal turn resolves predecessor S1 and pauses before lease acquisition.
2. `/new` rotates the route to S2 and retires S1.
3. The normal turn later acquires the old S1 lease and writes to an ended
   transcript.

Checking only inside the reset handler is insufficient. Normal admission must
also re-check the route after it owns the resolved transcript lease. A stable
route admission lock is the smallest complete defense because automatic reset
policy can also rotate a route during the resolve-to-lease window.

## Recommended checkpoint boundary

- Implement `/new` and `/reset` with a dedicated compare-and-swap store
  transition and hard retirement of the predecessor conversation client.
- Keep the existing session-ID transcript lease because it serializes distinct
  routing keys that alias one transcript.
- Add a separate stable-route admission lease around resolution through
  transcript-lease acquisition, then verify the observed route before history
  reads.
- Recognize `/resume`, `/sessions`, `/compress`, and `/compact`, but return an
  explicit native-backend availability message until their complete security
  and transcript-rewrite machinery is ported. Never forward these lifecycle
  commands to the model.
- Test SQLite lineage, conversation-generation bumping, empty-route creation,
  stale-route admission, in-flight reset ordering, HTTP behavior, and push
  delivery.

