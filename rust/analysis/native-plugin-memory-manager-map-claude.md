# Native plugin prompt sections + external-memory management: source map

Independent source map for the next native port seam. Scope: making native
`build_fresh` actually render plugin `after_memory` sections and the external
memory provider block, and giving both a real owner and gate, without
reimplementing Python-only network providers. Read-only review; anchors are
`file:line` on branch `rust-rewrite` at the time of writing. No production,
test, or docs code was touched. This continues directly from the frozen
conversation state checkpoint (PORT.md, 2026-09-08), whose Q4 established that
`build_fresh` must not emit an empty plugin container.

## Headline

Both halves are already modeled as isolated, golden-tested Rust modules, and the
prompt struct already has the exact slots in the exact Python order. What is
missing is a live producer in the fresh build, a process/per-conversation owner
for the registry and manager, and the gate actually being consulted.

- `ResolvedPromptSections` already declares `external_memory: Option<String>`
  and `plugin_sections: Vec<String>` (`system_prompt.rs:361-376`), and
  `assemble()` already orders the volatile tier as skills, memory, user_profile,
  external_memory, plugin_sections, footer (`system_prompt.rs:684-694`,
  golden at `system_prompt.rs:1252-1255`). This matches Python's volatile order
  exactly (`system_prompt.py:925-961`: builtin memory, then external provider
  block, then `after_memory` plugin blocks, then the timestamp footer).
- `build_fresh` populates `memory`/`user_profile` via `set_memory_snapshot`
  (`conversation_prompt.rs:337-351`) but never calls `load_plugin_sections` and
  never populates `external_memory`. `plugin_hint` is passed as `""`
  (`conversation_prompt.rs:313`). So a fresh native prompt carries zero plugin
  frames and no external memory block today.
- `plugin_prompt::Registry`/`Snapshot` (`plugin_prompt.rs:95-183`, `241-284`),
  `ResolvedPromptSections::{load,restore}_plugin_sections`
  (`system_prompt.rs:480-508`), the memory gate
  `toolset_resolution::memory_provider_enabled` (`toolset_resolution.rs:111-130`)
  and the read-only `memory_snapshot` all exist and are golden-tested, but the
  gate and `load_plugin_sections` have zero production callers, and no native
  plugin registry or memory manager is constructed at startup (`main.rs` builds
  only a `ProviderRegistry`, `main.rs:284-288`, and a default empty
  `plugin_prompt::Snapshot`, `main.rs:511`).
- The native tool surface is the single `current_time` tool gated by
  `config.agent_tools` (`main.rs:205-215`); there is no `memory` tool.

Because `plugin_prompt::format(&[])` returns an empty string and an empty memory
manager yields an empty block, wiring both producers into `build_fresh` is
byte-identical to today when nothing is configured. That is the property that
protects the verbatim-reuse golden
(`conversation_prompt_is_persisted_before_model_io_and_reused_verbatim`,
`main.rs:906-1048`, asserting exact reuse at `main.rs:1041`).

## Decision: reuse the existing seams, not a manifest descriptor or an RPC adapter

The task asks whether a manifest-backed native descriptor, a subprocess/RPC
adapter, or another existing seam is the right immediate step. The honest answer
is the third: wire the already-ported `plugin_prompt::Registry`/`Snapshot` and a
thin native memory manager into `build_fresh` under the exact Python gates, and
prove the live render with fixtures. Rationale:

- A manifest-backed provider descriptor is rejected for now. Every bundled
  provider (`plugins/memory/{byterover,honcho,hindsight,holographic,mem0,
  openviking,retaindb,supermemory}`) is a Python network or SQLite client
  (`plugins/memory/__init__.py`, honcho uses httpx plus OAuth, holographic and
  retaindb use sqlite3). A descriptor that declared a provider's static
  `system_prompt_block` plus its tool schemas would advertise tools with no
  native `handle_tool_call`, which is exactly the mismatch Python guards against
  (`memory_manager.py:144-164`, the #81014 rationale). A descriptor that
  declared only a prompt block with no tools would be a fabricated capability
  claim with nothing behind it. Neither is honest.
- A subprocess/RPC adapter is the correct eventual bridge to real Python
  providers, but it re-imports the Python runtime the port is leaving and is far
  larger than this checkpoint. Defer it and name it explicitly.
- The existing seam advances both halves honestly. When nothing is configured
  (the shipped default), output stays byte-identical to today and matches
  Python's "no external provider, no plugin sections" path. When a section or a
  provider is present, the exact Python ordering, gate, and framing apply. The
  live path is proven by a fixture plugin section and a fake native provider in
  tests, not by shipping a provider that cannot work.

No new permanent core tool is added. The external-memory gate works with
`memory_tool_present = false` (`toolset_resolution.rs:120-126`), so the native
`memory` write tool stays deferred and nothing advertises it.

## Exact source contracts

### Plugin prompt sections

- Python register API `PluginContext.register_system_prompt_section`
  (`hermes_cli/plugins.py:3412-3487`): id must match `^[a-z0-9][a-z0-9._-]{0,127}$`
  (`plugins.py:563`), `content` is `str` or `Callable[[Mapping], str]`
  (`plugins.py:3431-3432`), `position` in `frozenset({"after_memory"})`
  (`plugins.py:558`), `max_chars` in `1..=4000` (`plugins.py:560`), duplicate id
  rejected before mutation. Render `render_system_prompt_sections`
  (`plugins.py:5917-6005`): iterate `sorted(ids)`, fail open per section, skip
  non-string, empty, reserved-marker, over-`max_chars`, over the 32-section cap
  and the 8000-char total. Container framing
  (`plugins.py:556-588`): `## Plugin Context: ` heading, a
  `<!-- hermes-plugin-section-chars:N -->` length frame, START/END markers.
- The Rust `plugin_prompt::Registry` mirrors all of this: id/position/max_chars
  validation before mutation (`plugin_prompt.rs:109-130`), `Content::Text` or
  `Content::Callback` (`plugin_prompt.rs:70-73`), `render_sections` with the
  32-section cap, per-section limit, reserved-marker rejection and 8000-char
  aggregate budget (`plugin_prompt.rs:188-237`), and byte-exact `restore` that
  requires the `\n\nConversation started:` sentinel and reformats to reject
  lookalikes (`plugin_prompt.rs:303-349`). Golden coverage against
  `tools/plugin-prompt-goldens.json` (`plugin_prompt.rs:527-539`).
- Frozen per-conversation ownership: `Snapshot::get_or_render` renders only on a
  genuinely new session and never on resume (`plugin_prompt.rs:263-284`,
  resume-never-renders proven at `plugin_prompt.rs:604-646`).

### External-memory management

- Gate `memory_provider_tools_enabled(enabled, disabled, memory_tool_present)`
  (`memory_manager.py:117-141`): disabled "memory" wins, then present memory
  tool, then None enabled allows, empty enabled denies, literal "memory" in
  enabled allows, else composed toolset resolution fail-closed. The Rust
  `toolset_resolution::memory_provider_enabled` (`toolset_resolution.rs:111-130`)
  is a line-for-line mirror, golden-tested
  (`toolset_resolution.rs:226-252`).
- Prompt entry `MemoryManager.build_system_prompt()`
  (`memory_manager.py:555-572`): concatenate each provider's
  `system_prompt_block()`, each in try/except, non-empty and stripped, joined
  with `\n\n`. Appended directly in `system_prompt.py:943-954` only when
  `agent._memory_manager` is truthy and the gate passes (fail open on import
  error, fail silent on build error). It is not a plugin section.
- One-external-provider invariant `MemoryManager.add_provider`
  (`memory_manager.py:473-539`): builtin always accepted, a second non-builtin
  rejected with a warning pointing at `memory.provider`, provider tool names that
  collide with `toolsets._HERMES_CORE_TOOLS` are dropped (built-ins win).
- Provider trait `MemoryProvider` (`memory_provider.py:110-416`). Abstract:
  `name`, `is_available`, `initialize(session_id, **kwargs)`,
  `get_tool_schemas`. Overridable non-network build-time surface:
  `system_prompt_block() -> str` (default ""), `unavailable_reason()`.
  Network/stateful lifecycle: `prefetch`, `queue_prefetch`, `sync_turn`,
  `handle_tool_call`, `shutdown`, and the `on_*` hooks. `is_available` and
  `system_prompt_block` are documented as non-network; everything that recalls or
  writes is.
- Selection: config key `memory.provider` drives
  `plugins/memory/__init__.py::load_memory_provider` with bundled-wins four-source
  precedence. Construction happens once in `agent_init.py:1944-2029`: build a
  `MemoryManager`, load the provider, add it only if `is_available()`, else warn
  via `_warn_memory_provider_unavailable`, then `initialize_all`, then
  `inject_memory_provider_tools`. When no provider is available,
  `agent._memory_manager` is set back to None.

### Native prompt struct and ordering (the destination)

`ResolvedPromptSections` already has the slots and order; the port only needs to
fill `external_memory` and `plugin_sections` in `build_fresh`. Setters exist:
`load_plugin_sections(snapshot, stored_prompt, registry, session_info)`
(`system_prompt.rs:492-508`), `restore_plugin_sections(stored_prompt)`
(`system_prompt.rs:480-488`), `set_memory_snapshot(snapshot, memory_enabled,
user_enabled)` (`system_prompt.rs:440-454`, fills `memory`/`user_profile` only,
never `external_memory`).

## Ownership and cache invariants

- Plugin `Registry` is process-stable, like Python's `get_plugin_manager()`
  singleton. It belongs on `Initializer` (`conversation_prompt.rs:65-115`,
  alongside the other captured-once inputs). It is immutable for one resolution
  pass; callers replace it after a registry change.
- The rendered plugin `Snapshot` is per-conversation and already owned by
  `NativeAgentClient._plugin_prompt` (`native_agent.rs:205-217`), set via
  `with_plugin_prompt_snapshot` (`native_agent.rs:258-261`). Today only the reuse
  path seeds it (`main.rs:511-534`, `Snapshot::restore(&resolution.prompt)`); the
  fresh path must now render into a `Snapshot` and carry it up so a later
  compression rebuild reuses frozen bytes rather than re-rendering. The snapshot
  stays frozen and is never read on the I/O path (`native_agent.rs:479-493`).
- The native memory manager is per-conversation (per home/profile), matching the
  Python per-agent construction and the per-home `memory.provider` config. For
  this checkpoint it holds no live external provider, so there is no prefetch
  cache and no session-switch invalidation to model yet.
- Byte-neutrality invariant: with an empty `Registry` and no external provider,
  `build_fresh` output is byte-identical to today. This is the load-bearing
  invariant for the verbatim-reuse golden (`main.rs:1041`) and for Python parity
  on the common path. Any change that emits an empty plugin container or an empty
  external block would violate it, so both producers must yield nothing when
  their inputs are empty (they already do).

## Provider selection and toolset gates

- Enabled toolsets are already read from `config["tools"]["enabled_toolsets"]`
  in `build_fresh` (`conversation_prompt.rs:175`). Disabled toolsets are
  currently hardcoded empty (`conversation_prompt.rs:176`,
  `let disabled = BTreeSet::new();`), which is a pre-existing honesty gap for the
  gate. Making the gate real means reading `config["tools"]["disabled_toolsets"]`
  the same untyped way, so an explicit disabled "memory" actually suppresses the
  block (matching `memory_manager.py:124-125`).
- `memory_tool_present` is `false` natively (no `memory` tool in the surface,
  `main.rs:205-215`), so the gate's tool-present branch is inert and correct.
- Selection honesty divergence: when `memory.provider` names a provider, Python
  loads that Python module. Native cannot, and must not silently emit an empty
  block that looks configured. The honest behavior is a one-time warning that
  external memory providers are not yet available in the native runtime, with the
  manager left None and `external_memory` left None (byte-identical to
  unconfigured). This is the single documented divergence, and it only triggers
  behind a config key that is off by default.

## Failure behavior

- Plugin render: fail open per section (`plugin_prompt.rs:199-235`); on a
  whole-render failure the `Snapshot` retains the previous sections
  (`plugin_prompt.rs:272-278`). Restore that fails framing yields an empty block
  (`plugin_prompt.rs:309-318`, `343-348`).
- External memory: `build_system_prompt` must catch per-provider errors and skip
  that provider (mirror `memory_manager.py:562-571`); the call site appends the
  block only when the manager is present and the gate passes, and swallows a
  build error silently (mirror `system_prompt.py:948-954`).
- Both producers run inside `build_fresh`, so any failure degrades to a smaller
  prompt, never a failed turn.

## I/O and transaction boundaries

- Prompt assembly (plugin render, memory `build_system_prompt`, gate) runs inside
  `build_fresh`, which completes before `persist_prompt` and before any provider
  model I/O (established order: read, then reuse or build, then persist prompt and
  tool names, then I/O; PORT.md 2026-09-08, `conversation_prompt.rs:536-673`).
- No memory provider network call may occur inside `restore_or_build`, inside the
  `SessionDb` write, or under the conversation lease or prompt-cache lock. Only
  `is_available()` and `system_prompt_block()` (both non-network per the trait)
  run at build time. In Python all recall/write work is pushed onto a threaded
  prefetch with an 8s timeout and a single-worker daemon executor
  (`memory_manager.py:616-669`, `804-876`), entirely off the synchronous
  prompt-assembly thread. The native port preserves that boundary by simply not
  implementing those methods yet; when they land they must stay off the assembly
  path and outside the DB transaction.
- SQLite writers stay serialized in the provider (Python holographic/retaindb use
  a shared connection plus a lock). That belongs to the deferred provider layer,
  not to this checkpoint.

## Tests that prove the live path

1. Fresh plugin render: build an `Initializer` whose `Registry` has one static
   `Content::Text` `after_memory` section; assert the assembled prompt carries
   the framed `## Plugin Context:` block in the after_memory slot (between
   user_profile/external_memory and footer), that a second build is byte-for-byte
   identical, and that reuse restores identical bytes without re-rendering (extend
   the pattern at `plugin_prompt.rs:604-646`).
2. Byte-neutrality: with an empty `Registry` and no provider, the assembled
   prompt equals the current output; the existing
   `conversation_prompt_is_persisted_before_model_io_and_reused_verbatim`
   (`main.rs:906-1048`) must still pass unchanged.
3. Native memory manager unit: a fake provider returning a name plus a non-empty
   `system_prompt_block`; assert the one-external limit rejects a second external,
   reserved-core-tool names are dropped, `build_system_prompt` joins with `\n\n`
   and skips a provider whose block throws (mirror `memory_manager.py:473-572`).
4. Gate consulted in `build_fresh`: with a fake provider present,
   `external_memory` is appended only when `memory_provider_enabled` passes;
   disabled "memory" suppresses it, empty enabled suppresses it, None enabled
   allows it. The pure gate is already golden (`toolset_resolution.rs:226-252`);
   this proves the call site consults it.
5. Selection honesty: `memory.provider` set to a name with no native provider
   leaves the manager None, `external_memory` None, emits one warning, and yields
   a prompt byte-identical to unconfigured.
6. No network at build time: a fake provider whose `initialize`/`prefetch` would
   panic is never invoked during `build_fresh`; only `is_available` and
   `system_prompt_block` run.

## Ordered file plan

1. `native_memory.rs` (new): port the build-time subset of the provider trait
   (`name`, `is_available`, `system_prompt_block` default "", `tool_schemas`
   default empty) and a thin `MemoryManager` (ordered providers, `add_provider`
   with the one-external limit and reserved-core-tool drop reusing a core-name
   set, `build_system_prompt` joining `\n\n` with per-provider try/skip,
   `providers()`). Reuse `toolset_resolution::memory_provider_enabled` for the
   gate rather than duplicating it. Document the omitted network lifecycle
   (`initialize`/`prefetch`/`queue_prefetch`/`sync_turn`/`handle_tool_call`/
   `on_*`) as deferred. Unit tests 3 and 6.
2. `conversation_prompt.rs`: in `build_fresh`, after `set_memory_snapshot`, (a)
   read `config["tools"]["disabled_toolsets"]` alongside the existing
   `enabled_toolsets`, construct the native `MemoryManager` from
   `config["memory"]`, and set `sections.external_memory` only when the manager
   is present, the gate passes, and the block is non-empty; (b) render plugin
   sections via the process-owned `Registry` into a per-conversation `Snapshot`
   using `load_plugin_sections`. Carry the `Snapshot` out in the build result so
   the fresh path can attach it to the client. Keep both producers byte-neutral
   when empty. Add the `Registry` (and a memory-provider source, empty for now) to
   `Initializer` captured inputs. Tests 1, 2, 4, 5.
3. `main.rs`: construct the process-stable plugin `Registry` (empty default, the
   seam a future loader or RPC bridge fills) and pass it into
   `Initializer::capture`. On the fresh path, attach the rendered `Snapshot` to
   `NativeAgentClient` via `with_plugin_prompt_snapshot` (today only the reuse
   path seeds it). Emit the `memory.provider` unavailability warning. Register no
   new tool.
4. `native_agent.rs`: ensure `with_plugin_prompt_snapshot` receives the freshly
   rendered `Snapshot` on fresh builds as well as the restored one on reuse, so
   the frozen owner is populated on both paths. No I/O-path change; the snapshot
   stays unread there.
5. Tests only: add the integration test in `main.rs` for fresh render plus
   byte-neutrality plus reuse, and the unit tables in `native_memory.rs` and
   `conversation_prompt.rs` described above. Keep the plugin fixture and the fake
   provider test-only; ship no bundled section and no provider.

## Deferred list

- All external memory provider implementations (byterover, honcho, hindsight,
  holographic, mem0, openviking, retaindb, supermemory): `initialize`,
  `prefetch`/`queue_prefetch`, `sync_turn`, `handle_tool_call`, `shutdown`, the
  `on_*` hooks, threaded prefetch with the 8s timeout, the single-worker sync
  executor, the shutdown drain, checkpoint API v2, and session-switch or
  pre-compress invalidation. These stay Python-only.
- The subprocess/RPC bridge to real Python providers. This is the eventual honest
  path to live recall in native, explicitly not this checkpoint.
- The manifest-backed provider descriptor, rejected above because it would either
  advertise unbacked tools or fabricate a capability.
- Exposing external memory provider tools on the native surface
  (`inject_memory_provider_tools`), deferred until a provider can dispatch
  `handle_tool_call`. Until then `memory_tool_present` stays false.
- The built-in `memory` write tool as a native core tool. The read-only
  `memory_snapshot` already covers the prompt side; do not add a core tool here.
- A native plugin loader and discovery (bundled, user, project, entry-point),
  callback transport, and unload/reload lifecycle. No bundled plugin ships a
  static `after_memory` section (a repo scan of `plugins/` found none), so the
  shipped default `Registry` is empty and the render is proven only by fixtures.
- MCP-derived dynamic tools, `check_fn` availability probes,
  `reconstruct_static_prefix` (native emits a single prompt block, not a
  segmented prefix), and compression-triggered prompt invalidation. Each waits on
  the named subsystem, not on this diff.

Net: steps 1 to 4 turn the inert `external_memory` slot, the unused
`load_plugin_sections`, the dormant memory gate, and the held-but-unread
`_plugin_prompt` snapshot into real, owned, gated state, proven live by fixtures,
while staying byte-identical to today whenever nothing is configured and without
claiming any Python network provider is native.
