# Explicit session rotation implementation resolution

## Implemented

- Slash aliases canonicalize before access policy evaluation.
- `/new` and `/reset` execute as native gateway commands on HTTP and push
  ingress and never consume a model turn.
- `SessionStore::reset_session` publishes with an observed-entry
  compare-and-swap fence. It sets `is_fresh_reset`, preserves source/display
  identity, promotes the predecessor to `session_reset`, bumps the durable
  conversation generation, and records child lineage in SQLite.
- A reset on an empty route creates exactly one session and no phantom parent.
- Explicit reset retires the old conversation client through the same deferred
  session-end path already used by automatic reset and expiry.
- HTTP and push now share complete `SessionSource` construction.
- A dedicated route admission lease spans routing resolution through transcript
  lease acquisition. The observed route is verified before history is read.
  Stale waiters release the old lease and resolve again, so they cannot append
  to a closed predecessor.
- `/compress`, `/compact`, `/resume`, and `/sessions` return clear availability
  replies and cannot fall through to the model.

## Review disposition

The Claude audit correctly identified the missing explicit reset seam and the
need to serialize rotation with in-flight turns. Its suggestion that a request
already resolved to the predecessor could safely write there after reset was
not accepted. The Gemini audit demonstrated that this would violate the active
route boundary. The implementation therefore uses both a stable-route lease
and the existing session-ID transcript lease, followed by route verification.

The destructive-confirmation user interface remains unported. Python defaults
that prompt on, including button callbacks and a text fallback with an
"Always Approve" config write. This is a visible compatibility item, but it is
separate from the reset transaction and cache correctness delivered here.

Full `/resume` remains blocked on titled-session listing and the IDOR ownership
gate. Full `/compress` remains blocked on auxiliary summarization, durable
archive/compact or child publication, and the corresponding frozen prompt and
tool snapshot transition.

