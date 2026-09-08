# Native resume implementation resolution

The native HTTP and push gateways now handle `/resume` and `/sessions` as
control commands. They never enter the transcript or reach the model.

## Implemented boundary

- `SessionStore::switch_session` performs an identity-based route CAS and
  rebinds the stable route to an existing session ID. It never mints a target.
- `SessionDb::switch_gateway_session` uses one `BEGIN IMMEDIATE` transaction
  for the outgoing `session_switch` boundary, conversation-generation bump,
  legacy reset-child stamp, target reopen, compression-ancestor peer update,
  and routing-row upsert.
- Memory is published only after the SQLite transaction succeeds. The routing
  writer then advances its revision and writes the optional JSON mirror as a
  best-effort compatibility artifact.
- The async command path owns the route lease, then both transcript leases in
  sorted ID order. It rechecks the route and authorization after locking and
  retries a lost CAS up to the same bounded count used by turn admission.
- Frozen clients remain keyed by `(profile home, session ID)`. Resume does not
  hard-retire the outgoing client or fire end-of-session extraction. Returning
  to a still-warm session reuses its client and its immutable prompt snapshot.

## Selection and authorization

- Shell-compatible parsing supports quotes and literal outer wrappers.
- Direct session ID wins, followed by exact title and newest numbered title
  continuation. Numeric `/resume N` uses the recent named-session list.
- `/sessions full` includes native untitled sessions and prints their IDs, so
  resume remains usable before native title writers exist.
- List rows and direct targets are checked against platform, stable route,
  chat, thread, and DM user identity. The check runs before locking and again
  after both transcript leases. Same-channel rows owned by a different HTTP
  sender are neither listed nor resumable.
- Widening is allowed only when slash access is enabled, the caller is a
  configured admin, and an explicit `--all` or `--cross-room` flag is present.
  A disabled slash gate grants no data-access authority.
- Title `LIKE` wildcards are escaped. Compression parents land on their live
  compression tip.

## Helper findings resolved

Both pre-implementation maps and both post-implementation reviews were read
against the source.

- The production-usability blocker was fixed with `/sessions full`, unnamed
  preview rows, and visible session IDs. The default `/resume` list remains the
  Python-compatible named list.
- Gemini's DM IDOR finding was valid. Authorization now also requires the
  persisted `user_id` on DM routes, and list rows pass the same visibility
  predicate. A same-channel, different-user HTTP regression proves it.
- Preview selection now uses the first user message by timestamp and ID.
- Numeric selection replaces the literal number with the resolved title in
  success and already-current replies.
- `/sessions <target> --cross-room` strips and carries the flag rather than
  corrupting the target text.
- The admin override and non-admin `all` downgrade branches now have live HTTP
  and push coverage.
- Claude's claim that Python counts the whole transcript in its success reply
  was rejected after checking `gateway/slash_commands.py`: Python counts user
  messages, which is what the Rust query does.
- The cold-route placeholder noted by Gemini was retained intentionally.
  Python calls `get_or_create_session` before switching too, so the empty
  predecessor and its `session_switch` boundary preserve current behavior.
- The initial implementation hard-retired the outgoing cached client. The
  source audit showed Python performs only a soft route-cache eviction, while
  Rust's session-ID cache needs no eviction. The hard retirement was removed.

## Verification

- A real SQLite trigger abort proves the entire durable switch rolls back,
  including the generation bump.
- Real HTTP coverage proves named and full listing, numeric and title resume,
  current-target no-op, foreign-chat and foreign-user rejection, explicit-admin
  widening, old-history restoration, warm-client reuse, and model exclusion.
- A held HTTP turn proves resume waits until the outgoing assistant reply is
  persisted, then switches without moving that reply to the target transcript.
- Push coverage proves non-admin `all` downgrade, reset, title resume, and zero
  control-message model turns.
- The selected Python source contract suite passes all 35 cases.

Deferred: `/title`, `/new <title>` persistence, automatic titles,
`/sessions search`, full reset/branch/delegate-aware descendant walking, Matrix
room semantics, and adapter picker buttons.
