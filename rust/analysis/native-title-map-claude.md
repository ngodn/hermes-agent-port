# Native `/title` and `/new <title>` -  independent contract and minimal Rust design

Checkpoint under review: the native gateway `/title` command (set/show) plus
title persistence for `/new <title>`. This is a pre-implementation map: `/title`
is not wired in Rust today, and `/new <title>` is a stub. Everything below is
read from the working tree. No files were edited; no Cargo, no git.

## 0. Current Rust state (what already exists vs. what is missing)

- The `sessions` table **already carries** `title TEXT`, `title_source TEXT`,
  and `hidden INTEGER NOT NULL DEFAULT 0` as additive columns
  (`session_db.rs:1356-1358`). The base `CREATE TABLE` (`:1551`) omits them; the
  migration loop adds them if absent.
- The index is **non-unique**: `CREATE INDEX ... idx_sessions_title ON
  sessions(title)` (`session_db.rs:1567`). Python instead has a **partial
  UNIQUE** index `idx_sessions_title_unique ON sessions(title) WHERE title IS
  NOT NULL` (`hermes_state_schema.py:1611-1612`). This gap matters (§4).
- Read paths exist and already assume uniqueness: `get_session_title`
  (`session_db.rs:1221`), `resolve_session_target` (direct id → newest `#N`
  lineage → exact title, wildcard-escaped, `:1234-1265`),
  `list_resume_sessions` (title-filtered, `:1268-1295`).
- **No production writer of `title` exists.** Every writer in the crate is a
  test-only raw `UPDATE` (`dispatch.rs:1154`, `message.rs:1069/1147/1548`).
- `/new <title>` parses into `NativeSlashCommand::Reset { title }`
  (`slash.rs:79-84`) and is threaded through `reset_or_confirm` →
  `reset_session` (`session_commands.rs:591-690`), but the title is **discarded**
  with a literal stub reply: `"\n\nSession titles are not available in the
  native gateway yet."` (`session_commands.rs:682-684`).
- `/title` is **not** classified as a native command: `slash.rs:76-92`
  (`native_command`) has arms for `new`, `compress`, `resume`/`sessions` only.
  So `/title ...` falls through to the model as ordinary text -  it spends an
  agent turn and enters the transcript (prompt bytes), which is the exact
  failure the native-command boundary exists to prevent. Meanwhile the relay
  manifest already advertises `title` ("Set or show the session title",
  `relay_command_manifest.rs:150-152`), so the manifest and behavior disagree.

Net: the store surface is ~90% present (columns + readers). The checkpoint is a
**writer + one new native-command arm**, not a schema build-out.

## 1. Python behavioral contract (source-cited)

### 1.1 `sanitize_title` -  the single cleanup/length gate
`SessionDB.sanitize_title` (`hermes_state.py:11013-11060`), `MAX_TITLE_LENGTH =
100` (`:10976`). Order of operations:

1. Falsy input (`None`/`""`) → returns `None` (no raise).
2. Scrub lone surrogates via `_sanitize_surrogates` (SQLite cannot bind them).
3. Strip ASCII control chars `[\x00-\x08\x0b\x0c\x0e-\x1f\x7f]` -  note `\t \n
   \r` are **kept** here on purpose.
4. Strip problematic Unicode: zero-width `​-‏`, line/para sep +
   directional overrides ` -‮`, isolates `⁠-⁩`, `﻿`,
   `￼`, `￹-￻`.
5. Collapse `\s+` → single space, then `.strip()` (this is where the kept
   `\t\n\r` become spaces).
6. Empty after cleaning → `None`.
7. `len(cleaned) > 100` → **raise `ValueError("Title too long (N chars, max
   100)")`**. Length is measured in Python `str` code points, post-clean.

So the two distinct "bad" outcomes are: **empty-after-clean → `None`** (caller
turns this into a specific message) and **too-long → raises** (caller turns the
raised text into a warning). These are not interchangeable.

### 1.2 `/title` handler -  `_handle_title_command` (`slash_commands.py:5139-5211`)
1. `get_or_create_session(source)` → `session_id` (never mints via `force_new`).
2. If no `_session_db` → `format_session_db_unavailable(...)`.
3. **Lazy-create in SQLite:** `get_session_title(session_id)`; if it returns
   `None`, call `create_session(...)` persisting `source, user_id, chat_id,
   chat_type, thread_id` (the IDOR-scoping origin), swallowing errors
   ("session might already exist"). Note `None` here means *either* untitled
   *or* not-yet-persisted -  both are treated the same (attempt create).
4. `title_arg = args.strip()`.
  - **Set path** (`title_arg` non-empty):
    - `sanitize_title(title_arg)`; on `ValueError` → `warn_passthrough`
       (`"⚠️ {error}"`), i.e. the too-long message, and returns.
    - sanitized is empty/`None` → `gateway.title.empty_after_clean`
       (`"⚠️ Title is empty after cleanup. Please use printable characters."`).
    - `set_session_title(session_id, sanitized)`:
      - returns `True` → schedule Telegram topic rename (best-effort, off-lane
         no-op), reply `gateway.title.set_to` (`"✏️ Session title set:
         **{title}**"`).
      - returns `False` (row vanished) → `gateway.title.not_found`.
      - raises `ValueError` (duplicate title) → `warn_passthrough` (`"⚠️
         {error}"`) carrying `"Title '{t}' is already in use by session {id}"`.
  - **Show path** (empty args): re-read `get_session_title`.
    - titled → `gateway.title.current_with_title` (`"📌 Session:
       \`{session_id}\`\nTitle: **{title}**"`).
    - untitled → `gateway.title.current_no_title` (`"📌 Session:
       \`{session_id}\`\nNo title set. Usage: \`/title My Session Name\`"`).

### 1.3 `/new <title>` -  reset block (`slash_commands.py:301-322`)
After the new session is created (`get_or_create_session(force_new=True)`):
- `_title_arg = args.strip()`. Only acts if `_title_arg and _session_db and
  new_entry`.
- `sanitize_title(_title_arg)`:
 - `ValueError` (too long) → `_title_note = gateway.reset.title_rejected`
    (`"\n⚠️ Title rejected: {error}"`); title left unset.
 - sanitized non-empty → `set_session_title(new_entry.session_id, sanitized)`:
   - success → **header replaced** with `gateway.reset.header_titled` (`"✨ New
      session started: {title}"`).
   - `ValueError` (duplicate) → `_title_note = gateway.reset.title_error_untitled`
      (`"\n⚠️ {error} -  session started untitled."`). Header stays the default
      reset header. Confirmed by `test_reset_command_duplicate_title_surfaces_warning`
      (`test_title_command.py:160-220`): reply contains "already in use" and
      "session started untitled", and does **not** contain "New session started:
      Dup".
   - any other exception → swallowed (`pass`), session stays untitled, header
      unchanged.
 - sanitized empty (whitespace/unprintable only) → `_title_note =
    gateway.reset.title_empty_untitled` (`"\n⚠️ Title is empty after cleanup -
    session started untitled."`).
- Final `header = header + _title_note`. The note is appended, so on the
  titled-success path there is no note; on failure paths the default/`new`
  header carries the warning tail.

Key asymmetry vs `/title`: on `/new`, a bad title **never fails the reset** -
the session is always created; the title is best-effort. On `/title`, a duplicate
or too-long title is surfaced as the whole reply and nothing is stored.

### 1.4 `set_session_title` / `_set_session_title` -  the write CAS
(`hermes_state.py:11207-11220`, internal `:11097-11206`.)
- `set_session_title` always uses `source = TITLE_SOURCE_USER`.
- Inside one write transaction (`_execute_write`):
  1. `SELECT title, title_source, hidden WHERE id = ?`. Missing row → return
     `0` (→ `False`, → `/title` "not_found"; `/new` swallows).
  2. **Canonical Bot Chat guard:** if stored `title == "Bot Chat"` and `hidden`
     and the new title differs → a `user` write **raises `ValueError`**
     ("its name is its identity…"); an auto write no-ops. `CANONICAL_BOT_CHAT_TITLE
     = "Bot Chat"` (`:10996`). Rust already knows this constant
     (`bot_mode.rs:8`) and has the `hidden` column, so it is in scope.
  3. Provenance precedence (auto only): `user` always lands; `derived`/`llm`
     land only if untitled or strictly lower stored rank. Rank
     derived<llm<user, NULL source treated as `user` (`_title_rank`,
     `:10999-11010`). For **this checkpoint only the `user` path is needed**;
     see §5.
  4. **Uniqueness:** `SELECT id WHERE title = ? AND id != ?`. Conflict →
     `ValueError("Title '{t}' is already in use by session {conflict_id}")`,
     *unless* the conflicting holder is a compression ancestor of this session
     (`_is_compression_ancestor`), in which case the title is transferred
     (ancestor set to NULL). Compression is deferred in the native gateway
     (`/compress` is `Unavailable`, `slash.rs:85-87`), so the transfer branch is
     out of scope -  but the plain conflict raise is in scope.
  5. **CAS update:** `UPDATE sessions SET title=?, title_source=? WHERE id=? AND
     title IS ? AND title_source IS ?` bound to the exact values read in step 1
     (NULL-safe `IS`). A concurrent write between SELECT and UPDATE loses (row
     count 0) rather than clobbering. `title_source` is set to `source` when
     title is non-empty, else `NULL`.
 - Returns `rowcount > 0`.

### 1.5 Uniqueness is enforced twice in Python
Both the app-level `SELECT ... WHERE title = ?` check **and** the DB partial
UNIQUE index (`hermes_state_schema.py:1611`). The index is the backstop against
a race the app check misses.

## 2. Cross-cutting invariants (from the task focus list)

- **User-visible replies:** enumerated in §1.2/§1.3 with exact i18n keys and the
  `en.yaml` text (`locales/en.yaml:267-270, 374-378, 474`). Rust currently
  hard-codes English reset replies (`session_commands.rs:680-684`), so matching
  the English strings verbatim is the bar; full i18n is a separate concern.
- **Title cleanup / max length:** §1.1. One shared `sanitize_title`. 100 code
  points. Empty-after-clean and too-long are distinct outcomes.
- **Duplicate-title error:** §1.4 step 4. `/title` surfaces it as the reply;
  `/new` degrades to untitled with a warning tail.
- **`title_source` precedence:** §1.4 step 3. Only `user` writes happen this
  checkpoint; still persist `title_source='user'` so the deferred auto-titler
  never overwrites a user title.
- **Empty / pre-first-message sessions:** `/title` lazily persists the row
  (§1.2 step 3) before writing, using `get_or_create_session` (no `force_new`).
  A brand-new session that only exists in the store is created in SQLite first.
  `/new <title>` writes against the freshly-minted `force_new` session.
- **Atomicity / races:** the read+guard+uniqueness+write are one transaction
  with a value-matched CAS (§1.4 step 5). A `/title` racing an in-flight auto
  title (deferred) or a second `/title` cannot silently clobber. In Rust this
  must be one `rusqlite` transaction on the pooled connection, not two calls.
- **Reset confirmation parsing:** unchanged. Title rides on
  `NativeSlashCommand::Reset { title }` through `reset_or_confirm` →
  `register_reset` → on confirm, `approved.title` is replayed into
  `reset_session` (`session_commands.rs:562, 603, 606`). The pending-confirm
  path already carries the title; only the terminal `reset_session` body needs
  to consume it instead of stubbing. Verify the destructive-confirm replay
  preserves the exact title bytes (it stores `Option<String>` already).
- **HTTP and push handling:** both ingress sites must reach the same handler.
  `Reset { title }` is handled at `message.rs:196` (HTTP) and `dispatch.rs:363`
  (push) -  both already call `reset_or_confirm`, so fixing `reset_session` fixes
  both. `/title` needs a new arm wired at *both* sites, mirroring how Resume is
  dual-wired (the resume resolution calls out the "duplicated-handler trap").
- **Cached clients / prompt bytes:** a title is **metadata only**. It is never
  part of the system prompt, the transcript, or the conversation cache key
  (`(profile home, session_id)`, per `native-resume-resolution.md`). Setting or
  changing a title must **not** touch the cached client, must not bump the
  conversation generation, and must not add a transcript message. This is the
  strongest invariant: `/title` is a pure sidecar write. The current
  fall-through-to-model behavior *violates* it (a `/title` today would inject
  bytes and spend a turn); wiring the native arm is what restores it.

## 3. Ingress differences to respect

- HTTP `/message` returns a single `reply` string (`message.rs:191-193`); push
  returns via the dispatch reply path (`dispatch.rs`). Both already do for Reset.
- Slack rewrites `/` to `!` (`slash.rs:typed_command_prefix`). `/title` gating
  goes through the same `evaluate`/`can_run_command` policy as any command; no
  special access rule -  `title` is a low-privilege own-session command like
  `status`. It should **not** require admin (contrast `/resume --all`).
- The Telegram-topic rename side effect (`slash_commands.py:5181-5198`) is a
  best-effort adapter concern. Native gateway has no Telegram topic-rename seam
  wired; treat the rename as **deferred adapter work** (§5), but the DB write and
  the reply must not depend on it (Python swallows its failure).

## 4. Minimal Rust design

Goal: the narrowest change that makes `/title` and `/new <title>` behave like
Python, honoring the sidecar invariant, with no new schema build-out.

### 4.1 Store surface (session_db.rs)
Add one writer next to the existing readers:

```
pub enum SetTitleError { NotFound, TooLong(usize), Duplicate{ id: String }, BotChat }
pub fn set_session_title_user(&self, session_id: &str, title: &str)
   -> rusqlite::Result<Result<bool, SetTitleError>>
```
- Port `sanitize_title` as a free fn `sanitize_title(&str) -> Result<Option<String>,
  TooLong>` in a small module (it is pure; unit-test it in isolation against the
  Python regex classes and the 100-cap). Reuse the crate's existing
  surrogate-scrub helper if one exists (the codec paths already handle
  surrogates); otherwise scrub `\u{d800}-\u{dfff}` -  though Rust `String` cannot
  hold lone surrogates, so this is a no-op in practice and can be noted rather
  than implemented.
- One transaction: `BEGIN`; `SELECT title, title_source, hidden`; bot-chat
  guard; uniqueness `SELECT id WHERE title=? AND id!=?`; CAS `UPDATE ... WHERE id=?
  AND title IS ? AND title_source IS ?` binding read values; commit. Mirror
  `_set_session_title` exactly. Skip the compression-ancestor transfer branch
  (deferred) -  on any conflict, return `Duplicate`.
- Persist `title_source = 'user'`.
- **Schema:** either (a) leave the non-unique index and rely on the app-level
  check (matches "app enforces it" but drops Python's DB backstop), or (b) add
  the partial UNIQUE index to match Python and get defense-in-depth. Recommend
  (b) as a one-line migration, but flag that an existing store with duplicate
  titles (only possible via the test-only raw UPDATEs today) would fail index
  creation; since no production writer has ever run, real stores have ≤1 titled
  row per title and the migration is safe. Decide explicitly; do not leave the
  resolver assuming uniqueness the schema doesn't guarantee.

### 4.2 Lazy-create for `/title` on a pre-persisted session
`/title` must ensure the row exists before writing (§1.2 step 3). Reuse the
same create-if-missing path the Reset/admission flow already uses to persist a
session's origin columns (`chat_id, chat_type, thread_id, user_id, source`) so a
later `/resume` of this titled row passes the IDOR predicate
(`session_commands.rs:resume_target_allowed`). Do not invent a second create
path -  route through the store's existing session-create so origin scoping stays
identical to admitted turns.

### 4.3 Command wiring
1. `slash.rs::native_command`: add a `"title"` arm →
   `NativeSlashCommand::Title { arg: Option<String> }` (`command_args`,
   empty→None). Keep it out of `BUILTIN_COMMANDS`.
2. `message.rs` (HTTP) and `dispatch.rs` (push): add a `Title` handler that
   resolves the session (`get_or_create` semantics, no `force_new`), lazily
   persists, then set-or-show. Factor the body into
   `session_commands::title_command(...)` so both ingress sites call one
   function (avoid the duplicated-handler trap).
3. `session_commands.rs::reset_session`: replace the stub (`:682-684`) with:
   after the reset succeeds and `title` is `Some`, `sanitize` → on `TooLong`
   append `title_rejected` note; on empty append `title_empty_untitled`; on
   success call `set_session_title_user` → on `Ok(true)` **replace** header with
   `header_titled`; on `Duplicate` append `title_error_untitled` and keep the
   default header; on `NotFound`/other, keep untitled silently (Python swallows).
   Preserve the append-vs-replace header rule exactly (§1.3).

### 4.4 Reply strings
Match `en.yaml` English verbatim (§1.2/§1.3 text). Keep the emoji and the
markdown (`**{title}**`, backtick `session_id`). These are assertable substrings
the Python tests already check ("already in use", "session started untitled",
"New session started: {title}" negative).

## 5. Concrete tests

Unit (pure):
- `sanitize_title`: trims; collapses internal `\t\n\r`+spaces to single space;
  strips a zero-width/RTL-override sample and an ASCII control byte; empty and
  whitespace-only → `None`; a 100-cp string passes, 101 → `TooLong(101)`;
  multi-byte (emoji/CJK) counted in code points not bytes (a 100-emoji title
  passes, 101 fails). Golden against Python for a shared fixture set (reuse the
  oracle pattern used elsewhere in `rust/tools`).
- `set_session_title_user` transaction: set on fresh row; idempotent re-set of
  same title (row count 0 via CAS but returns - match Python: re-setting the exact
  same title/source yields rowcount 0 → `Ok(false)`; note this and assert the
  chosen semantics); duplicate on a *different* session → `Duplicate{id}`;
  bot-chat hidden row rename → `BotChat`; missing id → `NotFound`.
- Uniqueness/CAS race: two concurrent `set_session_title_user` to the same new
  title on two different sessions -  exactly one wins, the other gets `Duplicate`
  (or the UNIQUE index rejects it); prove no double-titling.

HTTP + push integration (mirror the resume suite's dual coverage):
- `/title Foo` on a pre-first-message session: row lazily created, reply
  `set_to`, `get_session_title == "Foo"`, **no transcript message added, no
  model turn, conversation generation unchanged, cached client untouched** (the
  sidecar invariant -  assert the turn counter / generation did not move).
- `/title` (no arg) titled vs untitled → `current_with_title` /
  `current_no_title` with the right `session_id`.
- `/title <101 chars>` → `warn_passthrough` too-long text; nothing stored.
- `/title <only control/zero-width chars>` → `empty_after_clean`; nothing stored.
- `/title Dup` when another owned session holds "Dup" → reply contains "already
  in use"; original title unchanged.
- `/new Project X` → header `header_titled` with the title; `get_session_title`
  of the new session == "Project X".
- `/new Dup` (duplicate) → reset still succeeds, reply contains "already in use"
  and "session started untitled", and does **not** contain "New session started:
  Dup" (direct port of `test_reset_command_duplicate_title_surfaces_warning`).
- `/new <101 chars>` → reset succeeds, `title_rejected` tail, session untitled.
- Reset **confirmation** path: `/new Project X` that triggers the destructive
  confirm, then confirm → the replayed reset applies title "Project X" (title
  survives `register_reset`/`approved.title`).
- `/title` is authorized by the ordinary slash policy (a normal user in their
  own chat can run it; no admin requirement), unlike `/resume --all`.

High-value Python tests to port as the oracle: `tests/gateway/test_title_command.py`
(conflict, control-char sanitize, show-vs-set topic-rename gating, reset
duplicate warning, `/new [name]` help), `tests/hermes_state/test_canonical_title_guard.py`
(bot-chat rename refusal), and the `sanitize_title`/`_set_session_title` cases in
`tests/test_hermes_state.py`.

## 6. Explicitly deferred (not this checkpoint)

- **Auto-titling:** `derived`/`llm` sources, `agent/title_generator.py`
  (`generate_title` LLM task + `derive_title` heuristic), `set_auto_title` /
  `set_auto_title_if_empty`, and the first-turn hook in
  `agent/turn_context.py`. The writer should persist `title_source='user'` now so
  the later auto path's precedence logic composes correctly, but the auto path
  and its rank/no-op branch are out of scope.
- **Compression:** the `_is_compression_ancestor` title-transfer branch in
  `_set_session_title`, `get_next_title_in_lineage`, and
  `set_session_title_source` (carry-across-boundary). Native `/compress` is still
  `Unavailable`, so none of this is reachable. On any uniqueness conflict, raise
  `Duplicate` (no transfer).
- **Adapter topic rename:** `_schedule_telegram_topic_title_rename`
  (`slash_commands.py:5181-5198`) and the Telegram-forum topic-name propagation.
  Best-effort, adapter-side; the DB write and reply must not depend on it. Defer
  as adapter work; keep a seam.
- **UI / desktop:** `sessionTitle` sidebar fallback, exports, pickers
  (`hermes_state_common.py:33-34`) -  no native gateway surface.
- **Provenance-carry and legacy NULL-source handling** beyond writing `'user'`:
  `set_session_title_source`, `_title_rank(None)==user`, backfill -  only relevant
  once auto-titling or compression rotation lands.

## 7. Blocking vs. non-blocking for this checkpoint

- **Blocking:** (1) `/title` is unwired → it leaks into the model turn and
  transcript, violating the sidecar invariant while the relay manifest already
  advertises it. (2) `/new <title>` silently drops the title. Both are the
  checkpoint's reason to exist.
- **Should-fix:** the non-unique `idx_sessions_title` vs Python's partial UNIQUE
  index. The resolver already assumes uniqueness; either add the partial UNIQUE
  index or accept app-level-only enforcement, but decide it, don't inherit it by
  omission.
- **Watch:** dual-wire `/title` at both HTTP and push through one shared handler;
  preserve the append-vs-replace header rule on `/new`; keep the write in a
  single transaction with the value-matched CAS; do not bump generation or touch
  the cached client.
