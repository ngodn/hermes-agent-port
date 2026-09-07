# Async conversation prompt initialization: API map and design

Independent inspection on 2026-09-08 of the current `rust-rewrite` worktree and
the Python reference. Written for the next checkpoint: make conversation
construction async, assemble the full fresh system prompt from captured session
inputs, run the stored-prompt restore/build decision, and attach the resulting
immutable bytes to a native client, all with DB reads and writes completing
before any model I/O. No implementation files were edited to produce this.

Everything below separates **Verified** (what the code does today, with
file:line) from **Design** (proposals). Line numbers were read directly, not
recalled.

---

## 1. Verified: the four building blocks already exist, but are unwired

### 1a. Immutable prompt attachment (present, dead code)

`NativeAgentClient` already carries the immutable prompt and prepends it per
turn:

- `native_agent.rs:207` , `system_prompt: Option<std::sync::Arc<str>>`. Doc:
  "Clones share the same immutable bytes, including across tool rounds;
  construction never reads files here."
- `native_agent.rs:248` , `with_system_prompt(mut self, prompt: impl Into<String>) -> Self`,
  annotated `#[allow(dead_code)] // Consumed by the pending conversation prompt assembler.`
- `native_agent.rs:465-478` , `run_turn` clones the client, sets
  `cache_scope = message_session_id(msg)`, and when `system_prompt` is `Some`
  prepends a `HistoryMessage { role: "system", content: prompt.to_string() }`
  ahead of `history`. When `None` (today's production), nothing is prepended.

Consequence: attaching bytes is a solved problem. Building with
`.with_system_prompt(bytes)` yields wire bytes byte-identical to the stored
string, because `run_turn` emits `prompt.to_string()` verbatim as `history[0]`.
The `Arc<str>` is shared across the per-turn clone and every tool round, so the
same bytes flow to the upstream prefix cache each round.

### 1b. Fresh assembly (present, staged)

`system_prompt.rs` (`#![allow(dead_code)]`) holds the whole tiered assembler:

- `ResolvedPromptSections` (`system_prompt.rs:362`) with async loaders:
  - `load_skills(loader, context, visibility, compact, detect)` , `:415`, gated
    on the tool surface containing `skills_list|skill_view|skill_manage`.
  - `initialize_stable(request, load_soul_identity, skip_context_files, StableGuidance)` , `:616` (async; reads `SOUL.md`).
  - `load_runtime_guidance(RuntimeGuidance)` , `:567` (async; provider identity,
    environment hints, coding blocks; coding is best-effort like Python).
  - `load_context(scope, ContextRequest, platform, launch_artifact, skip_context_files)` , `:641` (async; `AGENTS.md`/`.cursorrules`, install-tree fallback for cli/tui).
  - `set_memory_snapshot(snapshot, memory_enabled, user_enabled)` , `:440`.
  - `set_skills_index` / `restore_plugin_sections` / `load_plugin_sections` , `:462`, `:480`, `:492`.
  - `append_bot_chat(cache, BotChatGuidance)` , `:512` (returns the timeless flag).
  - `append_profile_platform(ProfilePlatformGuidance)` , `:546`.
  - `set_footer(Footer)` , `:456`.
  - `assemble(self) -> PromptParts` , `:670`; `PromptParts::joined()` , `:716`.
- Runtime guard: `PromptRuntime { model, provider, platform, cwd }`
  (`system_prompt.rs:753`) and `stored_prompt_matches_runtime(prompt, runtime)`
  (`:763`), oracle-tested against Python at `stored-prompt-runtime-goldens.json`.

Input structs the assembler needs (verified signatures):
- `RuntimeGuidance` (`:380`): `provider, model, platform, tools: &[String],
  config, scope: &CwdInputs, temp_root, env: &BTreeMap<OsString,OsString>,
  environment: &EnvironmentPromptInputs`.
- `StableGuidance` (`:243`): `soul, tools: &[String], model, skills_index, settings`.
- `ProfilePlatformGuidance` (`:394`): `home, root, platform, plugin_hint,
  config, overrides, desktop_terminal`.
- `BotChatGuidance` (`:404`): `enabled, title_hint, stored_title, home, config`.
- `CwdInputs` (`runtime_cwd.rs:8`): `session, terminal, launch, home`.
- `EnvironmentPromptInputs` (`environment_prompt.rs:336`): backend/local_host/
  wsl/extra_hint etc.
- `ContextRequest` (`context_files.rs:41`).
- `Footer` (`prompt_footer.rs:73`): dates, tz, `session_id, model, provider,
  platform`, `timeless`, `pass_session_id`; `resolve_start_date(db, ...)` reads
  the conversation root from the DB.
- Skills: `SourceContext` (`skill_loader.rs:72`): `config_path, home, skills,
  user_home, launch, project_start, environment: &BTreeMap<String,String>`;
  `Visibility` (`skill_loader.rs:12`): `host, termux, platform, tools, toolsets,
  disabled`. `PromptLoader` (`skill_loader.rs`) is a retained LRU owner.
- Plugins: `plugin_prompt::Registry`, `Snapshot` (with `get_or_render` /
  `restore`), rendered from a `session_info` map.
- Bot Chat: `bot_mode::ProtocolCache`, `BOT_CHAT_TITLE`, `capability_fingerprint`,
  `stored_prompt_capability_stale(stored, current)` (`bot_mode.rs:174`),
  `stored_prompt_needs_upgrade(stored, home)` (`bot_mode.rs:161`).

### 1c. Restore/build orchestration (present, staged, golden-tested)

`conversation_prompt.rs` (`#![allow(dead_code)]`):

- `restore_or_build(store: Option<&dyn PromptStore>, session_id, input: &RestoreInputs, build: impl FnOnce() -> anyhow::Result<String>) -> anyhow::Result<Resolution>`
  (`conversation_prompt.rs:52`).
- `RestoreInputs { has_history, runtime: PromptRuntime, capability_stale, legacy_bot_upgrade }` (`:16`).
- `Resolution { prompt, stored_state, reused, restore_frozen_sections,
  reconstruct_static_prefix, refreshed_capability, read_attempted, persist_attempted }` (`:23`).
- `PromptStore` is implemented for `SessionDb` (`:40`): `session_row` →
  `get_session`, `persist_prompt` → `update_system_prompt(id, Some(prompt))`.
- Behavior (matches Python, see §2): reads the row only when `has_history`;
  distinguishes Missing/Null/Empty/Present/StaleRuntime; reuses verbatim only
  when the row is a non-empty string that matches runtime AND neither
  `capability_stale` nor `legacy_bot_upgrade`; otherwise calls `build()` and
  best-effort persists.
- The golden matrix `tools/conversation-prompt-goldens.json` (12 cases) is
  exactly: first-turn (reads 0, writes 1, builds), missing/null/empty (reads 1,
  writes 1, builds), valid-reuse and ordinary-legacy-reuse (reads 1, writes 0,
  restored+reconstructed), stale-model/provider/platform/cwd + capability-stale
  + bot-legacy-upgrade (reads 1, writes 1, builds).

**Critical mismatch for async:** `build` is a synchronous `FnOnce() ->
anyhow::Result<String>`, but the real assembler in §1b is `async`
(`initialize_stable`, `load_runtime_guidance`, `load_context`, `load_soul` all
`.await`). The orchestration module cannot be driven by the async assembler as
written. This is the central seam the checkpoint must add (see §4).

### 1d. Async per-conversation client selection (present; no prompt yet)

`ConversationAgent` (`conversation_agent.rs`) already provides per-conversation
client lifetime:

- `clients: Mutex<HashMap<(PathBuf, String), Arc<dyn AgentClient>>>` (`:21,26`).
- `factory: Box<dyn Fn(&Path, &Message, &[HistoryMessage], Option<&SessionDb>) -> anyhow::Result<Arc<dyn AgentClient>>>` , **synchronous** (`:13-20`).
- `run_turn_with_context` (`:68-97`): if `context.home` is `None`, falls back to
  the shared client. Otherwise key = `(home, message_session_id(msg))`; under the
  std `Mutex` it returns a cached client or calls the factory, inserts, and
  returns. The lock is released before `client.run_turn` (model I/O). Failed
  builds are not cached and never fall back to another profile's credentials.

The production factory wired in `main.rs:550-562` currently **ignores**
`_message, _history, _database`: it only reloads `home/config.yaml` and calls
`build_agent_client_for_home(&captured, &selected, model, home)`. No prompt is
read, assembled, persisted, or attached. `with_system_prompt` is never called.

### 1e. Ingress ordering (verified)

Both live paths already resolve the session/DB and load prior history before the
agent runs, and pass a real `TurnContext`:

- `dispatch.rs:395` , `history = begin_turn(turn_db, manages, &msg, source)`
  (loads prior history, appends the inbound user message; the returned `history`
  is the *prior* transcript, current message excluded).
- `dispatch.rs:401-410` , spawns `agent.run_turn_with_context(TurnContext::from_database(agent_db), &msg, &history, tx)`.
- `message.rs:193,201` , identical for the HTTP path with `"cli"` source.
- `TurnContext::from_database` (`agent.rs:43`) derives `home` from
  `db.profile_home()` and carries `database`.
- Per-session serialization already exists: the turn lease
  (`SessionTurnLeaseRegistry::acquire`) is held across the admitted turn, so no
  two turns for one session build a prompt concurrently.

So `has_history = !history.is_empty()` is available and correct at the factory
boundary, and the factory runs before `client.run_turn` (model I/O).

---

## 2. Verified: Python reference (`agent/`, `gateway/run.py`)

`_restore_or_build_system_prompt(agent, system_message, conversation_history)` ,
`agent/conversation_loop.py:993-1227`.

- DB read gate (`:1020-1023`): `stored_prompt=None, stored_state="missing"`; read
  only `if conversation_history and agent._session_db:`. This is the has-history
  gate the Rust `RestoreInputs.has_history` mirrors.
- Read + fields (`:1025-1034`): `session_row = agent._session_db.get_session(agent.session_id)`;
  `raw_prompt = session_row.get("system_prompt")`; None→`"null"`, `""`→`"empty"`,
  else→`"present"`. On reuse it also reads `session_row.get("tool_names")` (`:1129`).
- Runtime match (`:1043`) via `_stored_prompt_matches_runtime` (`:1230-1303`):
  compares `Model`/`Provider`/`Platform` lines (last match wins) and
  `Current working directory` anchored under the `User home directory:` host-info
  block, against `agent.model/provider/platform` and `str(resolve_agent_cwd())`.
  The Rust `stored_prompt_matches_runtime` implements the same four-field guard.
- Bot Chat epoch (`:1043-1121`): after a runtime match, `capability_stale =
  stored_prompt_capability_stale(stored_prompt, home)`; if not stale and
  `agent._bot_mode_protocol`, and the resolved title (`_session_title_hint` or
  `get_session_title`) equals `BOT_CHAT_TITLE`, then `legacy_bot_upgrade =
  stored_bot_chat_prompt_needs_upgrade(stored_prompt, home)`. On either, it
  clears the skills cache, rebuilds, persists, sets `_bot_capability_refreshed`,
  and returns without re-firing `on_session_start`.
- Verbatim reuse (`:1122-1156`): `_cached_system_prompt = stored_prompt`; restore
  tool prefix (`restore_agent_tool_prefix`), plugin sections
  (`restore_plugin_prompt_sections`), and static prefix
  (`reconstruct_static_prefix`), then return. These are the Rust
  `restore_frozen_sections` / `reconstruct_static_prefix` flags , still declared
  but not yet acted on.
- Fresh build order (`:1182-1227`): **build → on_session_start hook → (credits
  seed) → persist** (`update_system_prompt` + `persist_agent_tool_names`). All in
  the per-turn prologue, before the provider call.

`build_system_prompt_parts(agent, system_message)` , `agent/system_prompt.py:435-1033`;
`build_system_prompt` joins `stable+context+volatile` (`:1036-1062`). Captured
inputs (all from the resolved `agent`/config, none from ambient globals mid-turn):
`agent.model / provider / platform`; `agent.valid_tool_names` + resolved
toolsets; the config flags `_task_completion_guidance, _parallel_tool_call_guidance,
_memory_enabled, _user_profile_enabled, _kanban_worker_guidance,
_tool_use_enforcement, _execution_guidance, _environment_probe,
_bot_mode_protocol`; `context_compressor.context_length`; `resolve_context_cwd()`
and the live workspace snapshot; `load_soul_identity/skip_context_files` +
`load_soul_md(home_override=_agent_home(agent))`; the skills index
(`build_skills_system_prompt` with tools/toolsets/compact categories/skills-dir
override); memory + user snapshots (`_memory_store.format_for_system_prompt`) and
external memory (`_memory_manager.build_system_prompt`); context files
(`build_context_files_prompt`, install-tree fallback for cli/tui);
frozen plugin sections; the footer/metadata block (tz, session start date,
timeless override, Session ID, Model/Provider/Platform); `system_message`;
environment hints + env probe line; Bot Mode protocol section + epoch line;
profile home vs `get_default_hermes_root()` for the profile-name text.

Gateway cache path , `gateway/run.py`:

- `_agent_cache: OrderedDict[str, tuple]` keyed by `ctx.session_key`, value
  `(agent, _sig, _current_msg_count, ctx.session_id)`, guarded by
  `_agent_cache_lock` (`:7792-7793`, `:6376-6386`).
- `_current_msg_count` is loaded from disk *before* the lock via
  `_session_db._db.get_session(ctx.session_id)["message_count"]` (`:6181-6189`);
  a mismatch evicts the cached agent so a fresh one re-reads history.
- **Restore runs at cache-miss, once per new agent.** It is not called from the
  gateway directly and not under the cache lock. `turn_context.py:900-902` calls
  it only `if agent._cached_system_prompt is None`. A reused cached agent already
  holds `_cached_system_prompt`, so restore is skipped and bytes are reused.
- `conversation_history` is built from disk by `_build_gateway_agent_history`
  (`:1826-1928`, skipping system rows) before `run_conversation`, so it is
  available to gate the DB read.
- `invalidate_system_prompt` (`agent/system_prompt.py:1065-1085`) nulls
  `_cached_system_prompt`/`_cached_system_prompt_static`, stashes plugin sections,
  reloads memory; called at context compression (`conversation_compression.py:4739`)
  and `/new`-style commands. Next turn re-enters restore-or-build.

**Mapping.** The Rust `ConversationAgent` cache keyed by `(home, session_id)` is
the analogue of Python's per-`session_key` agent cache, and the factory
cache-miss is the analogue of "`_cached_system_prompt is None`". So the restore
/build/attach belongs exactly at the Rust factory cache-miss, run once and
frozen, which is what the checkpoint asks for.

---

## 3. Gap analysis

Present and correct: immutable attachment (§1a), the full assembler (§1b), the
restore decision + goldens (§1c), async per-conversation client cache (§1d),
ingress ordering + prior history + `TurnContext` (§1e).

Missing to reach a live path:

1. **Async build seam.** `conversation_prompt::restore_or_build` takes a *sync*
   `build`. The assembler is async. Need an async-build entry point that still
   reuses the golden-tested decision (§4a).
2. **Async factory.** `ConversationAgent`'s factory type is sync and runs under a
   `std::sync::Mutex`. DB reads (rusqlite, blocking) and file reads (SOUL,
   context, skills, memory , async) cannot be done there without either blocking
   the async runtime or holding a sync lock across `.await` (§4b).
3. **Missing captured inputs at the factory.** The factory receives only
   `(home, msg, history, database)`. To assemble/restore it also needs, captured
   once at `ConversationAgent::new` (static per home) and threaded in:
   - `root` (`config_file::hermes_root()`) , for profile name/hint vs home.
   - OS `user_home`, `launch` cwd, `project_start`, `skills` dir, `config_path`.
   - Two env captures, taken once and immutable: `BTreeMap<OsString,OsString>`
     (coding/runtime) and `BTreeMap<String,String>` (skills `SourceContext`),
     plus the desktop-terminal flag (`HERMES_DESKTOP_TERMINAL`-style) and
     `HERMES_PLATFORM/HERMES_SESSION_PLATFORM`.
   - `EnvironmentPromptInputs` (host OS/home/cwd, WSL, extra hint).
   - Platform `overrides` and `plugin_hint` (from gateway config).
   - The retained `PromptLoader` (LRU owner) and `ProjectAdmission`, behind one
     owner shared across builds , a `Mutex`/`tokio::sync::Mutex` field.
   - `plugin_prompt::Registry` + per-conversation `Snapshot`, and `bot_mode::ProtocolCache`.
   - The **advertised tool-name surface** for prompt gating. Today the native
     runtime registers at most `CurrentTimeTool` (`native_tools.rs:722`,
     name `"current_time"`; wired in `main.rs:378`). The prompt gates key off
     `skill_view/memory/web_search/session_search/skill_manage/kanban_show`, none
     of which exist in the native tool loop yet. This is a genuine input gap: the
     assembler must be handed the *intended advertised* tool names (derived from
     config/toolsets), not the executable native tool list. Until those tools are
     ported, the skills/memory tiers will assemble empty , which is faithful to
     the current tool surface, not a bug, but must be a conscious captured input,
     not silently `["current_time"]`.
   Per-conversation dynamic inputs come from the factory args: `session_id =
   message_session_id(msg)`, `has_history = !history.is_empty()`, `platform` from
   `msg.platform`, and the stored row via `database.get_session`.
4. **Runtime identity for the guard.** `PromptRuntime { model, provider,
   platform, cwd }` must be resolved from the selected home's config and the
   captured cwd, consistently for both the guard and the footer, so a rebuilt
   prompt's identity lines match what the guard will check next turn.
5. **Frozen-section restore on reuse.** `Resolution.restore_frozen_sections` /
   `reconstruct_static_prefix` are reported but not acted on. For native OpenAI
   chat there is no Anthropic two-block static-prefix layout to reconstruct and
   no live tool re-probe to freeze, so on reuse the correct action is simply to
   attach the stored bytes verbatim. Plugin-section restore
   (`restore_plugin_sections`) matters only when a later rebuild path exists;
   with a frozen per-conversation client and no in-process compression yet, it is
   deferrable. Note this explicitly rather than pretending it is done.

---

## 4. Design: the smallest correct change

Keep every ported, golden-tested piece untouched in behavior. Add three things:
an async build seam, an async factory, and a captured-input context.

### 4a. Async build seam in `conversation_prompt`

Refactor the decision out of the sync entry point into a private helper, and add
an async sibling that shares it:

```rust
enum Decision { Reuse(String, StoredState), Build(StoredState) }

fn decide(store, session_id, input) -> (Decision, /*read_attempted*/ bool) { ... } // the existing logic

pub async fn restore_or_build_async<F, Fut>(
    store: Option<&dyn PromptStore>, session_id: &str,
    input: &RestoreInputs<'_>, build: F,
) -> anyhow::Result<Resolution>
where F: FnOnce() -> Fut, Fut: std::future::Future<Output = anyhow::Result<String>>
{
    let (decision, read_attempted) = decide(store, session_id, input);
    match decision {
        Decision::Reuse(prompt, state) => Ok(Resolution { prompt, reused: true, .. }),
        Decision::Build(state) => {
            let prompt = build().await?;                 // model-free assembly
            let persist_attempted = persist(store, session_id, &prompt);
            Ok(Resolution { prompt, reused: false, .. })
        }
    }
}
```

The existing sync `restore_or_build` keeps calling `decide` + a sync `build`, so
`conversation-prompt-goldens.json` still exercises the shared decision unchanged.
The store reads/writes are synchronous rusqlite and complete inside this function
(wrap in `spawn_blocking` only if profiling shows it matters); either way they
finish before `build().await` and before the client ever runs , which is the
ordering the checkpoint requires.

### 4b. Async factory in `ConversationAgent`, single-flight per key

Change the factory type to async and remove the sync-lock-across-work hazard by
holding the map lock only long enough to hand out a per-key init cell:

```rust
type Factory = dyn Fn(FactoryArgs<'_>) -> BoxFuture<'_, anyhow::Result<Arc<dyn AgentClient>>> + Send + Sync;
clients: Mutex<HashMap<Key, Arc<tokio::sync::OnceCell<Arc<dyn AgentClient>>>>>,
```

`run_turn_with_context`:
1. lock map, `entry(key).or_insert_with(|| Arc::new(OnceCell::new())).clone()`, unlock.
2. `let client = cell.get_or_try_init(|| self.factory(args)).await?;` , a failed
   init leaves the cell empty (OnceCell semantics) so it is retried next turn and
   never cached, and never falls back to another profile (preserves the current
   guarantee).
3. `client.run_turn(msg, history, events).await`.

The turn lease already serializes same-session turns; the `OnceCell` makes
concurrent first-turns across the *same* key safe without blocking other keys and
without holding a sync mutex across `.await`. The existing
`routed_clients_preserve_profile_and_conversation_identity` test adapts by making
its recorded factory `async`.

### 4c. Captured-input context, built once in `main.rs`

At `ConversationAgent::new` (`main.rs:550`), build one owned
`ConversationPromptContext` and move it into the async factory closure. It holds
the static-per-home inputs from §3.3 (root, user_home, launch, skills dir,
config_path, both env captures, `EnvironmentPromptInputs`, platform overrides +
plugin_hint, advertised tool names) plus the retained shared owners
(`Mutex<PromptLoader>`, `Mutex<ProjectAdmission>`, `plugin_prompt::Registry`,
`bot_mode::ProtocolCache`). Per-conversation `plugin_prompt::Snapshot` is created
per key inside the factory.

### 4d. The factory body (cache-miss path), in order

```
async fn build_conversation_client(ctx, home, msg, history, database) -> Arc<dyn AgentClient> {
    let base = build_agent_client_for_home(&ctx.config, &selected, model, home)?; // sync: reqwest pool, provider, tools
    if base.manages_history() { return base; }            // subprocess/CLI bypass, zero prompt I/O

    let session_id = message_session_id(msg);
    let runtime = PromptRuntime { model, provider, platform, cwd };
    let input = RestoreInputs {
        has_history: !history.is_empty(),
        runtime,
        capability_stale: bot_mode::stored_prompt_capability_stale(stored, home_epoch),
        legacy_bot_upgrade: title == BOT_CHAT_TITLE && bot_mode::stored_prompt_needs_upgrade(stored, home),
    };
    let store: Option<&dyn PromptStore> = database.map(|d| d as _);

    let resolution = conversation_prompt::restore_or_build_async(store, &session_id, &input, || async {
        let mut s = ResolvedPromptSections::default();
        s.load_skills(...); s.initialize_stable(...).await; s.load_runtime_guidance(...).await;
        s.load_context(...).await; s.set_memory_snapshot(...);
        s.load_plugin_sections(...); s.append_bot_chat(...); s.append_profile_platform(...);
        s.set_footer(&footer);
        Ok(s.assemble().joined())
    }).await?;
    // reuse path: resolution.prompt is the stored bytes verbatim, no assembly ran, no write happened.
    Ok(NativeAgentClient rebuilt with .with_system_prompt(resolution.prompt))  // immutable Arc<str>
}
```

Attachment: build the `NativeAgentClient` with `.with_system_prompt(resolution.prompt)`
(§1a). Because `run_turn` emits `prompt.to_string()` as `history[0]`, the wire
system message is byte-identical to `resolution.prompt`, which on the reuse path
is byte-identical to what `get_session` returned. No new trait method is needed;
`with_conversation_prompt(&self, Arc<str>) -> Arc<dyn AgentClient>` is only worth
adding later if a drift-triggered rebuild wants to swap the prompt while reusing
the existing reqwest pool.

Reuse note (§3.5): on `resolution.reused`, attach the stored bytes and stop.
`restore_frozen_sections` / `reconstruct_static_prefix` are no-ops for native
OpenAI chat today; call this out in the code comment rather than silently
dropping them. `capability_stale` / `legacy_bot_upgrade` force the build branch,
which persists the refreshed bytes, matching the goldens.

### 4e. What is explicitly out of scope for this checkpoint

Eviction/TTL and cross-process `message_count` invalidation of the
`ConversationAgent` map (Python's `_enforce_agent_cache_cap` /
`_sweep_*` / message-count drift). Context-compression `invalidate_system_prompt`
(no in-process native compression yet). Tool-prefix freeze/restore
(`tool_names` column) until the native tool surface is real. Do not attach a
partial prompt as a production fallback: on assembly error, return the error and
let `run_turn_with_context` fall through to the shared client, exactly as the
current failed-build path does.

---

## 5. Integration tests

All use a real temporary `SessionDb` (`SessionDb::open_shared`) and a local mock
provider (a `tokio`/`axum` server on `127.0.0.1:0`, as the existing
HTTP/local-model native tests do), so DB and wire behavior are both observable.
Two invariants must be *proven*, not assumed: (A) DB read+write finish before any
model I/O; (B) on reuse, the exact stored bytes are what hit the wire.

1. **Write-before-model-I/O (invariant A).** New session, `has_history=false`,
   no stored prompt. The mock provider handler, on receiving `/chat/completions`,
   opens the same `state.db` and asserts the session row's resolved
   `system_prompt` is already the freshly built, non-empty prompt (the persist
   ran before the request). Also assert `history[0]` in the request body is a
   `system` message equal to that stored value. This proves read→build→persist
   all complete before model I/O, structurally and observably.

2. **Byte-exact reuse (invariant B).** Pre-seed the session with prior history
   and a stored `system_prompt` string that matches the runtime identity lines
   (`Model/Provider/Platform/Current working directory`). Instrument the build
   closure to `panic!` if invoked. Run one turn; assert: build closure never
   ran; `update_system_prompt` was not called (row bytes unchanged); the wire
   `system` message equals the seeded bytes byte-for-byte; and no SOUL/context
   file placed on disk leaks into the prompt. Prove no assembly I/O occurred by
   putting distinctive content in `home/SOUL.md` and asserting its absence.

3. **First turn skips the read, then reuses.** Turn 1 on a fresh session:
   `restore_or_build` performs 0 reads (has_history false), builds, and persists
   (assert `reads==0`, one write, wire==persisted). Turn 2 (history now present):
   1 read, 0 writes, wire bytes identical to turn 1. Assert the `ConversationAgent`
   factory was invoked exactly once for the `(home, session_id)` key (client
   cached), and turn 2's wire system message is byte-identical to turn 1's.

4. **Two ingress paths, real turn.** Drive `dispatch.rs` (push) and `message.rs`
   (HTTP) each with two successive messages on the same channel against the mock
   provider. Assert turn 2 sends the identical `history[0]` system message as
   turn 1 on both paths, and the DB holds one persisted prompt with no second
   write.

5. **Runtime drift rebuild.** After turn 1 persists a prompt for model A,
   construct a second `ConversationAgent` (or context) pinned to model B, run turn
   2 with the same `session_id`. Assert `stored_prompt_matches_runtime` returns
   false, `update_system_prompt` overwrites the row, and the new wire prompt's
   `Model:` line is B. (Guards against a stale prompt surviving a config change.)

6. **Bot Chat capability refresh.** Stored prompt present and runtime-matching
   but with a Bot Chat title and a changed capability fingerprint: assert the
   build branch runs, the row is overwritten, and `refreshed_capability` is set ,
   mirroring the `capability-stale`/`bot-legacy-upgrade` goldens end to end.

7. **Subprocess/CLI bypass.** A backend whose `manages_history()==true` returns
   unchanged from the factory with zero `get_session`/`update_system_prompt`
   calls and no assembly, on both ingress paths.

8. **Concurrent first turns, single build.** Fire two turns for the same
   `(home, session_id)` concurrently (bypassing the lease in the unit harness);
   assert the async factory / `OnceCell` builds exactly once and both turns see
   identical bytes, and a failed build is not cached (next turn retries).

---

## 6. Anchor index

Rust: `conversation_agent.rs:13-97`, `conversation_prompt.rs:16-125`,
`system_prompt.rs:362-724,753-803`, `native_agent.rs:207,248,457-478`,
`agent.rs:37-49,62-89`, `session_db.rs:146,179-211,274-279,1039-1076`,
`dispatch.rs:383-410`, `message.rs:193-206`, `main.rs:217-391,536-565`,
`bot_mode.rs:8,161,174`, `native_tools.rs:722`, `runtime_cwd.rs:8`,
`environment_prompt.rs:336`, `prompt_footer.rs:73`, `skill_loader.rs:12,72`,
`plugin_prompt.rs:96,242`.

Python: `agent/conversation_loop.py:993-1227,1230-1303`,
`agent/system_prompt.py:435-1033,1036-1062,1065-1085`,
`agent/turn_context.py:900-902`, `gateway/run.py:1826-1928,6181-6386,7792-7793`,
`agent/conversation_compression.py:4739`.
