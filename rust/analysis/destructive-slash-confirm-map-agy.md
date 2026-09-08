# Destructive slash confirmation source audit

This report records the source-grounded findings produced by `agy.sh` for the
native `/new` and `/reset` confirmation checkpoint. The implementation was
checked against the Python gateway, its confirmation registry, slash policy,
config writer, and the existing Rust reset path.

## Required behavior

- The gate is `approvals.destructive_slash_confirm` and defaults to enabled.
- The gate is read from the active config at command time. An Always Approve
  choice must affect the next command without a gateway restart.
- Pending confirmations are process-local, keyed by the stable route, expire
  after 300 seconds, and are replaced when a newer destructive command is
  registered for the same route.
- Registration happens before the prompt or future buttons are presented.
- A recognized answer removes the pending record under the lock before the
  asynchronous destructive action begins. This is the exactly-once fence for
  retries and double clicks.
- Tool execution approvals take precedence over slash confirmation replies.
- Unrelated fresh text falls through to ordinary handling. Unrelated text only
  clears a pending confirmation after it has expired.
- The prompt must not retain a route or transcript lease while it waits. The
  actual reset acquires the existing reset/admission fences only after approval.
- Once runs the action, Cancel leaves the conversation unchanged, and Always
  writes `approvals.destructive_slash_confirm: false` before running the action.
- A failed preference write does not cancel an already approved reset. The
  response instead reports that the prompt will appear again.
- The config write must preserve unrelated YAML layout and comments, publish
  atomically, flush the file, and leave private permissions.

## Exact text parsing nuance

The Python interceptor uses both `MessageEvent.get_command()` and a normalized
raw-text fallback. The command parser only recognizes slash-prefixed commands.
Consequently `/yes`, `/ok`, `/confirm`, `/remember`, and `/deny` work, while
bare `yes`, `ok`, `confirm`, `remember`, and `deny` do not. Bare `approve`,
`approve once`, `once`, `always`, `always approve`, `cancel`, `nevermind`, and
`no` are accepted by the raw fallback. The Rust parser follows the code rather
than the broader wording in the helper's initial test suggestion.

## Main hazards

1. Holding admission leases while waiting would block the route for up to five
   minutes. Pending intent must stay outside the session admission critical
   section.
2. Using the startup config snapshot would make Always Approve ineffective
   until restart. The gate needs a live read.
3. Resolving after awaiting the reset would permit duplicate execution. The
   record must be popped first.
4. A shared-channel reply must pass the same slash-access policy as the command
   that created the pending confirmation.
5. A reset racing an active model turn must serialize through the already
   implemented route and transcript leases, then retire the predecessor client.

## Verification requested

The audit called for parser and timeout tables, supersession and exactly-once
tests, live config reload, comment-preserving persistence, HTTP and push prompt
flows, Cancel, Always Approve, and a reset raced against an in-flight turn. It
also identified adapter buttons, `/undo`, and `/clear` as later work because
those native command and adapter callback surfaces do not exist yet.
