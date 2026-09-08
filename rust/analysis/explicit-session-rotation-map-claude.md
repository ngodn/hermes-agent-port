# Explicit gateway session rotation: `/new` `/reset` `/resume` `/compress` and compression-driven prompt invalidation

Independent audit. No implementation files were edited, no Cargo ran, nothing was
committed or pushed. Every Rust line reference was read from the working tree; every
Python contract was traced in the live reference tree (`gateway/`, `hermes_cli/`,
`run_agent.py`, `agent/`), not from the helper report.

Rust paths are under `rust/crates/hermes-gateway/src/`. Python paths are repo-root
originals.

---

## 0. Headline

Two of the four commands can land as production-quality native handlers now; two
cannot without lying about capability.

- **`/new` and its alias `/reset`**  -  implementable now. Every store, teardown,
  lineage, generation and lease primitive they need is already ported and live. The
  only missing pieces are a command handler and a small explicit-rotation seam on the
  session store. This is the real checkpoint.
- **`/resume`**  -  partially implementable, but the honest version needs a titled
  session index, an IDOR authority gate, and a `switch_session` store write that are
  not ported. Recommend deferring the full command; a same-session-id no-op guard is
  the only slice that is safe today.
- **`/compress`**  -  must remain deferred. Python `/compress` is a real transcript
  rewrite driven by `run_agent.AIAgent._compress_context` →
  `agent/conversation_compression.py` → `agent/context_compressor.py`, whose
  `_generate_summary` makes one real auxiliary-summarizer `call_llm` per attempt (an
  aux model call, not a control command to the main model). It is not
  `trajectory_compressor.py`  -  that is a separate offline/datagen tool not on the
  gateway path. None of the compression machinery is ported. The
  task's own constraint ("do not propose pretend compression or a handler that
  silently sends a command to the model") means the only correct native behavior today
  is an explicit "not available on this backend" refusal, not a stub.

There is also **one correctness risk in the current Rust design that this seam
exposes**: the slash gate sits *before* the turn lease in both ingress paths
(`dispatch.rs:259` vs lease acquire at `:366`; `message.rs:108` vs `:182`), so any
rotation handler wired at the existing gate site would run **unserialized against an
in-flight turn on the same session**. Getting the ordering right is the crux of the
design below (§4, §5).

---

## 1. Python contracts (traced from source)

### 1.1 Registration, canonical names, aliases

Command definitions live in `hermes_cli/commands.py`:

| Typed | Canonical | Aliases | Gateway? | args | Source |
| --- | --- | --- | --- | --- | --- |
| `/new` | `new` | `reset` | yes | `[name]` (title) | `commands.py:151-153` |
| `/reset` | `new` (alias) |  -  | yes | `[name]` | `commands.py:152` |
| `/clear` | `clear` |  -  | **CLI-only** (`cli_only=True`) |  -  | `commands.py:156-157` |
| `/compress` | `compress` | `compact` | yes | `[here [N] \| focus \| --preview]` | `commands.py:178-179` |
| `/resume` | `resume` |  -  | yes | `[name] [--all] [--cross-room]` | `commands.py:250-253` |
| `/sessions` | `sessions` |  -  | yes (delegates to resume) |  -  | `commands.py:254`, handler `slash_commands.py:5425` |

`/new` carries `busy_policy="interrupt_then_dispatch"`, `busy_handler="new"`
(`commands.py:153`): if an agent is already running for the session, `/new` interrupts
it (`_busy_new_command` → `_interrupt_and_clear_session` → `_handle_reset_command`,
`run.py:18659-18676`) and then dispatches the reset. `/compress` and `/resume` take the
default `busy_policy="reject"`  -  mid-turn they return a "can't run mid-turn, wait or
/stop" refusal (`run.py:18594-18600`) rather than interrupting. In Rust the per-session
turn lease already gives "reject" for free (a busy session yields a lease timeout /
"session busy"); only `/new` needs the interrupt-then-dispatch path, which the
lease-wait in §5.2 supplies by serializing behind the in-flight turn. `/clear` is
**not** a gateway command  -  only `/new` and `/reset` reach the reset handler on a
platform.

Alias resolution to canonical happens in `gateway/run.py:19630-19655` (`_resolve_cmd`
+ `quick_commands` operator aliases), before dispatch.

### 1.2 Access gates (identical to the ported Rust policy)

All four commands gate through `GatewayRunner._check_slash_access`
(`run.py:24013-24054`), called at `run.py:19663-19666`. It delegates to
`gateway.slash_access.policy_for_source`  -  the exact module ported verbatim into
`slash_access.rs`. Semantics:

- Gating is **off** unless the operator set `allow_admin_from` for the source's scope
  (DM vs group). Off → every allowed user runs every command (backward-compat).
- On → non-admins may run only commands in `user_allowed_commands` plus the
  always-allowed floor `help`/`whoami` (`slash_access.py`, ported at
  `slash_access.rs:19` `ALWAYS_ALLOWED_FOR_USERS`).
- `new`/`reset`/`compress`/`resume` are **not** in the floor, so on a gated platform a
  non-admin is denied unless explicitly allowlisted.

`/resume` has a **second, command-specific authority gate**:
`_resume_target_allowed` (`slash_commands.py:5333`)  -  an IDOR guard binding the target
session to the caller's own platform/user/chat so one user cannot attach to another's
transcript by guessing a session id/title. Matrix adds room binding
(`:5323-5332`).

`/new` (and `/undo`) additionally pass through
`_maybe_confirm_destructive_slash` (`run.py:19761-19770`)  -  an operator-configurable
two-step confirmation UX (button/inline). Optional; not load-bearing for correctness.

### 1.3 `/new` / `/reset` ordering  -  the authoritative sequence

`GatewayRunner._handle_reset_command`, `slash_commands.py:150-359`. Exact order:

1. `session_key = _session_key_for_source(source)` (`:155`).
2. **`_invalidate_session_run_generation(session_key, reason="session_reset")`**
   (`:156`)  -  bump the run generation *first* so an in-flight run's guarded release
   returns `False` and cannot resurrect its slot (#28686).
3. `_release_running_agent_state(session_key)` (`:162`)  -  evict the running-agent slot
   (idempotent).
4. Snapshot `old_entry = session_store._entries.get(session_key)` (`:166`) **before**
   rotation, so finalize can report the expiring id.
5. Under `_agent_cache_lock`, fetch the cached agent and, if present,
   **`await asyncio.wait_for(_cleanup_agent_resources(old_agent),
   timeout=_RESET_CLEANUP_TIMEOUT_S)`** off-loop, bounded, **proceeds on timeout**
   (`:180-208`, timeout const at `:58-60`). This is HARD teardown: `agent.close()` →
   `shutdown_memory_provider` (fires `on_session_end`), `process_registry.kill_all`,
   sandbox/browser cleanup, DB `end_session`.
6. `_evict_cached_agent(session_key)` (`:209`).
7. `_clear_conversation_scope(session_key, reason="session_reset")` (`:215`)  -  clears
   all conversation-scoped per-session state (model/reasoning overrides, one-turn
   restores, model notes, last-resolved cache, `/queue` overflow) + security state.
8. `interrupt_for_session(...)` (`:224-233`)  -  end the old conversation's in-flight
   async delegations, keyed by durable `old_entry.session_id` and routing key.
9. `clear_env_passthrough()`, `clear_credential_files()` (`:235-245`).
10. **`new_entry = await async_session_store.reset_session(session_key)`** (`:248`)  - 
    the store write that rotates the session id (mints a new id; the old row was ended
    in step 5; lineage recorded).
11. `_finalize_session_off_loop(...)` (`:260-266`)  -  plugin `on_session_finalize`,
    off-loop + bounded.
12. `hooks.emit("session:end")`, `hooks.emit("session:reset")` (`:271-282`).
13. Build banner (`_reset_notice_session_info`, profile-scoped #59003), optional title
    from `/new <title>` (`:301-322`), Telegram topic rebinding (`:324-333`),
    `on_session_reset` plugin hook (`:335-348`), tip line (`:350-355`).
14. Return `EphemeralReply(...)`.

Load-bearing invariants: **generation bump and running-slot eviction happen first**;
**HARD teardown (`on_session_end`) happens before the id rotation**; teardown is
**bounded and proceeds on timeout** (`_RESET_CLEANUP_TIMEOUT_S`).

### 1.4 `/resume` ordering  -  a SOFT switch, not a session end

`_handle_resume_command`, `slash_commands.py:5211-5398`:

1. Resolve `target_id` from a number, title, or raw id (`:5289-5313`).
2. **`resolve_resume_session_id(target_id)`** (`:5319`)  -  follow the compression child
   chain to the live tip, so resume lands on the continuation that holds the transcript
   (#15000).
3. Authority gate `_resume_target_allowed` / Matrix room bind (`:5323-5340`).
4. If `current_entry.session_id == target_id` → early "already on" return (`:5344`).
5. `_release_running_agent_state(session_key)` (`:5348`).
6. **`switch_session(session_key, target_id)`** (`:5351`)  -  repoint the routing entry
   to the *existing* target id (a `session_switch`, no new id minted, no
   `on_session_end`).
7. `_clear_conversation_scope(session_key, reason="resume")` (`:5360`).
8. `_evict_cached_agent(session_key)` (`:5367`)  -  **SOFT** cache release so the next
   turn rebuilds against the correct id. No provider `on_session_end` here: resume
   never ends a session, it switches away from one and into another.
9. Load transcript for the message count, return the banner.

So `/resume` = soft cache release + routing repoint to an existing id + generation bump
via `session_switch`. It does **not** fire end-of-session extraction.

### 1.5 `/compress`  -  real LLM rewrite, deeply unported

`_handle_compress_command` (`slash_commands.py:4481`, a profile-scope wrapper) →
`_handle_compress_command_inner` (`:4564`). It:

- Loads the full transcript, requires ≥4 messages (`:4584`).
- Parses `--preview/--dry-run` (report-only, no writes, `:4614-4629`), `here [N]`
  boundary-aware partial compress, and a focus topic (`:4599-4603`).
- Builds a **temporary `run_agent.AIAgent`** and calls
  **`tmp_agent._compress_context(head, ..., force=True)`** (`:4750-4802`), which routes
  through `agent/conversation_compression.py::compress_context` →
  `agent/context_compressor.py::_generate_summary` → one auxiliary-summarizer
  `call_llm` (`context_compressor.py:5548`). Memory is committed first via
  `agent.commit_memory_session` before the transcript is rewritten. This is **not**
  `trajectory_compressor.py` (an unrelated offline tool).
- Default mode is **in-place** (`compression.in_place`, default true, #38763): same id,
  `archive_and_compact` soft-archives active rows and inserts the compacted set. The
  legacy **rotation** mode ends the old session `compression`, mints a child id, and
  `publish_compression_child(parent_session_id=old, ...)` records the lineage.
- Persists the compressed transcript, then either **rotates** (ends the old session
  with `end_reason='compression'`, creates a continuation id, writes compressed
  messages into the new session so the original stays searchable) or **compacts in
  place** (`compression.in_place`/#38763: same id, active rows soft-archived
  `active=0`, compacted set inserted)  -  `:4822-4850`. Persist-before-repoint ordering
  is explicit and load-bearing (`:4831-4841`).
- Has a dedicated codex-app-server path that compacts a server-side thread
  (`_compress_codex_app_server_session`, `:4503-4562`).

Every path depends on `run_agent.AIAgent`, `agent.context_compressor`,
`agent.conversation_compression`, `agent.manual_compression_feedback`,
`hermes_cli.partial_compress`, and `trajectory_compressor.py`  -  **none ported**. There
is no honest native `/compress` until the agent core and compressor land.

---

## 2. What the Rust gateway can do today

### 2.1 Slash handling stops at gating + three built-ins

`slash.rs` gates via the ported `slash_access` policy and answers exactly three
built-ins itself: `help`, `whoami`, `status` (`slash.rs:16`, `handle_builtin` `:84`).
Both ingress paths gate identically (`dispatch.rs:259-272`, `message.rs:108-120`) and
then, for any *allowed non-built-in* command, **fall through to the agent as ordinary
text** (`slash.rs:7-8`, `dispatch.rs:265-270`). So today `/new`, `/reset`, `/resume`,
`/compress` are gate-checked and then delivered to the model as the literal string
"/new" etc. There is **no rotation handler on either path**.

### 2.2 The rotation primitives that already exist and are live

The bounded-conversation-cache checkpoint (PORT.md, 2026-09-08) already shipped
everything a native `/new`/`/reset` needs except the trigger:

- **Automatic reset** is fully wired. `session_store.transition` detects a policy reset
  and publishes a one-shot predecessor marker; both ingress paths consume it via
  `take_auto_reset_predecessor` (`session_store.rs:71`) and retire the old conversation
  through `retire_conversation` (`dispatch.rs:330-346`, `message.rs:143-172`).
- **Hard teardown with deferral**  -  `ConversationAgent::retire_session(home,
  session_id)` (`conversation_agent.rs:232`) marks the entry for `SessionEnd`
  retirement and, if a finalizer is still pending (`pending_turns > 0`), defers
  teardown until it completes rather than racing it. `close_retired` loads the durable
  lifecycle transcript and calls `close_conversation(Some(messages))`
  (`conversation_agent.rs:572-603`), which drives the extension host's
  `flush_pending → session_end → shutdown → await worker` (`extension_host.rs:307-364`)
   -  the Rust analog of Python's `on_session_end`.
- **Store rotation**  -  `get_or_create_session(..., force_new=true, ...)`
  (`session_store.rs:209`) mints a brand-new session id, bypassing reset policy
  (verified by the `force-new must not evaluate reset policy` test,
  `session_store.rs:878-881`).
- **Reset boundary + lineage**  -  `db.promote_to_session_reset(parent, reason)`,
  `create_session` with `parent_session_id` + `model_config._reset_from`
  (`session_store.rs:497-534`), `end_session` (`session_db.rs:958`),
  `finalize_session_expiry` (`:935`).
- **Conversation generation**  -  `bump_conversation_generation` (`session_db.rs:285`)
  fires for reasons `session_reset`, `session_switch`, `idle`, `daily`, `suspended`,
  `resume_pending_expired`. Note **`compression` is deliberately absent** from that
  list  -  a compression boundary does not bump the generation.
- **Compression lineage reads**  -  `get_compression_tip` (`session_db.rs:696`),
  `get_compression_chain` (`:650`), `session_lineage_root_to_tip` (`:607`) are ported,
  so a future `/resume` can already follow a compression chain to the tip.
- **Turn-lease rebind for rotation**  -  `SessionTurnLeaseRegistry::rebind`
  (`turn_lease.rs:183`) exists specifically for mid-turn compression rotation
  (its header, `:4-5`), and is unwired.
- **Run-generation invalidation**  -  `session_registry.rs:126` `invalidate_run_generation`
  is a verbatim port of Python's `_invalidate_session_run_generation`, but the whole
  `SessionRegistry` is **unwired** (no caller anywhere in the tree). In Rust the turn
  lease, not a running-agent slot, is the live serialization primitive, so this helper
  is not load-bearing yet.
- Cache maintenance and bounded shutdown are wired in `main.rs` (`start_maintenance`
  `:995`, `cache.shutdown(45s)` `:1094`).

### 2.3 Prompt / cache-scope keying

The native client's prompt cache key is derived per turn from the session id:
`turn_client.cache_scope = message_session_id(msg)` (`native_agent.rs:580`), fed to
`prompt_cache::apply` (`:478-485`). `prompt_cache.rs:94-100` honors a
`cache_scope_id` that takes precedence over the physical id ("rotation-stable logical
scope", mirroring Python `_cache_scope_from_session_id`), but no caller sets it. The
cache entry itself is keyed `(home, session_id)` (`conversation_agent.rs:108-113`).

Consequence for rotation: a `/new` (new id) naturally produces a new cache key, a new
conversation client, and a cold prompt cache  -  which is exactly right for a reset. For
`/compress` and `/resume`, Python deliberately keeps the *same* agent across a rotated
id (keyed by `session_key`, #54947) to preserve prompt-cache warmth; Rust's
id-keyed cache would go cold. That warmth gap is real but is a performance property,
not a correctness one, and it belongs with the deferred rotation-stable-keying work.

---

## 3. Compression-driven prompt invalidation, concretely

"Compression-driven prompt invalidation" in the Python model is two coupled effects:

1. The transcript is replaced by a summary (rotation to a child id, or in-place
   `active=0` archival + compacted insert). The message prefix the model sees changes,
   so any warm provider-side prefix cache for the old prefix is naturally invalidated.
2. The cached agent for the routing key is **kept** (not torn down), because Python
   wants the summarized continuation to run on the same warm client; only the
   transcript and the persisted `_cached_system_prompt` are rewritten.

Rust cannot honestly reproduce either half yet: there is no compressor to produce the
summary, and the conversation client freezes its prompt/tool/plugin snapshot for the
conversation's life (`native_agent.rs:241-247`, "frozen per-conversation"  -  the design
deliberately has no mid-conversation prompt rebuild). So both the compression pass and
the in-conversation prompt rebuild it would trigger are out of scope. This is why
`/compress` is deferred, not stubbed.

---

## 4. Correctness risks in the current Rust design (prioritized)

**R1 (high)  -  the slash gate runs before the turn lease, so a rotation handler wired
at the gate site would race an in-flight turn.** In both paths the slash decision is
evaluated at `dispatch.rs:259` / `message.rs:108`, well before session resolution and
the per-session turn lease (`dispatch.rs:366`, `message.rs:182`). Python's reset is
explicitly serialized against the live run (it bumps the generation and interrupts,
`slash_commands.py:156-162`, and carries `busy_policy="interrupt_then_dispatch"`). A
naive Rust `/new` handler dropped in at the gate would rotate the store id and call
`retire_session` while a concurrent turn on the same session id is mid-run holding the
lease and its own client clone. `retire_session` handles the *teardown* race correctly
(it defers on `pending_turns > 0`), but the *store rotation* would still execute
concurrently with the running turn's persistence. The fix is to make the rotation
handler acquire the same per-session turn lease before mutating the store (§5).

**R2 (medium)  -  `force_new` mints a new id but records no predecessor lineage or reset
boundary.** `transition` under `force_new=true` reaches
`publish_forced_candidate(candidate, observed)` (`session_store.rs:480-486`) but never
sets `context.prev_session_id`, so the `promote_to_session_reset` / `_reset_from`
lineage block at `:497-534` is skipped. The auto-reset path gets lineage only because
policy sets `prev_session_id` (`:397-402`). An explicit `/new` built directly on
`force_new` would therefore mint a new session with **no `end_reason` on the old row,
no generation bump, no parent link, and no cache retirement**  -  a silent orphan. The
explicit-rotation seam must reproduce the promote + retire that auto-reset already does
(§5.1).

**R3 (medium)  -  no explicit path fires `on_session_end` today.** Only auto-reset,
policy expiry, and shutdown call `retire_session`/`retire_conversation`. Since there is
no `/new` handler, an operator-driven reset currently produces nothing:  the "/new"
text is sent to the model. Any handler must route through `retire_session` (hard) to
match Python's pre-rotation `on_session_end`.

**R4 (low)  -  `SessionRegistry` run-generation is ported but inert.** If a later change
introduces a running-agent slot (e.g. streaming drafts, `/stop`), the reset must bump
the generation the way Python does. Today the turn lease covers the same ground, so
this is a latent gap, not a live bug. Note it so the lease-based serialization in §5
is not later duplicated or contradicted by wiring the registry.

---

## 5. Smallest deep native interface + exact call sites

Goal: land `/new`+`/reset` as a production handler, refuse `/compress` honestly, and
guard `/resume` to a safe no-op, reusing every existing primitive and adding the one
missing store seam. No new config keys.

### 5.1 One new store method: explicit rotation

Add to `SessionStore` a method that does what auto-reset does, but unconditionally and
returning the predecessor id so the caller can retire its cache entry. It reuses the
existing `transition` lineage block rather than bolting rotation onto raw `force_new`:

```rust
/// Explicit user-driven reset. Ends the current session as `session_reset`
/// (bumping the conversation generation and recording lineage), mints a fresh
/// id, and returns (new_entry, prev_session_id). Mirrors auto-reset's
/// promote_to_session_reset + _reset_from wiring (session_store.rs:497-534),
/// but is not gated on reset policy.
pub fn reset_session(&self, source: &SessionSource)
    -> Result<(SessionEntry, Option<String>), Arc<anyhow::Error>>
```

Implementation is a thin variant of `transition`: capture the observed entry's
`session_id` as `prev_session_id`, run `publish_forced_candidate`, then take the
existing `if let Some(parent) = &context.prev_session_id` branch so
`promote_to_session_reset(parent, "session_reset")` and `create_session(..
parent_session_id, _reset_from ..)` run. This closes R2 in one place and keeps the
lineage/generation logic single-sourced.

### 5.2 One new dispatch seam: rotation runs under the turn lease

Both ingress paths already have the shape. The rotation handler must run **after** the
turn lease is acquired for the resolved session id, so it serializes against any
in-flight turn (closes R1). Concretely, extend the slash decision so a recognized
rotation command is *not* answered at the early gate but is carried as an enum through
session resolution and lease acquisition, then handled in place of `run_admitted_turn`:

- `slash.rs`: add a `RotationCommand { New { title: Option<String> } }` arm (canonical
  `new`, alias `reset`; `clear` stays CLI-only and is not recognized here). Keep
  `handle_builtin` for help/whoami/status unchanged. `/compress` and `/resume` are
  recognized only to produce a fixed refusal/no-op reply (below), not a rotation.
- `dispatch.rs:259-272` and `message.rs:108-120`: when the decision is a rotation
  command, still run the access gate (unchanged), but skip the "flow to agent" branch.
- After lease acquisition (`dispatch.rs:366` / `message.rs:182`) and session
  resolution, branch: instead of running a model turn, call the rotation:
  1. `let (new_entry, prev) = store.reset_session(&source)?;`  (§5.1)
  2. if `let Some(prev) = prev { agent.retire_conversation(TurnContext::from_database(db), &prev); }`  -  reuses `retire_session`'s deferred hard teardown (`conversation_agent.rs:232`), matching Python step 5's bounded `on_session_end`.
  3. Optionally apply `/new <title>` via a `set_session_title` store call (defer if the title index isn't ported).
  4. Deliver the reset banner (a fixed localized string; the tip/hook cosmetics of
     Python steps 11-13 are optional and can follow).

Holding the lease across steps 1-2 gives Python's serialization for free: a concurrent
turn on the same session id either already holds the lease (reset waits) or is blocked
behind it. After rotation the new id gets a fresh lease; the old in-flight turn keeps
its old lease and its own client clone, persists into the now-ended old transcript
(still searchable), and its finalizer completes before `retire_session`'s deferred
teardown fires `session_end` on the old id  -  exactly once.

### 5.3 `/compress` and `/resume`: honest refusals now

- `/compress`: recognized, gated, and answered with a fixed reply that says compression
  is not available on the native backend and suggests `/new` for a clean session. Do
  **not** synthesize a summary, and do **not** forward "/compress" to the model
  (forwarding would be the "silently sends a command to the model" anti-pattern the
  task forbids). A `--preview` sub-mode could later be a pure token estimate with no
  writes, but even that needs a token estimator; defer.
- `/resume`: the only safe slice today is the `current.session_id == target_id`
  early-return "already on this session" and the "session listing unavailable on this
  backend" reply. The switch itself needs `switch_session`, a titled-session index
  (`list_sessions_rich`, `resolve_session_by_title`, `resolve_resume_session_id`), and
  the `_resume_target_allowed` IDOR gate  -  none ported. Recommend deferring the whole
  command behind that "not available" reply rather than shipping a switch without the
  authority gate (shipping it without the IDOR guard would be a security regression).

---

## 6. Concurrency, cancellation, idempotency, profile scope

- **Concurrent rotation vs in-flight turn**  -  handled by §5.2: the lease serializes.
  Two `/new` on the same session serialize behind one lease; the second sees the
  already-rotated entry and rotates again (idempotent-ish: each mints a fresh empty
  session; harmless). Two routing keys → one session id is the exact many-to-one case
  the lease was built for (`turn_lease.rs:11-13`).
- **Two requests resolving across an id rotation**  -  request A rotates old→new under
  the lease; request B, if it resolved the old id before A's store write, is serialized
  behind A on the old lease and will persist into the ended old transcript (searchable,
  not lost)  -  matching Python, where the old session stays queryable after reset. B's
  *next* message resolves to the new id. No interleaving on one transcript.
- **In-flight finalizer**  -  `retire_session` defers hard teardown while
  `pending_turns > 0` (`conversation_agent.rs:241-248`) and fires it from
  `finish_turn` once the post-persist finalizer completes (`:216-226`). So an explicit
  reset during a live turn never drops the old session's `on_session_end`; it runs
  after the finalizer, exactly once. This is the property the cache checkpoint already
  proved (`reset_retirement_waits_for_post_persist_finalizer`,
  `conversation_agent.rs:896-965`).
- **Cancellation**  -  both paths run the admitted turn in a detached `tokio::spawn` that
  owns the lease token (`dispatch.rs:382`, `message.rs:201`), so a disconnected HTTP
  waiter or dropped ingress future does not abort the rotation or its teardown. A
  rotation handler placed in the same admitted-turn owner inherits that guarantee.
- **Retry / idempotency**  -  reset is naturally idempotent at the user level (re-running
  `/new` just mints another fresh session). The store write is a single
  `promote + create` transition; a crash between promote and create is recoverable by
  the normal finder (the old row is ended, the new row absent → next message creates
  one). `retire_conversation` is idempotent (missing entry → `false`, no-op).
  `close_conversation` is documented idempotent (`agent.rs:107-116`).
- **Profile scope**  -  the cache key is `(home, session_id)` and `home` is the resolved
  profile install dir (`conversation_agent.rs:108-113`). `reset_session` must resolve
  the source's profile the same way `transition` does
  (`session_store.rs:238-250`, `multiplex_profiles`), so a reset in profile A cannot
  retire profile B's client. Python installs the per-profile secret scope around
  `/compress` (`slash_commands.py:4494-4501`); the native reset does no provider I/O in
  the handler itself (teardown is off the handler via `retire_session`'s scheduled
  task), so it needs no scope install beyond passing the correct `home` into the cache
  key  -  but any future title write must run under the resolved profile's DB.

---

## 7. Tests (source-backed)

**Unit (extend `conversation_agent.rs` + `session_store.rs` harnesses):**

1. `reset_session_ends_old_bumps_generation_and_links_parent`  -  call the new
   `reset_session`, assert the old row's `end_reason == 'session_reset'`, the
   `conversation_generations` row incremented, and the new row's
   `model_config._reset_from` / `parent_session_id` point at the old id. (Mirrors the
   existing `live_idle_reset_preserves_predecessor_and_persists_boundary`,
   `session_store.rs:942`.)
2. `explicit_reset_retires_predecessor_cache_entry`  -  checkout a client on the old id,
   run `reset_session`, feed the returned `prev` to `retire_conversation`, assert the
   old `(home, old_id)` entry is scheduled for `SessionEnd` and, with no pending turn,
   closed with `Some(messages)` (extends the `RecordedAgent` `closes == [true]`
   pattern).
3. `reset_during_live_turn_defers_session_end_until_finalizer`  -  start a turn (pending
   = 1) on the old id, run the rotation + `retire_conversation`, assert the entry
   survives and `closed == 0`; complete `finalize_turn_after_persist`, assert
   `closed == 1` exactly once. (Direct analog of `conversation_agent.rs:896-965`.)
4. `force_new_still_records_no_lineage`  -  a guard test pinning R2: raw
   `get_or_create_session(force_new=true)` leaves the old row's `end_reason` NULL, so
   the new `reset_session` path is required for lineage. Prevents a future refactor from
   quietly routing `/new` through bare `force_new`.
5. `compress_and_resume_refuse_without_forwarding`  -  a `slash.rs` unit asserting the
   rotation recognizer returns the fixed refusal reply for `/compress` and `/resume`
   and never yields an `Allowed`-to-agent decision (so the literal command can't reach
   the model).

**Real HTTP (extend the `/api/message` integration harness, matching the existing
`coordinated_dispatch_resumes_durable_history_after_store_restart` and the "real
two-request HTTP auto-reset" already in the tree):**

6. `http_new_command_rotates_session_and_starts_fresh`  -  POST a normal turn, capture
   `resolved_session_id` S1; POST `/new`; assert the reply is the reset banner (not a
   model turn), the old row is ended `session_reset`, and a following turn reports a
   new `resolved_session_id` S2 ≠ S1 with empty history.
7. `http_new_serializes_against_in_flight_turn`  -  hold a turn open on session S
   (gate agent), fire `/new` concurrently, assert the reset acquires the lease only
   after the in-flight turn's finalizer runs, the old transcript keeps the in-flight
   turn's assistant message, and `on_session_end`/close fires exactly once on the old
   id. This is the R1 regression test.
8. `http_new_denied_for_non_admin_on_gated_platform`  -  with `allow_admin_from` set and
   `/new` not in `user_allowed_commands`, assert a non-admin gets the denial text and
   no rotation occurs (agent/turn count unchanged), mirroring
   `denied_slash_command_skips_agent_and_delivers_refusal` (`dispatch.rs:982`).
9. `http_compress_is_refused_not_forwarded`  -  POST `/compress`, assert the fixed
   "not available" reply and that the stub agent's turn counter stays 0.

---

## 8. Now vs deferred

**Implement now (the checkpoint):**

- `SessionStore::reset_session` (§5.1)  -  explicit rotation with lineage + generation,
  single-sourced from the existing `transition` promote/create block (closes R2).
- Native `/new` + `/reset` handler wired **after** the turn lease in both `dispatch.rs`
  and `message.rs` (§5.2), routing through `retire_conversation`/`retire_session` for
  bounded, deferred, exactly-once hard teardown (closes R1, R3).
- Optional `/new <title>` only if a `set_session_title` store write is trivial;
  otherwise defer the title arg.
- Honest fixed-reply recognizers for `/compress` and `/resume` (§5.3) so the literal
  command never reaches the model.
- Tests 1-9.

**Deferred (needs still-unported machinery):**

- **`/compress` for real**  -  blocked on `run_agent.AIAgent`,
  `agent/conversation_compression.py`, `agent/context_compressor.py` (the aux
  summarizer `call_llm`), `hermes_cli/partial_compress.py`, and the in-conversation
  prompt rebuild the frozen native client deliberately lacks (`native_agent.rs:241-247`).
  Includes the default in-place `archive_and_compact`, the legacy rotation
  `publish_compression_child` lineage, the codex-app-server thread compaction path, and
  `--preview`. (`trajectory_compressor.py` is not on this path and is not a
  dependency.)
- **`/resume` for real**  -  blocked on `switch_session`, the titled-session index
  (`list_sessions_rich`, `resolve_session_by_title`), `resolve_resume_session_id`
  (compression-chain tip walk; the read helpers exist at `session_db.rs:650-704` but
  the switch write does not), and the `_resume_target_allowed` IDOR authority gate.
  Ship the switch only with that gate, never without it.
- **Rotation-stable cache keying (#54947)**  -  keying the conversation cache by
  `session_key` and setting `cache_scope_id` to the stable key so `/compress`/`/resume`
  keep the prompt cache warm across a rotated id (`prompt_cache.rs:94-100`,
  `native_agent.rs:580`). Performance, not correctness; lands with compression.
- **`SessionRegistry` run-generation wiring / `busy_policy` interrupt-then-dispatch**
   -  only needed once a running-agent slot or streaming-draft surface exists (R4). Until
  then the turn lease is the serialization contract; do not wire both without
  reconciling them.
- **Destructive-confirm UX** for `/new` (`_maybe_confirm_destructive_slash`) and the
  cosmetic reset banner extras (tips, `session:reset`/`on_session_reset` hooks,
  Telegram topic rebinding)  -  additive, non-correctness.

---

## 9. One-line summary

Land native `/new`/`/reset` now on the primitives already shipped (add one
`reset_session` store seam, wire the handler **after** the turn lease to serialize
against live turns, retire the predecessor through the existing deferred hard-teardown
path), refuse `/compress` and `/resume` honestly rather than stub or forward them, and
defer both until the compressor and the titled-session/IDOR machinery are ported.
