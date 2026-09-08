# Frozen conversation state: persisted `tool_names` + frozen plugin sections

Independent source map for the next native port seam. Scope: making
`conversation_prompt::Resolution.restore_frozen_sections` (and the tool-order
freeze it implies) real for native routed conversations, without touching
production or test code. Read-only review; anchors are `file:line` at the time
of writing (branch `rust-rewrite`).

## Headline

Both behaviors the task targets (persisted `tools[]` prefix freeze, frozen
plugin prompt sections) are **already fully modeled in isolated Rust modules with
golden coverage**, but **neither has a live consumer**, and the native prompt
that would carry them **does not yet contain the things being frozen**:

- The native prompt is reused **byte-for-byte verbatim** on continuation
  (`native_agent.rs:207`, `468-476`; `main.rs:1041`). That already guarantees
  prompt-cache identity. `Resolution.restore_frozen_sections` /
  `reconstruct_static_prefix` are inert booleans - a grep for consumers finds
  none outside `conversation_prompt.rs` and its own tests.
- The live prompt assembler `Initializer::build_fresh` **never calls
  `load_plugin_sections`** (`conversation_prompt.rs:145-378`; the only `plugin`
  hit in that function is `plugin_hint: ""` at line 308). So a native prompt
  contains zero plugin section frames today, and `plugin_prompt::restore` over a
  reused prompt returns `Vec::new()` (`plugin_prompt.rs:304-313`). Restoring
  frozen plugin bytes is currently **vacuous**.
- The native tool surface is a **static singleton**: `CurrentTimeTool` only,
  gated by `config.agent_tools` (`main.rs:386`, `453-457`). There is no
  registry, no `check_fn` availability probe, and nothing that can flap. There is
  **no `tool_names` column and no persist/read for it** in `session_db.rs`
  (schema list `session_db.rs:1095-1111` has no `tool_names`; Python's does at
  `hermes_state_common.py:477`).

So the honest smallest checkpoint is **not** "implement the freeze in the wire
path" (there is nothing dynamic to freeze). It is: (1) close the concrete
DB-parity gap (`tool_names` column + methods) so the pin round-trips and a
Rust-created DB stays Python-compatible; (2) port `_merge_preserving_prefix` as a
pure, golden-tested function so the seam exists exactly where the future dynamic
tool manager plugs in; (3) give the frozen values a real per-conversation owner
(`NativeAgentClient`) and consume the existing `restore_frozen_sections` flag by
seeding a `plugin_prompt::Snapshot`, so the seam is load-bearing the moment
native gains a plugin manager or a compression rebuild. Everything that depends
on availability probes, a plugin manager rendering into `build_fresh`, or a
mid-conversation re-assembly path must wait - those subsystems do not exist in
native yet.

## Premises from the prior handoff that no longer hold

The `async-conversation-prompt-map` handoff summary is stale on three points;
the code moved since:

1. **"`restore_or_build` is unwired/staged."** It is wired into the live path:
   `build_conversation_client` calls it at `main.rs:459-487`, with the async
   `build` closure delegating to `initializer.build_fresh` (`main.rs:474-485`).
2. **"`ConversationAgent`'s factory is sync and runs under `std::sync::Mutex`;
   convert to async + per-key `OnceCell` single-flight."** Already done:
   `clients: tokio::sync::Mutex<Clients>` where `ClientCell =
   tokio::sync::OnceCell` (`conversation_agent.rs:25-31`), the map lock is taken
   only to `entry(...).or_insert_with(OnceCell::new)` then dropped, and
   `get_or_try_init` runs the async factory with the map lock released
   (`conversation_agent.rs:84-108`).
3. **"The factory ignores its captured context."** The live factory
   `build_conversation_client` captures an `Arc<Initializer>` and the `Config`
   (constructed `main.rs:647-655`) and uses `home`, `message`, `history`,
   `database` plus the retained `Initializer` caches. The generic `Factory` type
   alias signature is `(home, msg, history, database)`
   (`conversation_agent.rs:17-24`), but the captured-once inputs the handoff
   called missing (`root`, launch cwd, user home, two env captures, skills
   `PromptLoader`, `ProtocolCache`, `EnvironmentPromptInputs`, timezone cache)
   are all fields of `Initializer` (`conversation_prompt.rs:65-76`,
   captured in `Initializer::capture` `90-115`).

The one genuinely-missing input the handoff named that is still true: the
**advertised tool-name surface** is passed explicitly but is degenerate -
`tools: &["current_time"]` or empty (`main.rs:453-457`), feeding only skill/guidance
gating inside `build_fresh` (`conversation_prompt.rs:169-182`, `input.tools` at
`244-249`).

---

## Q1. What data is read/written, and in what order relative to fresh build and provider I/O

Python (`agent/conversation_loop.py:_restore_or_build_system_prompt`, 993-1227):

1. **Read** the session row when there is history: `get_session` at
   `conversation_loop.py:1025`. Three-way state on `system_prompt`
   (missing/null/empty/present, `1027-1034`).
2. If present and `_stored_prompt_matches_runtime` (`1043`): **reuse verbatim**
   (`agent._cached_system_prompt = stored_prompt`, `1124`), then in order:
   - **read** `session_row["tool_names"]` and `restore_agent_tool_prefix`
     (`1129-1133`),
   - **restore plugin sections** from the stored prompt bytes,
     `restore_plugin_prompt_sections(agent, stored_prompt)` (`1139-1141`),
   - **reconstruct static prefix** from stored bytes (`1153-1155`).
   No build, no write.
3. Else **build fresh**: `agent._build_system_prompt(system_message)` (`1182`),
   fire `on_session_start` (`1187-1196`), seed credits (`1204-1207`), then
   **write**: `update_system_prompt` **then** `persist_agent_tool_names`
   (`1215-1220`), both inside one try. Persist happens **before** the caller
   proceeds to the model request.

The invariant: **read → (reuse | build) → persist, all before provider I/O.**
`persist_agent_tool_names` writes the names of `agent.tools`
(`mcp_tool.py:8912-8924` → `update_session_tool_names`,
`hermes_state.py:9635-9652`). `update_system_prompt` is content-addressed: it
stores the prompt in `system_prompts(hash,prompt)`, sets
`system_prompt_hash` and NULLs the `system_prompt` column
(`hermes_state.py:9621-9633`); `get_session` resolves it back via
`COALESCE(sp.prompt, s.system_prompt)`.

Rust today matches the prompt half exactly, with the tool half absent:

- `restore_or_build` (`conversation_prompt.rs:510-621`): read `session_row`
  guarded by `has_history` (`521-534`); classify `StoredState`
  (`536-544`); reuse-verbatim early return when `stored_prompt_matches_runtime`
  and no capability/bot rebuild (`547-587`) - this return sets
  `restore_frozen_sections: true, reconstruct_static_prefix: true` but performs
  **no** tool or plugin restore; else `build_snapshot` read (`595-601`), `build`
  (`603`), then `persist_prompt` (`605-610`).
- `SessionDb` implements `PromptStore` with `get_session` / `update_system_prompt`
  (`conversation_prompt.rs:487-506`); the Rust `update_system_prompt`
  (`session_db.rs:1039-1056`) is the same content-addressed
  insert→repoint→prune-in-one-transaction as Python, SHA-256 hashed.
- The persist-before-I/O ordering is proven live:
  `conversation_prompt_is_persisted_before_model_io_and_reused_verbatim`
  (`main.rs:906-1048`); the mock model handler asserts the row is already
  non-empty when the request lands (`main.rs:918-921`).

**Gap:** there is no `tool_names` read or write anywhere in Rust. The order to
add is exactly Python's: persist tool-name order in the **same before-I/O
window** as the prompt persist (i.e. inside/next to `restore_or_build`'s build
branch), and read it back in the reuse branch. Note the Rust reuse branch
already returns before `build_snapshot`, so a tool-restore added to the reuse
branch must do its own `session_row["tool_names"]` read - but the row is already
in hand (`row` at `conversation_prompt.rs:521`), so no extra query is needed if
the restore is folded into `restore_or_build` rather than the caller.

---

## Q2. Per-conversation owner for immutable prompt bytes, ordered tools/names, restored plugin sections, static-prefix metadata

**`NativeAgentClient` is the owner, and its lifetime already matches the
immutable prompt.** It holds `system_prompt: Option<Arc<str>>`
(`native_agent.rs:207`) and `tools: Vec<Arc<dyn Tool>>` (`native_agent.rs:212`),
both installed once at construction (`with_system_prompt` `247-250`, `with_tools`
`433-436`) and never mutated per turn - `run_turn` clones the client and only
sets `cache_scope` (`native_agent.rs:464-465`), then prepends the prompt as
`history[0]` verbatim (`468-476`). The client is cached per `(home,
session_id)` in `ConversationAgent`'s `OnceCell` (`conversation_agent.rs:26`,
`83-90`), so one client == one conversation for the life of the cache entry.

That makes `NativeAgentClient` the natural home for the remaining frozen values.
Add, as immutable-at-construction fields set by `with_*` builders (matching the
existing pattern):

- `tool_names: Vec<String>` - the frozen order actually sent, derived from
  `tools` at build time or restored from the pin. (Or: keep deriving from
  `tools` via `spec().name` - see Q3 - and store only the restored ordering
  decision. Since `tools` already carries order, a separate `tool_names` field is
  only needed if the ordered `Vec<Arc<dyn Tool>>` cannot be reconstructed; today
  it can, so this is optional now and becomes necessary when a registry exists.)
- `plugin_sections: plugin_prompt::Snapshot` - the frozen plugin bytes
  (`plugin_prompt.rs:241-279`). Seed via `Snapshot::restore(&prompt)` on reuse,
  or `get_or_render(stored, render)` on build. Empty today; correct owner going
  forward.
- `static_prefix: Option<Arc<str>>` - reserved for `reconstruct_static_prefix`.
  **Do not add yet**: native uses a single verbatim prompt block
  (`native_agent.rs:468-476`) with no two-block cache-breakpoint layout, so there
  is no static/volatile split to reconstruct. This field would be speculative
  until native emits a segmented prompt.

The owner boundary is clean because `Initializer` (`conversation_prompt.rs:65`)
owns the **process-stable** build inputs and caches (skills `PromptLoader`,
`ProtocolCache`, env captures, timezone) while `NativeAgentClient` owns the
**per-conversation frozen** outputs. `restore_or_build` is the transfer point.

---

## Q3. Mapping `_merge_preserving_prefix` onto native `Tool` trait objects

Python `_merge_preserving_prefix(current_defs, new_defs, registered_names)`
(`mcp_tool.py:8967-8998`) folds a fresh snapshot onto a live one, ordered by
`current_defs`:

- name in both → keep the slot, take the **fresh** schema (`8990-8994`);
- name only in current → keep it **iff still registered** (flapped `check_fn`),
  else drop (`8995-8996`);
- name only in fresh → **append at tail** (`8997`).

`restore_agent_tool_prefix` (`mcp_tool.py:8927-8964`) is the reconstruction entry
point: it rebuilds `saved_defs` from `saved_names`, pulling a still-registered
saved tool's schema from the registry when the fresh build dropped it
(`8946-8954`), computes `registered_names = {entry.name for entry in
registry.get_all_entries()}` (`8955`), merges (`8956`), and re-persists only if
the order changed (`8962-8963`). `persist_agent_tool_names` writes
`[t["function"]["name"] for t in agent.tools]` (`mcp_tool.py:8919-8922`).

Native mapping. The `def` dicts become `Arc<dyn Tool>` (trait at
`native_tools.rs:37-40`), whose identity is `spec().name`
(`ToolSpec.name`, `native_tools.rs:28-33`); the wire array is built by
`tools.iter().map(tool_spec_json)` in order (`native_tools.rs:400`,
`tool_spec_json` `76-85`), so **`Vec` order == wire order == prefix order**. A
faithful port:

```rust
// native_tools.rs - pure, order-preserving, no registry dependency
pub fn merge_preserving_prefix(
    saved: &[String],
    fresh: &[Arc<dyn Tool>],
    registered: &BTreeSet<String>,
) -> Vec<Arc<dyn Tool>> { /* ordered by `saved`, then fresh tail */ }
```

Case-by-case, all decidable now as a pure function even though the live registry
is a singleton:

- **in both** → keep the saved slot but swap in the fresh `Arc<dyn Tool>`
  (fresh schema wins), mirroring `fresh.pop(name)` (`mcp_tool.py:8992-8994`).
- **registered but unavailable** (probe flapped) → carried forward from the
  fresh/registry object when `registered.contains(name)`
  (`mcp_tool.py:8995`). Native has no probe, so `registered` == the set of all
  tool objects; today this branch never drops. Faithful and inert.
- **removed / deregistered** → saved name not in `registered` → dropped
  (`mcp_tool.py:8995-8996` else-path).
- **new tool** → in fresh but not saved → appended at tail
  (`mcp_tool.py:8997`).
- **duplicates in saved** → Python's `fresh` dict de-dupes on second pop
  (returns None), and the merged-name set is a `set`; the Rust port must
  likewise emit each name at most once (track a seen-set), keeping first
  occurrence, to match the byte prefix.
- **malformed stored JSON** → Python swallows via the `except` around
  `json.loads(saved_tools)` (`conversation_loop.py:1128-1135`, logs at DEBUG and
  skips restore). Rust: `serde_json::from_str::<Vec<String>>(pin)` → on `Err`,
  `tracing::debug!` and skip (fall through to the freshly-built order). Non-string
  array elements: filter them out (Python would `json.loads` them as-is then they
  never match a registry name, so they drop at merge - filtering is equivalent).
- **partial persistence failure** → Python's persist is best-effort
  (`persist_agent_tool_names` swallows, `mcp_tool.py:8923-8924`;
  `restore_or_build`'s prompt persist is best-effort too). Rust already logs and
  continues on `persist_prompt` failure (`conversation_prompt.rs:607-609`); a
  `tool_names` persist must be the same best-effort - a failed pin write must not
  fail the turn, and the next turn simply rebuilds the order.

**What blocks the *live* wiring of this merge:** it needs (a) a `tool_names`
column to read the saved list from, and (b) a **name→`Arc<dyn Tool>` registry**
so a still-registered-but-dropped saved tool can be re-materialized
(`mcp_tool.py:8948-8953`). Native has neither a registry nor any tool that a
build can drop. So: **port `merge_preserving_prefix` + its unit tests now**
(pure, non-speculative, exact behavioral parity), but **apply it in the reuse
branch only once a tool registry exists**. Until then the applied result is
always the current singleton, so wiring it changes no bytes and adds risk for no
gain.

---

## Q4. Restoring plugin frames without callback execution, with a clean seam for the future native plugin manager

The whole mechanism already exists in `plugin_prompt.rs` and is Python-golden:

- Framing/format parity with Python (`plugins.py:574-588`): `format` /
  `format_section` (`plugin_prompt.rs:281-296`), same `## Plugin Context:` +
  `hermes-plugin-section-chars:` frame and `START`/`END` markers
  (`plugin_prompt.rs:13-14`, `289-296`).
- Byte-exact recovery **without any callback**: `restore(prompt)`
  (`plugin_prompt.rs:298-344`) mirrors Python `_restore_plugin_prompt_sections`
  (`system_prompt.py:229-271`) - `rfind(START)`, require the `\n\nConversation
  started:` sentinel after `END`, re-`format` the parse and compare to reject
  lookalikes/partials. Golden test `restoration_matches_actual_python_fixtures`
  (`plugin_prompt.rs:521-534`) against `tools/plugin-prompt-goldens.json`.
- The per-agent frozen owner: `Snapshot` (`plugin_prompt.rs:241-279`) mirrors
  Python's `_plugin_system_prompt_sections_snapshot` /
  `_frozen_plugin_prompt_sections` (`system_prompt.py:191-226`).
  `Snapshot::restore` (`254-256`) == `restore_plugin_prompt_sections`
  (`system_prompt.py:274-276`); `Snapshot::get_or_render` (`258-278`) reuses
  stored bytes and **only calls the render closure on a genuinely new session**
  - the resume-never-renders contract is proven by
  `frozen_sections_survive_render_failure_and_resume_never_renders`
  (`plugin_prompt.rs:598-641`, note `panic!("resume must not execute plugins")`).
- The future plugin manager seam is `Registry` + `Content::Callback`
  (`plugin_prompt.rs:66-183`): callbacks are only ever invoked through
  `Registry::render` on a fresh build, never through `restore`. So "restore
  frames without executing callbacks" is structurally guaranteed today.

**How to make it real now, minimally and non-speculatively:** on the reuse
branch (`conversation_prompt.rs:576-587`, where `restore_frozen_sections: true`
is already set), seed a `plugin_prompt::Snapshot` on the owning
`NativeAgentClient` via `Snapshot::restore(&resolution.prompt)`. Because the
native prompt carries no frames yet, this yields an empty snapshot - harmless and
correct. It becomes load-bearing the instant either:

1. a native plugin manager renders sections into `build_fresh` (add a
   `sections.load_plugin_sections(&mut snapshot, stored, &registry, &info)` call
   - the method already exists, `system_prompt.rs:492-503`, and is exercised by
   `plugin_prompt.rs:437-446`); or
2. native gains a compression/rebuild path that re-assembles from parts and must
   reuse the frozen bytes instead of re-rendering.

Until (1), do **not** call `load_plugin_sections` in `build_fresh` - there are no
registrations to render, and adding an empty `<!-- ...:start -->...end -->`
container would change the prompt bytes and break the existing verbatim-reuse
golden (`main.rs:1041`). The clean seam is: own the `Snapshot`, restore into it,
leave `build_fresh` unchanged.

---

## Q5. Tests that prove the invariants

Existing, and what they cover:

- **Persistence-before-request + byte-for-byte reuse + no stale-source leak:**
  `conversation_prompt_is_persisted_before_model_io_and_reused_verbatim`
  (`main.rs:906-1048`). Mock model asserts the row is already persisted when the
  request arrives (`918-921`); both turns' `messages[0].content` equal the stored
  bytes (`1041`); a changed `SOUL.md` between turns does not appear (`1022`,
  `1042-1045`).
- **Restore-vs-build decision parity (reads/writes/reuse):**
  `decisions_match_actual_python_restore_helper` (`conversation_prompt.rs:646-735`)
  against `tools/conversation-prompt-goldens.json`, asserting read count, exact
  writes, `reused`, `restore_frozen_sections`, `reconstruct_static_prefix`.
- **Profile isolation + retryable single-flight + no lock over I/O:**
  `routed_clients_preserve_profile_and_conversation_identity`
  (`conversation_agent.rs:134-230`) - distinct `(home, session_id)` build
  separately, a failed build (`"broken"`) leaves the cell retryable, and the
  `get_or_try_init` design releases the map lock before the factory's
  `.await`/provider I/O (`conversation_agent.rs:84-108`).
- **Plugin restore parity + resume-never-renders + unicode/framing rejection:**
  `restoration_matches_actual_python_fixtures` (`plugin_prompt.rs:521-534`),
  `frozen_sections_survive_render_failure_and_resume_never_renders`
  (`598-641`), `restores_exact_unicode_bytes_and_rejects_modified_framing`
  (`643-675`).

Missing, to add with the checkpoint (test code only - outside this review's
no-edit scope, listed as the target):

- **`tool_names` round-trips + best-effort persist** in `session_db.rs`: write a
  name list, read it back on `get_session`; a Rust-created DB gains the column;
  a persist failure (fixture trigger, as in `session_db.rs:1757`) does not throw.
- **`merge_preserving_prefix` unit table** in `native_tools.rs` covering the six
  Q3 cases (both / registered-unavailable / deregistered / new / duplicate /
  malformed-JSON-skip), ideally golden-shared with Python
  `_merge_preserving_prefix` the way `conversation-prompt-goldens.json` is.
- **Tool-order freeze end-to-end** (deferred until a registry exists): a rebuilt
  client for an existing session sends the saved order; only meaningful once more
  than one native tool and an availability probe exist.

---

## Ordered implementation recommendation (smallest non-speculative checkpoint)

Do the parts whose subsystems exist; stub the seam for the parts that must wait.

1. **`session_db.rs` - add the `tool_names` column and methods.** Append
   `("tool_names", "TEXT")` to the `ensure_recovery_schema` column list
   (`session_db.rs:1095-1111`) so Rust-created DBs match Python
   (`hermes_state_common.py:477`). Add `update_session_tool_names(&self, id,
   Option<&[String]>)` (JSON array, `NULL` clears) mirroring
   `hermes_state.py:9635-9652`. `get_session` already returns the column via
   `SELECT s.*` (`session_db.rs:1066`). **Tests:** round-trip + best-effort.

2. **`native_tools.rs` - port `_merge_preserving_prefix` as a pure function** with
   the six-case unit table (Q3). No live caller yet. This is a faithful port, not
   a new API.

3. **`conversation_prompt.rs` - persist the tool-name order in the build branch.**
   Extend `PromptStore` with `persist_tool_names(&self, id, &[String])`
   (default no-op so the golden `Store` test double is unaffected), and call it
   right after `persist_prompt` (`conversation_prompt.rs:605-610`), best-effort
   with a `tracing::warn!` on error. Feed it `input.tools` (already the advertised
   name surface, `FreshPromptInputs.tools`, `conversation_prompt.rs:85`). Keeps
   the read → build → persist(prompt, tools) → I/O order.

4. **`native_agent.rs` - give the client the frozen owner fields + builders.** Add
   `plugin_sections: plugin_prompt::Snapshot` and (optionally) `tool_names:
   Vec<String>`, with `with_plugin_sections`/seed set at construction, matching
   the existing immutable-`with_*` pattern (`native_agent.rs:247-250`,
   `433-436`). Do **not** add `static_prefix` yet.

5. **`main.rs` / `conversation_prompt.rs` - consume `restore_frozen_sections`.**
   In the reuse branch, seed the client's `Snapshot` via
   `Snapshot::restore(&resolution.prompt)` (empty today, correct forward). Do
   **not** call `load_plugin_sections` in `build_fresh` (would change prompt
   bytes and break the verbatim golden, `main.rs:1041`).

6. **Deferred - must wait for native subsystems:** applying
   `merge_preserving_prefix` in the live reuse branch (needs a name→`Arc<dyn
   Tool>` registry and >1 tool); a plugin `Registry` rendering into `build_fresh`
   (needs the native plugin manager); `reconstruct_static_prefix` / a
   `static_prefix` field (needs a segmented two-block native prompt); any
   availability-probe / `reprobe_tool_availability` analogue (native has no
   `check_fn`). Track each as a follow-up keyed to the subsystem it needs.

Net: steps 1–5 are concrete, testable, and byte-neutral to the current wire;
they turn the inert `restore_frozen_sections` flag and the absent `tool_names`
pin into real, owned state without inventing behavior for managers that native
does not have yet.
