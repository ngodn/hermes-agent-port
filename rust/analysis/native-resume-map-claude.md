# Native `/resume` and `/sessions`: audit and the narrowest safe checkpoint

Independent audit. No implementation files were edited, no Cargo ran, nothing was
committed or pushed. Every Rust line reference was read from the working tree; every
Python contract was traced in the live reference tree (`gateway/`, `hermes_cli/`,
`hermes_state*.py`, `agent/`, `locales/`), not from any helper report.

Rust paths are under `rust/crates/hermes-gateway/src/`. Python paths are repo-root
originals. This builds on the already-shipped rotation checkpoint (commits
`0a6e56e0c3` native rotation, `baf6514ff9` destructive confirm) and its two maps
(`explicit-session-rotation-map-claude.md`, `explicit-session-rotation-resolution.md`),
which deferred `/resume` behind "titled-session listing and the IDOR ownership gate."
This report is that deferred milestone, scoped down to what is genuinely useful and
safe today.

---

## 0. Headline

**A useful, safe `/resume` + `/sessions` can land now, but only if it drops the one
Python dependency the port cannot honor yet: session titles.** Python's picker is a
*titled*-session list, and Rust has no title column, no `/title` command, and a
`/new <title>` that is already an explicit no-op
(`session_commands.rs:167-168`). Waiting on titles would block the whole command.

The escape is that Python's list rows already carry a **`preview`** (first 60 chars of
the first user message) and `/sessions full` already lists *untitled* sessions by that
preview (`hermes_cli/session_listing.py:83-84`, `hermes_state.py:12032-12035`). So the
narrow checkpoint is exactly that: **list the caller's own recent sessions on their own
route, labeled by preview + message count + age, numbered; `/resume <N>` (or `/resume
<id>`) switches to one.** This needs no titles, and scoping to the caller's own
`session_key` makes the IDOR gate trivially satisfied rather than a thing to get subtly
wrong.

What must be built: one soft-switch store method (`switch_session`, currently
unimplemented), one `reopen_session` DB primitive, one per-route listing query + index,
a split of the current `resume` Unavailable stub into `ResumeList` / `ResumeSelect`, and
the async orchestrator wired at **both** ingress sites. What stays deferred: titles and
auto-titling, `--all` / `--cross-room` admin cross-scope, `/sessions full|all|search`
sub-grammar, adapter buttons, and full `resolve_resume_session_id` parent-walk (the
compression-tip half is already ported).

One structural advantage over Python falls out for free (§6): because the Rust
conversation cache is keyed by `(home, session_id)` (`conversation_agent.rs:34,108-113`),
a resume that repoints the route to a different id **needs no cache eviction at all**  -
the next turn naturally keys the target's own client. Python must explicitly
`_evict_cached_agent` because its cache is keyed by `session_key`.

---

## 1. Python contracts (traced from source)

### 1.1 Registration, names, aliases, argument syntax

Two **separate** gateway commands, no aliases:

| Typed | Canonical | Aliases | args | Source |
| --- | --- | --- | --- | --- |
| `/resume` | `resume` | none | `[name] [--all] [--cross-room]`, `argument_mode="mixed"` | `hermes_cli/commands.py:250-251` |
| `/sessions` | `sessions` | none | `[list\|ls\|browse\|all\|full\|search <q>\|<target>]` | `hermes_cli/commands.py:254` |

Dispatch: `gateway/run.py:19988-19998` routes `resume → _handle_resume_command`,
`sessions → _handle_sessions_command`. `/sessions <target>` (anything not a sub-keyword)
rebuilds the event as `/resume <target>` and delegates
(`gateway/slash_commands.py:5423-5425`), so `/sessions` is a richer front-end over the
same resume engine. There is **no** `/session`, `/list`, `/switch`, or `/continue`.

**Rust divergence to note up front:** `slash.rs:53-58` canonicalizes `sessions → resume`
(one command name), so the Rust handler cannot tell `/sessions` from `/resume` by the
canonical name  -  it must re-inspect the raw text. This is fine (bare `/resume` and bare
`/sessions` both mean "list" in Python anyway) but the handler must parse the original
token, not the canonical.

`/resume` arg parse (`gateway/slash_commands.py:5221-5238`): `shlex.split`, strip `--all`
(`allow_all`) and `--cross-room` (`allow_cross_room`), join the rest as `name`, strip
surrounding brackets/quotes. Bare `/resume` = list mode.

### 1.2 `/sessions` list semantics

`_handle_sessions_command` (`gateway/slash_commands.py:5400-5480`), sub-grammar in
`hermes_cli/session_listing.py:23-42`:

- `list`/`ls`/`browse`  -  display no-op; `all`/`--all`  -  widen source scope (admin only);
  `full`/`--full`  -  include unnamed; `search`/`find <q>`  -  FTS over titles/preview;
  anything else  -  a target delegated to `/resume`.
- Rows via `list_sessions_rich` (`hermes_state.py:12009`), fields
  `id, source, model, title, started_at, ended_at, message_count, preview, last_active`
  (`hermes_state.py:12032-12035`). `preview` = first 60 chars of the first user message.
- Limit **10** shown (`:5470`); search over-fetches 50 (`:5460`); `exclude_sources=["tool"]`
  (`:5461`). Default order `started_at DESC`; search orders by effective last-active.
- **Unnamed sessions are hidden** unless `full`, a search id-match, or the current session
  (`hermes_cli/session_listing.py:83-84`). Current session marked `(current)`.
- Scoped to caller `session_key` unless admin `all` (`:5452, 5463-5469`); non-admin `all`
  gets a downgrade notice (`:5442-5446`).
- Formatting `hermes_cli/session_listing.py:99-135`:
  `N. **title** (current)? \`source\`?  -  \`id\`  -  _preview_`, footer teaches
  `/resume <id>` / `/resume <number>`.

### 1.3 `/resume <arg>` selection semantics

`_handle_resume_command` (`gateway/slash_commands.py:5211-5398`):

- **No arg → list** the recent *titled* sessions (inner `_list_titled_sessions`,
  `:5240-5248`): `list_sessions_rich(source, session_key unless admin --all, limit=10)`,
  keep only rows with a title, `[:10]`, filtered by `_resume_row_visible` (`:5254-5257`).
- **Numeric `/resume N`** (`:5290-5305`): 1-based index into that visible titled list.
  `< 1 or > len` → `gateway.resume.out_of_range`. `/resume 1` = most recent titled.
- **Non-numeric** (`:5306-5313`): try **exact session id** `get_session(name)` first, else
  **exact title / `"title #N"` lineage** `resolve_session_by_title` (`hermes_state.py:11624`).
  There is **no substring/fuzzy** match (that lives only in `/sessions search`).
- **Ambiguity is never surfaced**  -  `resolve_session_by_title` silently collapses to the
  single most-recent match (`hermes_state.py:11635-11648`).

Exact replies (`locales/en.yaml`, `gateway.resume.*`): `not_found` (295),
`out_of_range` (294), `no_named_sessions` (285), `resumed_one/many/no_count` (298-300)
`"↻ Resumed session **{title}** ({count} messages). Conversation restored."`,
`already_on` (`"📌 Already on session **{name}**."`). There is **no** "multiple matches"
reply.

### 1.4 Titles  -  are they required, where from, which field is authoritative

**Not required.** `sessions.title TEXT` is nullable with no default
(`hermes_state_common.py:457`); untitled = NULL. Provenance in `title_source TEXT` (:458),
ranked `derived(0) < llm(1) < user(2)` (`hermes_state.py:10982-10988`); a higher rank is
never overwritten by a lower-rank auto title (`_title_rank`, `:10998`). Uniqueness is a
*partial* unique index `idx_sessions_title_unique ON sessions(title) WHERE title IS NOT
NULL` with a runtime dedupe-repair (`hermes_state_schema.py:1611-1620`).

Origins: user `/title <text>` → `set_session_title` (`hermes_state.py:11207`, `user`
rank); auto LLM/heuristic → `agent/title_generator.py` (`generate_title` LLM task on the
*first user message only*, `derive_title` heuristic fallback), persisted via
`set_auto_title` (`hermes_state.py:11222`) at first turn
(`agent/turn_context.py:266-325, 1694`). **Authoritative field: `sessions.title` +
`title_source`.**

Consequence for the port: **Rust has no title anywhere.** No column
(`session_db.rs:1356-1369` + additive `:1147-1164`  -  `display_name` exists, `title` does
not), no `/title`, `/new <title>` discarded (`session_commands.rs:167-168`). So a
Python-faithful bare `/resume` list would be **empty** in Rust. The checkpoint must list
by `preview`, i.e. adopt `/sessions full` semantics, not bare `/resume` semantics.

### 1.5 Ownership / scope (IDOR) checks

Central predicate `_resume_target_allowed(source, target_id, allow_override)`
(`gateway/slash_commands.py:1117-1258`):

- Admin `--all`/`--cross-room` bypass only if `_resume_caller_is_admin`
  (`:1098-1115`, stricter than `SlashAccessPolicy.is_admin`; default config → nobody).
- Live target: same-origin-chat check (`:1135-1140`). Persisted target: same platform
  (`:1148-1149`), same thread, DM requires same `user_id` + `chat_id` (`:1218-1221`),
  non-DM requires equal non-blank `chat_id` then shared-session or same `user_id`
  (`:1227-1249`). `user_id_alt` callers (Signal/Feishu) **fail closed** on persisted rows
  (no column, `:1178, 1216-1217, 1247-1248`). Legacy NULL-owner / blank-source /
  identity-less callers **fail closed** (`:1250-1258`).
- Enforced in-handler at `:5333-5340` (`blocked_not_owner`); enumeration guarded per row
  by `_resume_row_visible` (`:1260-1282`) on every list path.

**Answer: no IDOR.** A user cannot resume or even enumerate another user's / chat's
session unless they are a configured admin using an explicit widening flag. Fail-closed on
any ambiguity. This whole apparatus is the reason the rotation resolution deferred
`/resume`; §5 shows the checkpoint sidesteps most of it by only ever listing/resolving the
caller's own route.

### 1.6 Current-session behavior

After resolving `target_id`: `if current_entry.session_id == target_id: return
gateway.resume.already_on` (`:5342-5345`), no switch. Store-level guard too
(`gateway/session.py:3804-3806`).

### 1.7 What `/resume` mutates  -  a SOFT switch, not a session end

Sequence (`gateway/slash_commands.py:5347-5398`):

1. **Compression-chain follow:** `target_id = resolve_resume_session_id(target_id)`
   (`:5318-5319`; `hermes_state.py:13920-14000`). First `get_compression_tip`, then a
   forward `parent_session_id` walk (depth 32) choosing the deepest descendant *with
   messages*, **excluding** `_reset_from` / `_branched_from` / `_delegate_from` / tool
   children so resuming a reset parent never lands on the post-reset conversation.
2. `_release_running_agent_state(session_key)` (`:5348`).
3. **`switch_session(session_key, target_id)`** (`:5351`; `gateway/session.py:3784-3852`):
   repoint the in-memory `SessionEntry` to the *existing* `target_id`, `_save`, then in
   SQLite **`promote_to_session_reset(old, "session_switch")`** (ends the outgoing session,
   bumps `conversation_generations`), **`reopen_session(target_id)`** (clears
   `ended_at`/`end_reason` so the old transcript accepts new turns), and re-record the peer
   with `include_compression_ancestors=True`. **No new id is minted; `on_session_end` never
   fires.**
4. `_clear_conversation_scope(session_key, reason="resume")` (`:5360`).
5. `_evict_cached_agent(session_key)` (`:5367`)  -  *soft* evict so the next turn rebuilds
   the `AIAgent` (whose memory provider cached `_session_id` at `initialize`) against the
   correct id. This is a Python-cache-shape artifact, not a semantic requirement (§6).
6. Load transcript for the message count only; a read error still reports success
   (`:5374-5383`).

**Prompt cache:** `/resume` does not copy or rewarm prompt cache (only `/branch` does,
`:5603-5606`).

### 1.8 Destructive confirmation / interactive callbacks

**None.** `_maybe_confirm_destructive_slash` wraps only `/new` (`gateway/run.py:19756-19770`);
`/resume`, `/sessions`, `/title`, `/branch` dispatch directly (`:19988-19998`). No
button/callback flow. So the checkpoint needs no confirm store and no adapter buttons.

### 1.9 Tests defining the contract

`tests/gateway/test_resume_command.py` (972 lines): bare-list-when-no-arg (`:83`), non-admin
`--all` downgrade (`:111`), scope-clear-for-resumed-key-only (`:150`), compression-continuation
switch (`:214`), cached-agent evict (`:249`), exact-lane scoping before limit + foreign-lane
exclusion (`:277`), numeric fallback to exact lane (`:329`), `/sessions full` legacy-child and
reset-created listing (`:417, 470`), search beyond recent-10 + no cross-user leak (`:735, 766`),
IDOR fail-closed on `user_id_alt` (`:795`), Matrix room scoping (`:925-972`). Store contracts:
`tests/hermes_state/test_resolve_resume_session_id.py` (tip/parent-walk redirect).

---

## 2. Live Rust surface (what exists, what is missing)

### 2.1 Current behavior of `/resume` and `/sessions`

Both canonicalize to `resume` (`slash.rs:53-58`) and classify as
`NativeSlashCommand::Unavailable` (`slash.rs:78-83`), delivering
*"Session resume is not available in the native gateway yet."* and **never reaching the
model** (`dispatch.rs:315-321`, `message.rs`). Pinned by
`unported_lifecycle_commands_never_reach_the_agent` (`message.rs:1304`)  -  the exact test to
flip.

### 2.2 The reset seam this checkpoint mirrors (shipped, live)

- Store CAS core `SessionStore::reset_session(source, expected)` (`session_store.rs:90-166`):
  observe → build predecessor context → `index.publish_forced_candidate(candidate, expected)`
  CAS (`:120-124`, returns `Ok(None)` on lost race) → `persist_full` →
  `promote_to_session_reset(parent,"session_reset")` (`:132`, `session_db.rs:982`) →
  `create_session` child with `parent_session_id` + `model_config._reset_from` (`:140-156`).
- Async orchestrator `session_commands::reset_session` (`session_commands.rs:97-173`):
  up to 32 iterations, **route lease** on `session_key` (`:106`) → observe `expected`
  (`:118`) → **transcript lease** on predecessor id (`:124-136`) → `store.reset_session`
  (`:141`) → on `None` drop+retry → `agent.retire_conversation(...)` hard teardown (`:152-158`).
- Admission `session_admission::admit_turn` (`session_admission.rs:28-100`): route lease →
  `get_or_create_with_legacy` → transcript lease → `route_matches` verify + retry (up to 32).
  Two `SessionTurnLeaseRegistry` (`turn_lease.rs:93`): `route_leases` (by session_key),
  `transcript_leases` (by session_id).
- Cache `ConversationAgent` (`conversation_agent.rs:68`), key `(home, session_id)`
  (`:34,108-113`), `retire_conversation` = hard `SessionEnd` (`:698-706`),
  `RetirementKind::{Release(soft), SessionEnd(hard)}` (`:36-40`).

### 2.3 What `/resume` needs and does NOT have

- **`switch_session` (soft repoint to an existing id): unimplemented.** Only a comment
  (`dispatch.rs:430`) and `session_switch`/`superseded_by_resume` end-reason strings baked
  into recovery SQL fences (`session_db.rs:384,390,414,426,947,994,1013,1479`) with **no
  writer**. `publish_forced_candidate` only mints a *new* candidate, so a route-rebind to an
  existing id is a new store method.
- **`reopen_session` (clear `ended_at`/`end_reason`): absent.** Needed so a resumed, ended
  session accepts new turns (Python's `reopen_session`, `hermes_state.py:8512`).
- **Per-route session listing: absent.** Nearest queries return a *single* row
  (`find_latest_gateway_session_for_peer` `session_db.rs:1025` `LIMIT 1`;
  `find_session_by_origin` `:1735` returns `None` on multi-match by design). No index on
  `(source, session_key)`.
- **Title: absent** (§1.4).
- **`resolve_resume_session_id`: half-present.** `get_compression_tip` (`session_db.rs:696`)
  and `get_compression_chain` (`:650`) are ported (the compression half); the full
  parent-forward walk with reset/branch/delegate/tool exclusion is not.
- **Adopt-not-invent lineage:** entry fields `suspended/resume_pending/resume_reason`
  (`session_entry.rs:175-176`) and end reasons `session_switch`,
  `resume_pending_expired`, `superseded_by_resume` are already fenced in recovery and
  reset policy but have **no writers**  -  resume can adopt them.
- Useful read primitives that already exist for building rows: `get_session` (`:1113`,
  full row JSON), `message_count` (`:1767`), `user_turn_count` (`:1520`),
  `load_lifecycle_messages` (`:1635`), `session_lineage_root_to_tip` (`:607`).

---

## 3. The design tension, stated plainly

Python's `/resume` picker is a **titled** list; `/sessions full` is the **untitled,
preview-labeled** list. The port has no titles. Therefore:

- Porting bare `/resume` faithfully ⇒ an empty list until auto-titling lands ⇒ useless.
- Porting `/sessions full` semantics (list by `preview` + recency, no title dependency)
  ⇒ genuinely useful today, and it is a strict subset of Python behavior (nothing a user
  sees would contradict Python; they would just also see untitled sessions, which
  `/sessions full` shows anyway).

So the checkpoint **is** the preview-labeled, own-route list plus a numeric/id switch.
Titles, auto-titling, and the title-based resolution path are deferred as a coherent unit,
and adding them later only *narrows* the default list (title-only) and adds a resolution
branch  -  no rework of the switch machinery.

---

## 4. Correctness risks the seam exposes (prioritized)

**R1 (high)  -  a switch must serialize against the in-flight turn on the current session
and against a concurrent turn on the target.** Like reset (rotation map R1), the slash gate
runs pre-lease (`dispatch.rs:315-321`, `message.rs`). The switch handler must take the same
route lease + transcript lease discipline as `session_commands::reset_session`, plus CAS on
the route publish, or it will repoint the route while a turn mutates one of the two
sessions. Mitigation in §5.3.

**R2 (medium)  -  repoint-to-existing-id has no store primitive, and a naive
`publish_forced_candidate` would mint a *new* id, silently discarding the target.** The new
`switch_session` must publish an entry carrying the *target's existing* `session_id`, not a
fresh candidate. This is the core new store method (§5.1).

**R3 (medium)  -  a resumed session that is DB-ended will silently reject new turns unless
reopened.** Resuming an idle/ended session must clear `ended_at`/`end_reason`
(`reopen_session`), exactly as Python does, or the next turn's persistence lands on an ended
row and recovery fences may treat it as closed (`session_db.rs:947,994` reference
`session_switch` precisely so a switched-away row stays recoverable  -  the target side needs
the inverse, reopen).

**R4 (medium)  -  IDOR by direct id.** Numeric resume into the caller's own list is safe by
construction, but `/resume <id>` lets a user name an arbitrary id. The handler must re-verify
the target row's `(source, session_key)` equals the caller's before switching, and fail
closed otherwise (Rust analog of `_resume_target_allowed`, but the *narrow* version: no
admin override, own-route only). Never resolve a title (no titles) and never widen scope.

**R5 (low)  -  landing on the wrong lineage node.** Resuming a compression parent must land on
the tip (`get_compression_tip`), and resuming must not land on a *reset* child. The
compression half is ported; the reset-exclusion parent walk is not. For the checkpoint, list
concrete session ids and resume `get_compression_tip(id)`; because the listed id is the one
the user picked (not a reset parent inferred from a title), reset-exclusion is not triggered.
Full `resolve_resume_session_id` parity is a small deferred follow-up.

---

## 5. Smallest deep native interface + exact call sites

Goal: land `/sessions` (own-route preview list) and `/resume <N|id>` (soft switch) as
production handlers on both HTTP and push, reusing the reset seam's lease/CAS discipline,
adding the minimum store surface. No new config keys, no confirm, no buttons, no titles.

### 5.1 One new store method: soft switch to an existing id

```rust
/// Soft resume: repoint the route to an EXISTING target session id.
/// Ends the outgoing session as `session_switch` (bumping the conversation
/// generation), reopens the target so it accepts new turns, and CAS-publishes
/// the route entry carrying `target_id` (not a fresh candidate). Returns None
/// on a lost CAS so the caller retries. Mirrors reset_session's fencing
/// (session_store.rs:90-166) but mints no new id and records no reset lineage.
pub fn switch_session(&self, source: &SessionSource, expected: &SessionEntry,
    target_id: &str) -> anyhow::Result<Option<ExplicitSessionSwitch>>
```

Implementation, mirroring `reset_session`:
1. `session_key_for_source` + `reconcile`.
2. If `expected.session_id == target_id` → `Ok(Some(already_on))` (current-session guard,
   §1.6). Also cheaply enforce R4 here: the caller passes the target row and this method (or
   the orchestrator) asserts `row.source == expected origin source && row.session_key ==
   session_key`; fail closed otherwise.
3. Build a candidate `SessionEntry` from `expected.origin` but with
   `session_id = target_id` (carry `display_name`, mark `is_fresh_reset = false`).
4. **CAS publish** `index.publish_forced_candidate(candidate, expected)`; `!won → Ok(None)`.
5. `persist_full`.
6. `db.promote_to_session_reset(outgoing_id, "session_switch")` (ends outgoing + bumps
   generation, reusing the shipped primitive at `session_db.rs:982`, whose reason list
   already blesses `session_switch`).
7. **`db.reopen_session(target_id)`** (§5.2).
8. `refresh_peer(... include compression ancestors)`.

Note `publish_forced_candidate` may need a sibling that accepts a caller-supplied
`session_id` instead of minting one; if `SessionEntry::new_candidate` hard-codes id
generation, add `SessionEntry::rebind_to(existing_id, origin)`. That is the only new entry
constructor.

### 5.2 One new DB primitive: reopen

```rust
/// Clear ended_at/end_reason on a session so a resumed transcript accepts new
/// turns. Inverse of end_session. Python: hermes_state.py:8512 reopen_session.
pub fn reopen_session(&self, id: &str) -> anyhow::Result<()>
```
Single `UPDATE sessions SET ended_at=NULL, end_reason=NULL WHERE id=?`. No generation bump
(the outgoing side already bumped in §5.1 step 6).

### 5.3 One new listing query + index

```rust
/// Recent sessions on one route, newest effective-activity first, for the
/// /sessions picker. Preview = first user message content, truncated.
pub fn list_route_sessions(&self, source: &str, session_key: &str, limit: usize)
    -> anyhow::Result<Vec<RouteSessionRow>>  // { id, started_at, last_activity,
    // ended_at, message_count, preview }
```
SQL: `SELECT id, started_at, last_activity_at, ended_at, message_count, (SELECT content
FROM messages m WHERE m.session_id = s.id AND m.role='user' AND m.active=1 ORDER BY m.id
LIMIT 1) AS preview FROM sessions s WHERE s.source=? AND s.session_key=? AND s.source !=
'tool' ORDER BY COALESCE(s.last_activity_at, s.started_at) DESC LIMIT ?`. Add migration
`CREATE INDEX IF NOT EXISTS idx_sessions_source_session_key ON sessions(source,
session_key, COALESCE(last_activity_at, started_at) DESC)` in `ensure_recovery_schema`
(`session_db.rs:1147`). Scoping to `(source, session_key)` is the entire IDOR story: the
list only ever contains the caller's own route.

### 5.4 Slash classification split

`slash.rs`: replace the single `resume → Unavailable` (`:81-83`) with, parsed from the raw
text and canonical `resume`:
- bare (`/resume` or `/sessions`, no positional) → `NativeSlashCommand::ResumeList`
- positional present → `NativeSlashCommand::ResumeSelect { arg: String }`

`/sessions full|all|search|<sub>` sub-grammar stays folded into "bare = list" for now
(unknown sub-args → list); `--all`/`--cross-room` are parsed and **rejected with a
"cross-scope resume requires admin, not available yet" note**, never silently widening.
`/compress` stays `Unavailable`.

### 5.5 Async orchestrator (`session_commands.rs`) + both ingress sites

Add two functions beside `reset_session`, taking `AdmissionDeps` + `SessionSource`:

- `list_sessions(deps, source, limit=10)`: resolve `session_key`, `store.list_route_sessions`,
  render a numbered text list (preview + msg count + relative age + short id), footer
  teaching `/resume <N>`. No leases (read-only snapshot; a stale row is harmless in a
  picker). Empty → "No earlier sessions on this chat yet."
- `resume_session(deps, source, arg)`: mirror `reset_session`'s loop  -
  1. **route lease** on `session_key`.
  2. observe `expected = current_entry_for_source`.
  3. resolve `arg`: numeric → index into `list_route_sessions` (out-of-range →
     `out_of_range` reply); else treat as id → `get_session(arg)` and **verify
     `row.source == source.platform && row.session_key == session_key`** (R4; fail →
     `not_found`, indistinguishable from "no such session" so ids can't be probed).
  4. `target = get_compression_tip(resolved_id)` (R5).
  5. if `expected.session_id == target` → drop lease, `already_on` reply.
  6. **transcript lease** on `target` (serialize against a concurrent turn on a shared
     target; §R1). Optionally also lease the outgoing id, but the route lease + CAS already
     covers the outgoing route mutation and an in-flight outgoing turn is *allowed* to finish
     into the switched-away row (matches Python).
  7. `store.switch_session(source, expected, target)`; `None` → drop leases, retry.
  8. **No cache retirement** (§6). Drop leases.
  9. Reply `resumed` with message count from `message_count(target)`.

Wire both `dispatch.rs:315-357` (push, deliver via `self.deliver`) and the HTTP handler in
`message.rs` (return `Json(MessageResponse{reply})`)  -  the **duplicated-handler trap**: the
native-command match arm exists at both sites and a new command missing from one transport
is the single most likely defect. Add `ResumeList`/`ResumeSelect` arms to both.

### 5.6 Reply strings

Mirror `gateway.resume.*` semantics with preview-based labels: list header + numbered
`N. <preview>  -  <n> msgs, <age>` + footer; `resumed` = "↻ Resumed session (<n> messages).
Conversation restored."; `already_on`; `out_of_range`; `not_found`; empty-list line;
`--all` → "Cross-chat resume needs an admin and isn't available yet."

---

## 6. Cache identity and retirement  -  the Rust advantage

Python `_evict_cached_agent(session_key)` on resume because its agent cache is keyed by
`session_key`; repointing that key to a new id would otherwise keep the stale agent (whose
memory provider cached `_session_id`). **Rust's cache is keyed by `(home, session_id)`
(`conversation_agent.rs:34,108-113`), so a resume that changes the route's `session_id`
changes the cache key.** The next turn on the resumed route resolves `target_id` and keys the
target's own client (built fresh, or reused if still warm from an earlier visit). The
outgoing session's client stays cached under its own id and idles out via the soft `Release`
sweep (`conversation_agent.rs:256`), never firing `on_session_end`  -  which is correct,
because a switch *ends nothing* (Python promotes the outgoing to `session_switch`, a boundary,
but does not run end-of-session extraction). So **resume needs no `retire_conversation` call
and no eviction**.

The one thing to prove by test (§7): the frozen part of a built client is
prompt/tools/plugins, not transcript (`native_agent.rs:241-247` "frozen per-conversation");
history is reloaded per turn from the DB via `TurnContext`. If that ever stops being true,
resume would need a soft `Release` of the target's cache entry to force a rebuild. Today it
does not, and a test pinning "resumed session sees full prior history" guards the assumption.

---

## 7. Concurrency, idempotency, scope

- **resume vs in-flight turn on the current session:** route lease + CAS serialize the route
  mutation; the in-flight turn holds the *outgoing* transcript lease and persists into the
  switched-away (now `session_switch`-ended) row, which stays searchable/recoverable
  (`session_db.rs:947,994` fences). Matches Python (old session queryable after switch).
- **resume vs concurrent turn on the target (shared session):** the target transcript lease
  (§5.5 step 6) serializes reopen+repoint against it.
- **resume vs reset on the same route:** both take the route lease and CAS on the observed
  entry; the loser sees `Ok(None)` and retries against the rotated entry (reset wins → resume
  re-observes the fresh session; resume wins → reset re-observes the resumed id).
- **two resumes on the same route:** serialize on the route lease; the second re-observes and
  either switches again or hits `already_on`. Idempotent at the user level.
- **cancellation:** the handler runs pre-model, synchronously in the ingress owner; a dropped
  HTTP waiter cannot leave a half-switch because the store write is a single CAS publish +
  two DB updates under the leases.
- **profile scope:** `list_route_sessions` and `switch_session` resolve the DB via
  `database_for_key` / the store's `home`, so a resume in profile A cannot enumerate or
  switch into profile B (`session_store.rs` profile-namespaced `session_key`,
  `:46-52`).

---

## 8. Test matrix (source-backed)

Harnesses: `session_store.rs:812+` (real temp `state.db`, raw `rusqlite` asserts),
`session_admission.rs:108+` (real store + lease registries, pre-held leases to force
retries), `session_db.rs:1780+` (real SQLite, `CREATE TRIGGER RAISE(ABORT)` failure
injection), `slash.rs:205+` (pure unit), `message.rs:366+` (`serve` + `reqwest` HTTP),
`dispatch.rs:576+` (`StubAgent`/`StubAdapter`, `handle_turn`).

**Store (`session_store.rs` / `session_db.rs`):**
1. `switch_session_repoints_route_to_existing_id_without_minting`  -  observe entry S1, create
   S2 out-of-band, `switch_session(expected=S1, target=S2)`; assert route entry now carries
   S2, **no new id**, S1 row `end_reason='session_switch'`, `conversation_generations`
   bumped, S2 `ended_at IS NULL` (reopened). (Analog of `explicit_reset_is_cas_fenced...`
   `session_store.rs:1124`.)
2. `switch_session_cas_rejects_stale_observed`  -  replay switch with a stale `expected` →
   `Ok(None)`, route unchanged.
3. `switch_to_current_session_is_already_on_noop`  -  `expected.session_id == target` → no
   mutation, `already_on` signal.
4. `reopen_session_clears_end_state`  -  end a session, `reopen_session`, assert
   `ended_at/end_reason` NULL and a subsequent message persists.
5. `list_route_sessions_scopes_to_route_and_orders_by_activity`  -  seed sessions across two
   `session_key`s and a `source='tool'` row; assert only the caller's route rows, tool
   excluded, newest-active first, preview = first user message.
6. `switch_lands_on_compression_tip`  -  parent ended `compression` with a live child; resume
   parent → `get_compression_tip` → child is the target.

**Slash unit (`slash.rs`):**
7. `resume_bare_and_sessions_classify_as_list` / `resume_with_arg_classifies_as_select`  -
   flips the current `Unavailable` stub; asserts neither reaches the model.
8. `resume_all_flag_is_refused_not_widened`  -  `--all` → admin-required note, not a widened
   scope.

**HTTP (`message.rs` `serve` + reqwest):**
9. `http_sessions_lists_own_route_previews`  -  two prior turns creating S1 then a reset to S2;
   POST `/sessions`; assert a numbered list containing both previews, S1 not leaking any other
   route.
10. `http_resume_switches_and_sees_prior_history`  -  from S2, `/resume 1` (→ S1); assert
    `resumed` reply, then a normal turn on the route resolves S1 and the agent sees S1's full
    prior history (the §6 cache-identity guard).
11. `http_resume_out_of_range_and_unknown_id`  -  `/resume 9` → out_of_range; `/resume
    <foreign-id>` → not_found (indistinguishable from nonexistent; the R4 IDOR guard).
12. `http_resume_serializes_against_in_flight_turn`  -  hold a turn open on the current
    session, fire `/resume` to another; assert the switch waits on the lease and the in-flight
    turn's message lands in the switched-away transcript, not the target. (Analog of
    `explicit_http_reset_waits_for_in_flight_persistence` `message.rs:1180`.)
13. Flip `unported_lifecycle_commands_never_reach_the_agent` (`message.rs:1304`) to keep
    `/compress` unavailable while `/sessions`/`/resume` now act.

**Push (`dispatch.rs` harness):**
14. `push_sessions_lists_and_resume_switches_without_forwarding`  -  push analog of 9+10,
    asserting the list + `resumed` replies and that the agent turn counter never advances on
    either command (analog of `push_reset_rotates_without_forwarding_the_command`
    `dispatch.rs:1079`).

---

## 9. Now vs deferred

**Implement now (the checkpoint):**
- `SessionStore::switch_session` (§5.1) + `SessionEntry::rebind_to` if needed.
- `SessionDb::reopen_session` (§5.2) and `list_route_sessions` + index (§5.3).
- `slash.rs` `ResumeList`/`ResumeSelect` split (§5.4), `--all` refused not widened.
- `session_commands::{list_sessions, resume_session}` wired at **both** ingress sites (§5.5).
- Preview-labeled own-route list; numeric + own-route direct-id resume; compression-tip
  landing; current-session `already_on`; **no** cache retirement (§6).
- Tests 1-14.

**Deferred (needs still-unported machinery), each a clean add-on that does not rework the
switch:**
- **Titles + auto-titling**  -  `title`/`title_source` columns + partial unique index,
  `set_session_title`/`set_auto_title` rank logic (`hermes_state.py:10982-11272`), the
  `agent/title_generator.py` first-turn LLM task, and `/title` + a real `/new <title>`
  (today discarded at `session_commands.rs:167`). Adding this only narrows the default list to
  titled rows and adds a title-resolution branch to `resume_session`.
- **Admin cross-scope**  -  `--all` / `--cross-room` / `/sessions all`, `_resume_caller_is_admin`,
  and the full `_resume_target_allowed` matrix (`gateway/slash_commands.py:1098-1258`). Until
  then the own-route scoping makes IDOR unreachable.
- **`/sessions full|search`**  -  the FTS-over-titles/preview and unnamed-inclusion sub-grammar
  (`hermes_cli/session_listing.py`). Rust has message FTS (`session_db.rs:1682`) but it is
  cross-session and message-level, not a per-route session search.
- **Full `resolve_resume_session_id` parity**  -  the parent-forward walk with
  reset/branch/delegate/tool exclusion (`hermes_state.py:13920-14000`); the compression-tip
  half is enough for the checkpoint.
- **Adapter buttons / interactive picker**  -  text numbered list is the whole UI today; the
  `resolve_by_id` button resolver and `qqbot_keyboards.rs` are shipped-but-dead
  (no adapter). Not needed and not in Python for resume anyway.
- **Prompt-cache warmth across the switch**  -  Python doesn't rewarm on resume either; the
  id-keyed cache reuses the target's warm client if still resident, which is already better.

---

## 10. One-line summary

Ship `/sessions` as a preview-labeled list of the caller's own recent route sessions and
`/resume <N|id>` as a CAS+lease-fenced soft switch (new `switch_session` + `reopen_session` +
per-route listing query, wired at both ingress sites, landing on the compression tip), scope
it to the caller's own `session_key` so IDOR is unreachable, and rely on the id-keyed
conversation cache to make retirement unnecessary  -  deferring titles, admin cross-scope,
search/full sub-grammar, buttons, and full lineage-walk parity as clean add-ons that do not
touch the switch.
