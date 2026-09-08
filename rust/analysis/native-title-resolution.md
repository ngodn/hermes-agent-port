# Native title checkpoint resolution

## Implemented

- `/title` is a native HTTP and push gateway command. Bare use displays the
  current immutable session ID and title; an argument writes a manual title.
- A title-only cold route creates its SQLite session row before validation or
  reply, preserving later ownership checks even when the requested title is
  invalid.
- The shared sanitizer removes the Python control ranges, collapses whitespace,
  counts Unicode scalar values, and enforces the 100-character limit.
- Manual writes use `title_source=user` and one `BEGIN IMMEDIATE` transaction
  for the hidden canonical Bot Chat guard, exact uniqueness lookup,
  compression-ancestor transfer, and NULL-safe compare-and-swap update.
- A partial unique index provides the cross-process backstop. Opening an old
  Rust database repairs duplicates by retaining the newest row, removes the
  obsolete non-unique index, and waits up to five seconds for SQLite writers.
- `/new <title>` carries raw title bytes through confirmation, sanitizes only
  after the new session exists, and either sets the title or clearly reports
  that the new session is untitled.
- Title traffic never enters the transcript, invokes the model, changes the
  durable conversation generation, rebuilds the immutable prompt, or evicts the
  session-ID-keyed conversation client.

## Review disposition

- Fixed: safe display-name lookup, obsolete index cleanup, five-second SQLite
  busy timeout, reset-warning newline parity, cache assertion, held-turn title
  serialization, and reset-title validation coverage.
- Rejected: a constraint race between the title conflict read and update. Both
  occur after `BEGIN IMMEDIATE`, so another SQLite writer cannot commit in that
  interval. The concurrent claim test verifies the resulting one-winner rule.
- Retained deliberately: honest warnings for unexpected title persistence
  failures. Python currently hides those failures and can falsely claim a title
  was saved.
- Retained deliberately: sanitizer enforcement inside the database API even
  though command handlers sanitize once for precise feedback.

## Deferred

Automatic derived and LLM titles, provenance upgrades, Telegram topic rename,
desktop and web title state, and the native compression command remain separate
checkpoints. The small compression-ancestor title transfer is present now so a
Rust process remains compatible with Python-created continuation lineages.
