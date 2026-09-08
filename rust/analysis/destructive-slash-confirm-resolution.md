# Destructive slash confirmation resolution

This checkpoint implements the shared text-fallback confirmation primitive for
native `/new` and `/reset` across HTTP and push ingress.

## Accepted findings

- A route-scoped `SlashConfirmations` owner is shared by both ingress paths.
- Pending records carry monotonic confirmation ids for the future button path,
  replace older records on the same route, expire after 300 seconds, and are
  removed before reset I/O begins.
- The gate reads the active config on every destructive command and defaults to
  enabled using Python truthiness.
- The confirmation interceptor runs before normal slash dispatch, checks the
  original command through the existing slash-access policy, and exposes the
  tool-approval precedence input even though native blocking tool approvals are
  not ported yet.
- Prompt registration holds no route or transcript lease. Approved reset uses
  the existing compare-and-swap and lease-backed reset path.
- Always Approve writes the preference before reset, preserves YAML comments
  and layout, atomically replaces the target, keeps a config symlink intact,
  and enforces mode `0600` on Unix.
- Automatic session rotation clears pending confirmation state for that stable
  route so an old reply cannot target a successor conversation.
- Confirmed handler failures use the Python-compatible error response and are
  still consumed exactly once.

## Corrected or rejected findings

- The AGY parser table described some bare aliases as accepted. Direct source
  inspection shows that `get_command()` requires a slash prefix, so only the
  raw fallback's explicit bare phrases are accepted. Tests encode the actual
  Python behavior.
- The AGY suggestion to bind a confirmation strictly to its initiating sender
  would be a security-policy change. Python checks whether the replying user is
  allowed to run the original command for that route. Rust preserves that same
  policy instead of inventing a stronger ownership rule.
- Claude suggested that durable comment-preserving config writing could be
  deferred behind an in-process override. It is included now because the
  Python contract persists the choice, the port plan names live config writing
  as part of this seam, and a native lossless atomic implementation is covered
  by real filesystem tests.
- Adapter buttons remain deferred. Confirmation ids and an exact-id resolver
  preserve the state-machine seam, but the current native adapter trait has no
  outbound confirmation or inbound callback protocol.
- `/undo` and `/clear` remain deferred until their destructive operations are
  native. The prompt and preference notes retain the Python wording.

## Cache and concurrency disposition

The prompt adds no conversation turn and does not rebuild the immutable model
prompt or toolset. No model client is created for a prompt, Cancel, or Always
preference write. The reset itself remains serialized behind any in-flight turn
and retires the predecessor only after the route transition is durable.

## Validation

- Full Rust workspace: 1,537 passed, two ignored.
- Focused Rust reset suite: 32 passed.
- Python destructive-confirmation oracle: 22 passed.
- `cargo fmt --all -- --check`: passed.
- Clippy across the workspace and all targets with warnings denied: passed.
- `git diff --check`: passed.
