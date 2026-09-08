# Extension-host Python audit: plugin prompt sections + one external-memory provider

Scope: the exact Python calls a long-lived Rust extension host must drive over a
line-delimited JSON (JSONL) child to (1) initialize existing plugin prompt
sections and (2) initialize at most one configured external-memory provider,
then render both into the native prompt and route the provider's tool calls.
Analysis only. No implementation code, tests, Cargo, commit, or push. Anchors are
`file:line` on branch `rust-rewrite` at the time of writing. This continues the
seam mapped in `native-plugin-memory-manager-map-claude.md` and the frozen-state
checkpoint (PORT.md, 2026-09-08); it audits the Python contract that a real host
would call rather than the pure-Rust reuse of already-ported gates.

## Headline

The Python side already exposes a clean, mostly non-network build-time surface.
The minimal child needs only three inputs bound before anything runs (an explicit
`HERMES_HOME`, a `session_id`, and `platform`), and only two provider methods are
contractually non-network (`is_available`, `system_prompt_block`). Everything
that recalls, writes, or connects is optional lifecycle the host must not call at
build time. The real hazards are narrow and enumerable: a handful of `print()`
sites in `agent_init.py` (one unconditional) that would corrupt a JSONL stdout
channel, per-import top-level execution of provider modules, and two process
globals (`sys.modules`, the per-home `PluginManager` singleton with its event-bus
thread) that the child mutates. None of the build-time calls need gateway
identity; all identity kwargs are optional.

## Minimal safe call order for a long-lived JSONL child

The child is pinned to exactly one profile home for its lifetime. Drive the
calls in this order.

### 0. Environment and profile-home binding (before any import that reads home)

- Set `HERMES_HOME` explicitly in the child's environment. `get_hermes_home()`
  (`hermes_constants.py:114`) resolves context override, then the `HERMES_HOME`
  env var, then a platform default (`~/.hermes`). `set_hermes_home_override()`
  (`hermes_constants.py:30`) is a `ContextVar` and deliberately does not touch
  `os.environ` (`hermes_constants.py:33-37`), so for a subprocess the env var is
  the correct binding. Binding by env var also silences the stderr fallback
  warning `_warn_profile_fallback_once` (`hermes_constants.py:77,108`), which
  fires once when `HERMES_HOME` is unset but a non-default profile is active.
- The `PluginManager` is cached per resolved home (`get_plugin_manager`,
  `plugins.py:6221`; cache `_plugin_managers_by_home`, `plugins.py:6167`), and
  the memory loader keys its `sys.modules` namespaces per home as well. A child
  that only ever serves one home never triggers a profile switch, so the
  per-home cache and the `_clear_plugin_submodules` eviction path
  (`plugins.py:6188`) never run. This is the safe operating mode: one child per
  profile home, home fixed at spawn.
- Set `quiet_mode`/equivalent so gated prints in `agent_init.py` stay silent, and
  redirect or capture the unconditional prints noted in the stdout section below.
- Optional gates the child may inherit from config or env: `HERMES_SAFE_MODE`
  (skips plugin discovery entirely, `plugins.py:4240`) and
  `HERMES_ENABLE_PROJECT_PLUGINS` (enables the project-local `./.hermes/plugins`
  scan, `plugins/memory/__init__.py:100`).

### 1. Plugin discovery

- Call `discover_plugins(force=False)` (`plugins.py:6298`), which joins any
  background discovery then runs `get_plugin_manager().discover_and_load(force)`
  (`plugins.py:4226`). Discovery is not a blind import scan: it reads YAML
  manifests cheaply (`_scan_directory` `plugins.py:4663`, no import), and imports
  a module's top-level code only when it actually loads that plugin
  (`spec.loader.exec_module` at `plugins.py:5514` for directory plugins,
  `ep.load()` at `plugins.py:5537` for pip entry points).
- Memory providers are explicitly NOT imported here: manifests with
  `kind == "exclusive"` are recorded and skipped
  (`plugins.py:4421`, "activate via `<category>.provider` config"). So plugin
  discovery runs plugin section registrations but leaves the memory provider for
  step 2.
- Discovery honors `HERMES_SAFE_MODE` (`plugins.py:4240`), sets `_discovered`
  as a re-entrancy guard and resets it on failure (`plugins.py:4272`), and logs
  only to stderr.

### 2. Memory provider selection

- Read the single config key `memory.provider`:
  `cfg_get(load_config(), "memory", "provider")`
  (`plugins/memory/__init__.py:646-658`; `load_config` at
  `hermes_cli/config.py:3883`, cached on file mtime/size). Empty or whitespace
  means no external provider, and the child must render no external block (byte
  neutral). This mirrors `agent_init.py:1949-1951`.
- Load exactly the configured provider with `load_memory_provider(name)`
  (`plugins/memory/__init__.py:322`). Do NOT call `discover_memory_providers()`
  (`plugins/memory/__init__.py:261`) on the hot path: it imports every candidate
  module to compute availability (`_load_provider_from_dir` at
  `:287`, `is_available()` at `:289`), executing all providers' top-level code.
  Selection precedence is bundled-wins (`plugins/memory/__init__.py:16-23,124`),
  the reverse of general plugin precedence.

### 3. Availability

- Gate on `provider.is_available()` (`memory_provider.py:127`) exactly as
  `agent_init.py:1956`. The ABC documents this as non-network, config-and-deps
  only (`memory_provider.py:128-132`). If unavailable, do not register or
  initialize; surface `provider.unavailable_reason()` (`memory_provider.py:158`)
  once and continue with no external block, matching
  `_warn_memory_provider_unavailable` and the once-per-provider guard
  (`agent_init.py:1958-1969`).

### 4. Register the provider (one-external invariant)

- `MemoryManager()` then `manager.add_provider(provider)`
  (`memory_manager.py:433,473`). The one-external-provider invariant lives here:
  a second non-builtin provider is rejected with a warning pointing at
  `memory.provider` (`memory_manager.py:483-494`). Registration also indexes
  the provider's tool names, dropping any that shadow `_HERMES_CORE_TOOLS`
  (`memory_manager.py:506-523`) or collide (`:524-533`). This is where the tool
  routing table `_tool_to_provider` is built for step 8.

### 5. Initialize (kwargs)

- Call `manager.initialize_all(session_id=..., **kwargs)`
  (`memory_manager.py:1419`), which forwards to `provider.initialize(session_id,
  **kwargs)` (`memory_provider.py:135`) and auto-injects `hermes_home` if absent
  (`memory_manager.py:1426-1428`).
- Required kwargs (ABC contract, `memory_provider.py:141-145`): `session_id`
  (positional), `hermes_home` (str), `platform` (str). A headless child can
  initialize with just these three (use `platform="cli"`).
- Optional kwargs the gateway threads when it has them
  (`agent_init.py:1971-2014`): `agent_context="primary"`, `session_title`,
  `user_id`, `user_id_alt`, `user_name`, `chat_id`, `chat_name`, `chat_type`,
  `thread_id`, `gateway_session_key`, `agent_identity` (profile name),
  `agent_workspace="hermes"`. All optional; omit any the host does not carry.
- Do NOT forward the CLI-only `warning_callback`/`status_callback`
  (`agent_init.py:1977-1979`). Those are Python callables and cannot cross a
  JSONL boundary; the gateway path already omits them, and the indicator no-ops
  when absent (`agent_init.py:2015-2018`).
- Caution: `initialize()` "may create resources, establish connections, start
  background threads" per the ABC (`memory_provider.py:135-139`). For a cloud
  provider this is where network and threads appear. See the deferral note in
  the "cannot yet be claimed" section; for the offline provider (holographic) it
  only opens a local sqlite connection (`plugins/memory/holographic/__init__.py:156-179`).

### 6. Plugin section rendering (after provider registration)

- Render only after steps 4 and 5 so the external block and the plugin sections
  land in the correct order and the same session context is used. Call
  `render_system_prompt_sections(session_info)` (module fn
  `plugins.py:6486` to `PluginManager.render_system_prompt_sections`
  `plugins.py:5917`). It triggers discovery if not yet done, freezes
  `session_info` into a `MappingProxyType` (`plugins.py:5921`), iterates sections
  sorted by id (`plugins.py:5924`), and enforces the section-count budget of 32
  (`plugins.py:5926`, `MAX_SYSTEM_PROMPT_SECTIONS` `:561`), the per-section
  `max_chars` up to 4000 (`plugins.py:5967`, `:559-560`), the reserved-marker
  skip (`plugins.py:5959`), and the 8000-char aggregate budget
  (`plugins.py:5980`, `:562`). It is fail-open: a raising section is logged and
  skipped, never fatal.
- `session_info` the host must supply matches `_plugin_session_info(agent)`
  (`system_prompt.py:162,181-188`): `session_id, model, provider, platform,
  profile_name, cwd`. No gateway identity is required here. The Rust equivalent
  metadata builder is `plugin_prompt::session_info` (`plugin_prompt.rs:24-64`),
  which produces the same key set.
- Position is fixed: only `after_memory` is a legal position
  (`SYSTEM_PROMPT_SECTION_POSITIONS = frozenset({"after_memory"})`,
  `plugins.py:558`). Registration is
  `PluginContext.register_system_prompt_section(id, content, position,
  max_chars)` (`plugins.py:3412`); `content` may be a `str` or a
  `Callable[[Mapping], str]`.

### 7. Prompt block (external-memory text)

- Get the external-memory block with `manager.build_system_prompt()`
  (`memory_manager.py:555`): concatenate each provider's `system_prompt_block()`
  with `\n\n`, skipping empties, swallowing per-provider exceptions
  (`memory_manager.py:562-572`). This block is only static provider text; recall
  content is a separate `prefetch()` path the host must not call at build time.
- The host must apply the same visibility gate the Python assembler applies so
  it never advertises a block whose tools are toolset-gated off:
  `memory_provider_tools_exposed(agent)` (`memory_manager.py:144`), which calls
  `memory_provider_tools_enabled(enabled_toolsets, disabled_toolsets,
  memory_tool_present=...)` (`memory_manager.py:117`). The Rust mirror is
  `toolset_resolution::memory_provider_enabled`
  (`toolset_resolution.rs:111-130`).

### 8. Normalized tool schemas

- Expose the provider's executable tools via `manager.get_all_tool_schemas()`
  (`memory_manager.py:903`), which dedups, skips core-name and nameless entries,
  and runs each through `normalize_tool_schema(schema)`
  (`memory_manager.py:84`). Normalization unwraps an already-wrapped OpenAI tool
  entry (`{"type":"function","function":{...}}`) back to the bare
  `{"name","description","parameters"}` shape and returns `None` for anything
  without a resolvable top-level `name` (`memory_manager.py:104-114`), the
  guard against the DeepSeek "missing field name" HTTP 400 (#47707). The host
  must ship only tools it can actually dispatch (step 9); advertising a schema
  with no live `handle_tool_call` is the #81014 mismatch.

### 9. handle_tool_call (per turn, not build time)

- Route a tool call with `manager.handle_tool_call(tool_name, args, **kwargs)`
  (`memory_manager.py:948`). It looks up `_tool_to_provider`, calls
  `provider.handle_tool_call(tool_name, args, **kwargs)` (`memory_provider.py:241`),
  and on a miss or exception returns a JSON error string via `tool_error(...)`
  (`memory_manager.py:958,966`). The return contract is a JSON string.
- Error/result envelope helpers live in `tools/registry.py`: `tool_error(message,
  **extra)` (`tools/registry.py:1345`) returns `{"error": <bounded str>, **extra}`
  via `json.dumps(..., ensure_ascii=False)`; `tool_result(...)`
  (`tools/registry.py:1360`). The registry normalization contract
  `_normalize_handler_result` (`tools/registry.py:1135`) allows only a plain
  string or the multimodal envelope `{"_multimodal": True, "content": [...]}`,
  and turns anything else into a `tool_error(..., error_type=
  "tool_result_contract", ...)` (`:1158-1163`). A JSONL host should surface these
  strings verbatim as the tool result.

### 10. EOF / shutdown

- On EOF of the request stream (or an explicit shutdown request), call
  `provider.shutdown()` (`memory_provider.py:249`) via the manager's drain path
  `MemoryManager.shutdown_all` (`memory_manager.py:1339`), which honors the
  `_SYNC_DRAIN_TIMEOUT_S = 5.0` background-executor drain
  (`memory_manager.py:80,1359`). Plugins that started an event-bus daemon thread
  (`PluginManager._event_worker`, `plugins.py:3779`) and provider prefetch
  threads (`memory_manager.py:451`) must be given the chance to drain before the
  process exits; kill-on-drop at the Rust side is the backstop, not the primary
  path.

## Hazards for a JSONL-over-stdout child

### Can write stdout (would corrupt the JSONL channel)

- Framework code (plugin discovery, memory discovery, `MemoryManager`, the
  provider ABC, `system_prompt.py`) logs only through `logging` to stderr. Clean.
- The exception is `agent/agent_init.py`, which has many `print(...)` to stdout.
  Most are gated on `not agent.quiet_mode` (for example `agent_init.py:1637-1644`),
  but the fallback-model banner at `agent_init.py:1602` and `:1604` prints
  unconditionally. A JSONL child must run quiet and must route or suppress those
  two lines, or they will corrupt the stream. Treat this as the single hard
  stdout blocker for a host that reuses `init_agent`; a purpose-built child that
  drives only the calls in the order above avoids `init_agent` and its prints
  entirely.
- Third-party provider modules may `print` at import. `discover_memory_providers`
  imports all candidates (`plugins/memory/__init__.py:261,287`), so prefer
  `load_memory_provider(name)` for just the configured one to bound the blast
  radius to one module's top-level code.

### Can block or do network

- `is_available()` and `system_prompt_block()` are the only two methods
  documented non-network (`memory_provider.py:128-132,169-176`), and the
  build-time path uses only these. Everything else can block: `initialize()` may
  connect and start threads (`memory_provider.py:135-139`); `prefetch`,
  `queue_prefetch`, `sync_turn`, `on_*` hooks are recall/write paths. Cloud
  providers do HTTP in their own paths (for example
  `hindsight._fetch_hindsight_api_version`,
  `plugins/memory/hindsight/__init__.py:238`; openviking status probe,
  `plugins/memory/openviking/__init__.py:118`). `plugins/memory/honcho/oauth_flow.py`
  can block on an interactive OAuth flow and must never be triggered from a
  headless child with no TTY.

### Mutate process globals

- Both loaders mutate `sys.modules`: directory plugins register synthetic parent
  packages and submodules (`plugins.py:5512-5521`; memory loader
  `plugins/memory/__init__.py:449-507`). A single-home child never swaps homes so
  the `_clear_plugin_submodules` eviction (`plugins.py:6188`) is not exercised,
  but a multi-home child would need it to avoid one profile's already-imported
  submodule leaking into another (the relative-import hazard documented at
  `plugins.py:6188-6219`).
- `PluginManager` is a per-home cached singleton (`plugins.py:6221`) that owns an
  event-bus daemon thread (`plugins.py:3779`) and mutable registries
  (`_system_prompt_sections`, `plugins.py:3756`).
- `MemoryManager` lazily spawns a single-worker background executor
  (`memory_manager.py:458`) and per-provider prefetch threads
  (`memory_manager.py:451`). All of these must be owned by the child and drained
  at shutdown.

### Require gateway identity the host may not have

- None of the build-time calls need gateway identity. `initialize()` requires
  only `hermes_home` + `platform` + `session_id`; `user_id`, `user_id_alt`,
  `user_name`, `chat_*`, `gateway_session_key`, `agent_identity`,
  `agent_workspace`, and `session_title` are all optional and threaded only when
  present (`agent_init.py:1982-2014`). Section callables receive only
  `_plugin_session_info` (`system_prompt.py:181-188`), which carries no gateway
  identity. So a child with `HERMES_HOME`, `platform="cli"`, and a `session_id`
  can render both halves; richer identity only improves per-user memory scoping
  for cloud providers, which are deferred anyway.

## Where the host plugs into the Rust side

For cross-reference against the ownership map, the destination is already shaped.

- Section ordering already matches Python. `ResolvedPromptSections`
  (`system_prompt.rs:362-376`) has `external_memory: Option<String>`
  (`:373`) and `plugin_sections: Vec<String>` (`:374`), and `assemble()`
  (`system_prompt.rs:670,684-694`) orders the volatile tier skills, memory,
  user_profile, external_memory, plugin_sections, footer. This is the same order
  as Python's volatile tail (`system_prompt.py:925-961`: skills, builtin memory,
  user profile, external provider block at `:943-954`, `after_memory` plugin
  sections at `:959-961`, footer). Both `external_memory` and the render path are
  currently never populated in production (`external_memory` has no writer;
  `system_prompt::load_plugin_sections` at `system_prompt.rs:492` has zero
  production callers), so this is the exact insertion point.
- The per-conversation call site is `build_conversation_client`
  (`main.rs:435-546`): the host `initialize` belongs between `restore_or_build`
  (`main.rs:510`) and `NativeConversationState` assembly (`main.rs:540`) so the
  rendered sections land inside the persisted prompt bytes (persist-before-return
  ordering enforced at `conversation_prompt.rs:651`), and the executable memory
  `Tool` joins the `tools` vector attached at `main.rs:407-408`.
- The fresh-prompt splice is in `build_fresh`
  (`conversation_prompt.rs:150`): fill `external_memory` and `plugin_sections`
  after the memory block at `conversation_prompt.rs:351`, before the footer at
  `:381`. Note `disabled_toolsets` is currently hardcoded empty
  (`conversation_prompt.rs:176`, `let disabled = BTreeSet::new();`); making the
  memory gate real means reading `config["tools"]["disabled_toolsets"]` the same
  untyped way `enabled_toolsets` is read at `:175`.
- The per-process / per-profile host pool would be owned next to `Initializer`
  (`conversation_prompt.rs:70,95`) at its construction in `main.rs:698`, captured
  into the `ConversationAgent` closure (`main.rs:702-721`). The comment at
  `main.rs:752-754` already flags the missing process registry and its liveness
  probe.
- The executable memory tool would implement the sync `Tool::call`
  (`native_tools.rs:37-39`, template `CurrentTimeTool` at `native_tools.rs:783`)
  and forward to the host child, blocking on a runtime handle as the doc comment
  at `native_tools.rs:35-36` anticipates. It must be part of `fresh_tools` /
  `registered_tools` so `restore_tool_prefix` (`native_tools.rs:50-106`) keeps it
  across resumes, and gated by `toolset_resolution::memory_provider_enabled`
  (`toolset_resolution.rs:111`).
- There is no existing long-lived JSONL child in the tree. Every current spawn is
  one-shot. The closest precedent to cite for the framing is
  `SubprocessAgentClient::run_turn` (`agent.rs:213-269`): `tokio::process::Command`
  spawn, prompt to `child.stdin`, and line-delimited JSON read from stdout via
  `BufReader::new(stdout).lines()` + `next_line().await` deserializing each line
  (`agent.rs:251-263`). For crash/timeout/child-cleanup idioms, `git_probe.rs`
  uses `.kill_on_drop(true)` plus an explicit timeout+kill path
  (`git_probe.rs:61,103,108`); `control_socket.rs:210-213` shows the
  line-delimited request/response framing with a per-request timeout.

## Test fixtures: real discovery and a real provider contract, no network

- Real plugin discovery, no network: follow
  `tests/plugins/memory/test_discovery_sources.py`. It exercises the memory
  discovery contract for project-local dirs and pip entry points with an in-file
  provider source string (`PROVIDER_SOURCE`, `:22`) whose `is_available()`
  returns True and whose `register(ctx)` calls `ctx.register_memory_provider`,
  plus a `FakeEntryPoint`/`FakeEntryPoints` pair mirroring `importlib.metadata`
  (`:46-65`) and an `entry_points` monkeypatch fixture (`:68`). A host fixture
  should point `HERMES_HOME` at a tmp dir, drop a manifest + provider dir under
  it, and assert `discover_plugins()` + `render_system_prompt_sections()`
  produces the framed `## Plugin Context:` block for a registered `after_memory`
  section.
- Real provider contract, no network, using the offline sqlite provider:
  `holographic` (`plugins/memory/holographic/`) is fully local
  (`import sqlite3`, HRR vectors) with `is_available()` unconditionally True
  (`plugins/memory/holographic/__init__.py:126-127`) and no credentials.
  `initialize()` opens a sqlite db under `HERMES_HOME`
  (`plugins/memory/holographic/__init__.py:156-179`) and `system_prompt_block()`
  reads that db (`:181-`). Model the fixture on
  `tests/plugins/memory/test_holographic_store.py` (tmp `db_path`, autouse
  `_clean_shared_registry`): set `memory.provider` to the holographic key, drive
  select to `load_memory_provider`, `is_available`, `add_provider`,
  `initialize_all(session_id, platform="cli", hermes_home=<tmp>)`, then
  `build_system_prompt()` and `get_all_tool_schemas()`, asserting a non-empty
  block and normalized schemas. This proves the full register to render to
  schema contract end to end offline.
- Manager/provider seam without any real provider:
  `tests/agent/test_memory_provider.py` uses an in-file `FakeMemoryProvider`
  (`:18`) to exercise `MemoryProvider`, `MemoryManager`, and
  `inject_memory_provider_tools` purely in memory. Use it to prove the
  one-external limit (`memory_manager.py:483-494`), the reserved-core-tool drop
  (`:506-523`), the `\n\n` join with per-provider skip in `build_system_prompt`
  (`:562-572`), and the `handle_tool_call` error envelope (`:958,966`).
- The Rust-side proofs stay as in the prior map: fresh plugin render into a
  `Snapshot`, byte-neutrality when nothing is configured (protecting the
  verbatim-reuse golden at `main.rs:1041`), the gate consulted at the call site,
  and selection honesty when `memory.provider` names a provider the child cannot
  serve.

## What cannot yet be claimed

- No live external memory recall or writes in native. Only `is_available` and
  `system_prompt_block` are proven non-network; `initialize`, `prefetch`,
  `queue_prefetch`, `sync_turn`, `handle_tool_call`, `shutdown`, and the `on_*`
  hooks (`memory_provider.py:178-400`) are deferred lifecycle. Until the JSONL
  host actually spawns and drives the child, no cloud provider's recall path can
  be claimed native.
- No claim that any given bundled provider works offline except holographic.
  retaindb has a local sqlite mode but its cloud path needs `RETAINDB_API_KEY`
  (`plugins/memory/retaindb/__init__.py:552-553`); honcho, byterover, hindsight,
  mem0, openviking, and supermemory are network or CLI clients. The offline
  contract can be proven only against holographic today.
- No bundled plugin ships a static `after_memory` section (a scan of `plugins/`
  found none, consistent with the prior map). So the shipped default plugin
  registry is empty and the plugin-render path can be proven only by a test
  fixture, not by a real bundled section.
- No existing long-lived subprocess pattern in the Rust tree to inherit; the
  JSONL child protocol, its timeout/crash/cleanup semantics, and its
  stdout-contamination defense are all new work (the agy protocol task covers the
  wire shape). The one-shot `SubprocessAgentClient` framing is a template, not a
  drop-in.
- No claim that `init_agent` can be reused verbatim as the child entry point: its
  unconditional prints (`agent_init.py:1602,1604`) and its broad startup work
  make a purpose-built entry that drives only the ten steps above the safer path.
- Multi-home reuse inside one child is not claimed. The safe mode audited here is
  one child per profile home with `HERMES_HOME` fixed at spawn; the per-home
  singleton swap and `sys.modules` eviction (`plugins.py:6188`) would need
  separate proving before a single child could serve multiple profiles.
