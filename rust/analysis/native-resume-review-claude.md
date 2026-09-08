# Review: native `/resume` and `/sessions` checkpoint

Scope: the uncommitted working-tree diff on `rust-rewrite` touching
`slash.rs`, `session_commands.rs`, `session_store.rs`, `session_entry.rs`,
`session_routing.rs`, `session_db.rs`, `dispatch.rs`, `message.rs`. Read against
the Python reference (`gateway/slash_commands.py`, `hermes_state*.py`,
`hermes_cli/session_listing.py`) and the prior design map
(`native-resume-map-claude.md`). No files were edited, no Cargo ran, nothing was
committed.

Verdict: the machinery is well built and safe. The lease/CAS/transaction seam is
correct and the IDOR gate holds. But the checkpoint ships a command that is inert
for real users, because it depends on session titles that no production code path
ever writes. That is the one blocking issue. Everything else is a required
polish item or an acknowledged deferral.

---

## Blocking

### B1. The feature is dead in production: nothing ever writes a title

The list and picker are title-gated. `list_titled_sessions`
(`session_db.rs`) filters `s.title IS NOT NULL AND TRIM(s.title) != ''`, and the
bare-list and numeric-index paths in `resume_session`
(`session_commands.rs`) both go through it. `resolve_session_target`
(`session_db.rs`) only reaches its title branches when a title exists.

The only writers of `sessions.title` / `title_source` in the whole crate are
four test-only raw SQL `UPDATE`s (`dispatch.rs:1154`, `message.rs:1057`,
`message.rs:1119`, `message.rs:1432`). There is no `/title`, `/new <title>` is
still discarded (`session_commands.rs:167` from the prior checkpoint), and no
auto-title task exists. So on a real deployment:

- `/sessions` and bare `/resume` always reply "No named sessions found."
- `/resume <N>` is always out of range (the list is empty).
- `/resume <title>` never matches (no titles exist).
- Only `/resume <exact-session-id>` works, and the id is shown nowhere.

The design map (`native-resume-map-claude.md` sections 0, 3, 9) called this out
in advance and chose the opposite approach on purpose: drop the title
dependency, list by `preview` + recency like Python's `/sessions full`, and show
the id so `/resume <id>` is reachable. This checkpoint took the title-faithful
route instead but without any title source, so it delivers a command that only
ever says "nothing here."

This is safe, it is just not useful. Two acceptable resolutions:

1. Land the checkpoint but make the default listing preview + recency based
   (drop the `title IS NOT NULL` filter, or add a `full`-style fallback that
   lists untitled rows), and print the short session id in the list rows and
   footer so direct resume is discoverable. This matches the map and makes the
   command work today.
2. Keep the title-faithful behavior but pull `/title` (and ideally auto-title)
   into the same checkpoint so titles actually get set. Larger, and `/title` is
   listed as explicitly deferred, so this is the less likely choice.

Until one of these lands, merging this gives users a visible command that never
does anything. Also worth noting: the list format
`{index}. **{title}**{suffix}` (`session_commands.rs`) omits the session id
that Python includes (`hermes_cli/session_listing.py:99-135`,
`N. **title** ...  -  \`id\`  -  _preview_`), so even the direct-id escape hatch is
undiscoverable.

---

## Required fixes (non-blocking, should land with the checkpoint)

### R1. Preview picks the newest user message, Python picks the first

`list_titled_sessions` selects the preview with
`... AND m.role = 'user' ... ORDER BY m.id DESC LIMIT 1` (`session_db.rs`), i.e.
the most recent user message. Python's `preview` is the first 60 chars of the
first user message (`hermes_state.py:12032-12035`, map section 1.2). The row test
`resume_catalog_resolves_ids_titles_lineage_and_exact_lane`
(`session_db.rs`) only inserts one message so it cannot catch the direction.
Switch to `ORDER BY m.id ASC` to match Python, and the truncation is 40 chars
here (`preview.chars().take(40)`) versus Python's 60; align it.

### R2. `resumed` count uses user messages, Python uses transcript message count

The reply count comes from `user_message_count` (`session_db.rs`, role='user'
only), so "Resumed session X (N messages)" reports user turns, not total
messages. Python's `resumed_*` count is the loaded transcript length (map
section 1.3, 1.7). Minor, but it is a visible number that will read low. Confirm
which count Python actually emits and match it.

### R3. `/sessions <target> --cross-room` is not stripped

In the `from_sessions` branch of `parse_resume_request`
(`session_commands.rs`), `allow_cross_room` is hardcoded `false` and
`--cross-room` is not filtered out of `target_parts`, so
`/sessions foo --cross-room` folds `--cross-room` into the target name. Python
routes `/sessions <target>` through `/resume`, which does parse `--cross-room`
(map section 1.1). Low impact because cross-room is admin-only, but the token
should at least be stripped so it does not corrupt the target.

---

## Security and IDOR: holds, with notes

The ownership gate is correct for the own-route case and profile isolation is
solid.

- `resume_target_allowed` (`session_commands.rs`) is applied twice: once before
  the loop and again inside the loop after the transcript leases and
  `route_matches` recheck. It compares `session_key`, platform, `chat_id`, and
  `thread_id`, and fails closed when the row is absent. Good.
- The database handle is always `database_for_key(&route_key)`
  (`session_commands.rs`, `session_store.rs`), so `resolve_session_target`,
  `get_compression_tip`, `get_session`, and the switch all operate inside the
  caller's own profile DB. Cross-profile enumeration or resume is unreachable.
  The HTTP test proves the cross-chat block (`message.rs`, "Foreign Work").
- `resolve_session_target` (`session_db.rs`) escapes `\`, `%`, `_` before the
  `LIKE ... ESCAPE '\'`, so title lookups cannot inject wildcards. Good.

Notes, not blocking:

- N1 (info leak, matches Python): a blocked resume replies "belongs to a
  different user or chat" (`session_commands.rs`), which confirms that a title or
  id exists in the profile to a non-owner. The map's R4 suggested collapsing this
  into `not_found` so ids cannot be probed. Python itself surfaces a distinct
  `blocked_not_owner` (map section 1.5), so this matches Python and is a
  deliberate divergence from the stricter map recommendation. Leave it, but know
  it is an enumeration oracle scoped to one profile.
- N2 (scope creep, verify it is intended): admin cross-scope (`--all` /
  `--cross-room`) is actually implemented here via `is_explicit_admin`
  (`slash.rs`) and the `explicit_admin_override` early `return Ok(true)` in
  `resume_target_allowed`. The map listed admin cross-scope as deferred. The
  bypass is gated on `policy.enabled && policy.is_admin` (`slash.rs:122`), which
  is closed by default, and it is still bounded to the caller's own profile DB,
  so it is safe. But it is an untested auth-bypass branch (no test exercises
  `explicit_admin_override == true`). Either add a test or drop the branch to
  match the deferral. Do not ship a live admin-bypass path with zero coverage.
- N3: `resolve_session_target` searches the whole profile (no `session_key`
  filter) and runs before the auth gate. That is fine because the auth gate is
  always applied afterward, but it means the pre-loop `get_compression_tip` runs
  on a possibly-foreign id. Read-only and same DB, so harmless.

---

## Transaction atomicity, leases, cache identity: correct

These are the strongest parts of the checkpoint.

- `switch_gateway_session` (`session_db.rs`) does the whole durable transition in
  one `Immediate` transaction: existence check, outgoing boundary +
  `bump_conversation_generation`, target reopen, `_reset_from` backfill,
  compression-lineage peer repoint, and routing upsert, then one `commit`. The
  outgoing UPDATE is guarded by an end-reason allowlist so it will not clobber a
  session already ended for another reason. The rollback test
  `resume_transaction_rolls_back_every_durable_change` (`session_db.rs`) injects a
  trigger abort on the routing insert and asserts every change reverts including
  the generation row. Solid.
- `switch_session` (`session_store.rs`) CAS-checks `same_instance(expected)` and
  the session id under the index lock before writing, publishes the durable row,
  then advances the in-memory writer via `accept_database_commit`
  (`session_routing.rs`) only after the DB commit. Correct ordering: SQLite is
  authoritative, memory follows.
- The orchestrator (`session_commands.rs`) mirrors `reset_session`'s discipline:
  route lease on `session_key`, observe `expected`, sort+dedup the
  `{outgoing, target}` transcript ids and lease them in a stable order (deadlock
  safe), recheck `route_matches`, re-auth, switch, retry up to 32 times on lost
  CAS. The current-session short circuit returns `already_on` before any write.
- Cache identity is handled correctly by doing nothing: no `retire_conversation`
  on the switched-away session, relying on the `(home, session_id)` cache key
  (map section 6). The HTTP test proves the resumed route rebuilds against the
  target and sees its full prior history (`message.rs`, the `(first_id, 2,
  "third")` turn assertion and the `second_history` "persisted" check). This is
  the key behavioral guard and it is present.

One low concern: `switch_session` holds the index mutex across the blocking
`switch_gateway_session` SQLite transaction (`session_store.rs`). `reset_session`
uses the same pattern, so this is consistent, but it does serialize all route
mutations behind one DB write under a lock. Acceptable for now; note it.

---

## Parser and wiring

- `slash.rs`: `command_name` no longer collapses `sessions` into `resume`, and
  `native_command` maps both `"resume" | "sessions"` to `NativeSlashCommand::Resume
  { raw_args, from_sessions }`. `from_sessions` correctly distinguishes the two
  grammars. `command_args` exists (`slash.rs:68`). One side effect worth checking:
  because `/sessions` now canonicalizes to `"sessions"` rather than `"resume"`,
  any slash access policy keyed by canonical name now treats them as two commands.
  Python registers them separately too, so this is more faithful, but confirm the
  default policy does not gate `sessions` differently from `resume`.
- `parse_resume_request` empty-search handling is safe: `search` as the last token
  produces `parts[index+1..]` which is a valid empty slice (no panic), yields
  `Some("")`, and the handler returns the usage string. Good. The unterminated
  quote case returns `Err` and the handler replies with a parse hint.
- HTTP (`message.rs`) and push (`dispatch.rs`) both add the Resume arm before the
  Reset arm, both handle the missing-store case, and both call the identical
  orchestrator with their own lease registries. The duplicated-handler trap the
  map warned about is avoided. `unavailable_lifecycle_commands_and_resume_without_a_store_skip_the_agent`
  (`message.rs`) confirms `/compress` stays unavailable and `/resume`//`/sessions`
  no longer reach the model.

---

## Test coverage: good, with gaps

Present and meaningful: store commit + reopen + CAS-miss
(`explicit_resume_commits_route_boundary_and_reopen_together`), full transaction
rollback, catalog resolution (id, numbered-title lineage, exact-lane), parse
rules, HTTP list + cross-chat block + numeric resume + `already_on` +
resumed-history + in-flight serialization race, and the push rotate-without-forward
test.

Gaps to close, in priority order:

1. No test for the production reality of B1: an empty title table yielding "No
   named sessions found" from a real handler. This is the single most likely user
   experience and it is untested.
2. No test for `switch_session` losing the CAS on a stale `expected` (the store
   test only covers a missing target returning `None`, not a stale observed entry
   forcing a retry). The map listed this as test 2.
3. No test for the admin override branch (N2), which is the only auth-bypass path.
4. No test for the non-admin `--all` downgrade note (`scope_note`).
5. No test for resuming back into a switched-away (now `session_switch`-ended)
   session, which is where the "no retirement" assumption could bite if the
   outgoing agent stayed warm.

---

## Explicitly deferred (correctly out of scope)

Confirmed absent and acknowledged as deferred, consistent with the task and map:
`/title` and any title writer, auto-title generation, `/sessions full` (returns
"Full session listing is not available yet"), `/sessions search` (returns
"Session search is not available yet"), Matrix room scoping, and adapter buttons
or interactive pickers. The full `resolve_resume_session_id` parent-walk with
reset/branch/delegate exclusion is also deferred; only the compression-tip half
(`get_compression_tip`) is wired, which is sufficient for id/number resume where
the user names a concrete session.

One caveat on the deferrals: shipping the title-gated UI (this checkpoint) while
`/title` and auto-title are deferred is exactly what produces B1. The deferral is
fine; gating the whole command on the deferred piece is not.

---

## Bottom line

Fix B1 (make the listing work without a title writer, and show the session id) so
the command does something for real users, and close the R1-R3 fidelity gaps.
Add the admin-override test or drop that branch (N2). The transaction, lease, CAS,
and cache-identity design is correct and does not need rework.
