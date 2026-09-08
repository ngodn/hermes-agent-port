# Native resume source map from Gemini

This is the retained result of the read-only Gemini audit run through
`rust/tools/agy.sh`. The findings were checked against the Python and Rust
sources before implementation.

## Python contract

- `/resume [name] [--all] [--cross-room]` uses shell parsing. A direct session
  ID wins, then exact title resolution, including the newest `title #N`
  continuation. Numeric targets are one-based indexes into the recent named
  session list.
- `/sessions` owns the richer listing grammar. `full` includes unnamed rows,
  `all` widens only for an explicitly configured admin, and `search` or `find`
  performs session search. A positional target delegates to `/resume`.
- A session ID or title is only a routing handle. It is not authority. Normal
  callers must match the persisted platform, chat, thread, and DM user. Legacy
  rows with insufficient identity fail closed. Cross-origin access requires an
  explicit flag from a configured admin.
- Compression parents resolve to their live continuation. Reset, branch,
  delegate, and tool children are not generic resume continuations.
- A successful switch ends the outgoing routing epoch as `session_switch`,
  reopens the target row, updates the stable peer route, and clears
  conversation-scoped control state. It does not mint a new target ID.
- Resume is not destructive and does not use the destructive slash
  confirmation flow.

## Rust seam selected

- Keep one stable route key and repoint it to an existing conversation ID.
- Serialize on the route lease, then lock both transcript IDs in sorted order.
  Recheck the observed route and target authorization before committing.
- Commit the outgoing boundary, generation bump, target reopen, compression
  ancestor peer capture, and `gateway_routing` update in one short SQLite
  transaction. Perform no network or provider work inside it.
- Publish the in-memory route only after the database commit. A failed commit
  must leave every durable and in-memory value unchanged.
- Keep the conversation cache keyed by profile home and immutable session ID.
  Changing the route naturally selects the target's cached client. Do not hard
  retire the switched-away client because it remains resumable.
- Expose direct ID, exact title, numeric named-session selection, and
  `/sessions full` so native untitled sessions remain discoverable before the
  `/title` and auto-title pipelines are ported.

## Highest risks identified

1. IDOR through a direct ID, title, or list row that is not rebound to the
   caller's persisted origin.
2. A split durable transition where the route points at the target but the
   outgoing boundary or target reopen did not commit.
3. Resume racing a live turn and moving the route before its assistant reply is
   durable.
4. Cross-route resume deadlock if two transcript locks are acquired in
   different orders.
5. Rebuilding or hard-closing the wrong per-conversation client, which would
   either lose prompt-cache warmth or run end-of-session hooks too early.

## Required proof

- Real SQLite commit and injected rollback cases.
- Same-chat and same-user visibility plus foreign chat and foreign DM user
  rejection.
- Explicit-admin widening and non-admin downgrade behavior.
- HTTP and push commands never reaching the model.
- In-flight turn ordering, current-session no-op, numeric and title lookup,
  compression-tip landing, exact wildcard escaping, and warm target reuse.

Deferred work remains `/title`, `/new <title>` persistence, automatic title
generation, `/sessions search`, the full non-compression descendant resolver,
Matrix-specific room behavior, and adapter picker UI.
