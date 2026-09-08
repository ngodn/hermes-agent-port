# Hermes Rust Port: Native Plugin Prompt Rendering and External Memory Provider Manager Map

This document maps the smallest correct next implementation checkpoint for the Hermes Rust gateway: connecting native plugin prompt rendering and establishing an external memory provider manager boundary. It identifies what currently exists in the Rust codebase, what remains a parity test helper, which concrete fixture proves fresh prompt rendering without Python runtime dependencies, the narrow Rust trait and lifecycle guarantees needed for external memory, the deferred Python-only behaviors, and an ordered file-level implementation and verification plan.

---

## 1. Executive Summary and Scope

The current Rust port has completed two major native conversation checkpoints documented in [rust/PORT.md](file:///home/eins0fx/development/hermes-agent-port/rust/PORT.md#L3-L86):
1. **Live native conversation prompt checkpoint (2026-09-08)**: Asynchronous initial prompt construction in [conversation_prompt.rs](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/conversation_prompt.rs) attaching immutable system prompts before model calls.
2. **Frozen native conversation state checkpoint (2026-09-08)**: Exact persisted tool prefix preservation across resume and frozen plugin prompt snapshot ownership in [native_agent.rs](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/native_agent.rs#L205-L217) via [plugin_prompt.rs](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/plugin_prompt.rs#L241-L284).

However, in the live conversation construction path, fresh system prompts still omit plugin prompt sections and external memory provider blocks. The next logical and architectural milestone is to wire active plugin prompt rendering into fresh conversation prompt builds and establish a clean, single-provider external memory manager boundary.

---

## 2. Question 1: Existing Native Plugin Registry vs Parity Helper

### 2.1 What Already Exists in the Rust Codebase

The Rust codebase already contains substantial infrastructure for plugin prompt sections in [rust/crates/hermes-gateway/src/plugin_prompt.rs](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/plugin_prompt.rs):

1. **The Registry Container** ([`plugin_prompt::Registry`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/plugin_prompt.rs#L95-L183)):
   - Stores entries in a `BTreeMap<String, Registration>` sorted alphabetically by section identifier.
   - Provides [`register`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/plugin_prompt.rs#L101-L145) with fail-closed validation:
     - Section ID must match `^[a-z0-9][a-z0-9._-]{0,127}$`.
     - Section position must equal `"after_memory"`.
     - Length limit `max_chars` must be between 1 and 4,000 characters.
     - Duplicate IDs are rejected with the registered owner name.
   - Issues a [`Handle`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/plugin_prompt.rs#L84-L93) carrying `Arc<AtomicBool>` for stale-handle disposal safety.
   - Provides [`dispose`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/plugin_prompt.rs#L147-L156) verifying pointer equality on the atomic flag before removing entries.
   - Provides [`unload`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/plugin_prompt.rs#L158-L167) evicting all registrations belonging to a given owner and marking their handles inactive.
   - Provides [`render`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/plugin_prompt.rs#L169-L182) delegating to `render_sections`.

2. **Session Info Metadata Capture** ([`plugin_prompt::session_info`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/plugin_prompt.rs#L24-L64)):
   - Extracts immutable-at-render-time session metadata matching Python: `session_id`, `model`, `provider`, `platform`, `profile_name`, and `cwd`.

3. **Prompt Section Assembly and Restoration** ([`plugin_prompt::Snapshot`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/plugin_prompt.rs#L241-L284), [`format`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/plugin_prompt.rs#L286-L292), and [`restore`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/plugin_prompt.rs#L303-L349)):
   - Uses exact canonical framing markers: `<!-- hermes-plugin-sections:start -->` and `<!-- hermes-plugin-sections:end -->`.
   - Formats each section header as `## Plugin Context: <id>` followed by `<!-- hermes-plugin-section-chars:<len> -->`.
   - Reconstructs exact sections from stored prompt strings without executing plugin code.
   - Enforces the strict terminal anchor rule requiring `\n\nConversation started:` immediately after the closing tag.

4. **System Prompt Integration Hook** ([`system_prompt::ResolvedPromptSections`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/system_prompt.rs#L362-L376)):
   - Contains fields `pub external_memory: Option<String>` and `pub plugin_sections: Vec<String>`.
   - Exposes [`load_plugin_sections`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/system_prompt.rs#L492-L508) calling `snapshot.get_or_render` with `registry.render(session_info)`.
   - Exposes [`restore_plugin_sections`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/system_prompt.rs#L480-L488).
   - In [`assemble`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/system_prompt.rs#L670-L696), orders volatile components in exact parity with Python: skills, built-in memory, user profile, external memory, plugin sections, and footer.

### 2.2 What Is Still Only a Parity Helper

Despite the above components, the live runtime does not actively use `plugin_prompt::Registry`. Specifically:

1. **Absence in Live Construction**:
   - In [main.rs](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/main.rs#L435-L546) (`build_conversation_client`), no instance of `plugin_prompt::Registry` is created or passed.
   - On line 511 of [main.rs](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/main.rs#L511), `let mut plugin_prompt = plugin_prompt::Snapshot::default();` is instantiated directly. When resuming a stored session, `plugin_prompt.restore(&resolution.prompt)` is invoked, but on fresh session builds, the snapshot remains completely unrendered.
   - In [conversation_prompt.rs](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/conversation_prompt.rs#L150-L384) (`build_fresh`), `sections.load_plugin_sections` is never invoked. The `sections.plugin_sections` vector remains empty in all live prompt builds.

2. **Scope of the Existing Registry**:
   - `plugin_prompt::Registry` is purely an in-memory prompt-section dictionary.
   - It is not a general plugin manager. It does not discover plugins from disk, parse plugin manifests (`plugin.yaml` or `manifest.json`), load dynamic libraries, dispatch lifecycle hooks (`on_turn_start`, `on_tool_call`, `on_turn_end`), register tools, or inspect middleware.
   - It exists today as a unit-tested parity helper for validating and formatting the prompt-section slice of Python's `PluginManager` ([hermes_cli/plugins.py](file:///home/eins0fx/development/hermes-agent-port/hermes_cli/plugins.py#L3412-L3488)).

---

## 3. Question 2: Bundled or Fixture Plugin for Fresh Prompt Rendering

### 3.1 Investigation of Bundled Plugins

An inspection of the 19 plugin directories in [plugins/](file:///home/eins0fx/development/hermes-agent-port/plugins) (`browser`, `context_engine`, `cron_providers`, `dashboard_auth`, `disk-cleanup`, `google_meet`, `hermes-achievements`, `image_gen`, `kanban`, `memory`, `model-providers`, `observability`, `platforms`, `security-guidance`, `spotify`, `teams_pipeline`, `video_gen`, `web`) reveals that **zero bundled plugins register a prompt section**.

Bundled plugins in Python register hooks, tools, platform integrations, or external memory providers, but none invoke `ctx.register_system_prompt_section`.

### 3.2 Canonical Python Test Fixtures

In the Python reference test suite, fresh plugin prompt rendering is tested using synthetic fixture plugins:
- In [tests/agent/test_plugin_prompt_sections.py](file:///home/eins0fx/development/hermes-agent-port/tests/agent/test_plugin_prompt_sections.py#L30-L41), the test helper `_install_test_section` constructs:
  ```python
  ctx = PluginContext(
      PluginManifest(name="example-plugin", key="example-plugin", source="user"),
      manager,
  )
  ctx.register_system_prompt_section(
      "example.rules",
      content,
      position="after_memory",
      max_chars=1000,
  )
  ```
- In [tests/hermes_cli/test_plugin_prompt_sections.py](file:///home/eins0fx/development/hermes-agent-port/tests/hermes_cli/test_plugin_prompt_sections.py#L36), tests register `"example.rules"` with static text or callbacks.

### 3.3 Concrete Proof Mechanism Without Runtime Python

To prove fresh plugin prompt rendering in Rust without invoking Python at runtime, the gateway should adopt the same canonical fixture pattern:
1. **Fixture Plugin Registration**:
   - Owner identifier: `"example-plugin"`
   - Section identifier: `"example.rules"`
   - Position: `"after_memory"`
   - Max characters: `1000`
   - Content: static text such as `"Rules: execute turns deterministically"` or a closure reading `session_info["session_id"]`.
2. **Execution Seam**:
   - The fixture is registered directly into `plugin_prompt::Registry` during conversation initialization or test setup.
   - When `conversation_prompt::Initializer::build_fresh` runs, it invokes `sections.load_plugin_sections(&mut snapshot, None, &registry, &session_info)`.
   - The assembled prompt will contain:
     ```text
     <!-- hermes-plugin-sections:start -->
     ## Plugin Context: example.rules
     <!-- hermes-plugin-section-chars:40 -->

     Rules: execute turns deterministically
     <!-- hermes-plugin-sections:end -->
     ```
3. **Explicit Label of Uncertainty**:
   - Finding: It is uncertain whether any third-party or future bundled plugin will ever require dynamic file discovery for prompt sections alone.
   - Rationale: Because all bundled prompt sections in the entire repository are test fixtures, providing an in-memory registration API on `plugin_prompt::Registry` completely satisfies current system prompt requirements without building an unnecessary disk scanner or dynamic loader.

---

## 4. Question 3: Narrow Rust Trait and Lifecycle for External Memory

### 4.1 Comparison with Python Architecture

In Python, external memory providers implement the `MemoryProvider` abstract base class in [agent/memory_provider.py](file:///home/eins0fx/development/hermes-agent-port/agent/memory_provider.py#L110-L250) and are coordinated by `MemoryManager` in [agent/memory_manager.py](file:///home/eins0fx/development/hermes-agent-port/agent/memory_manager.py#L473-L540). Key characteristics include:
- Built-in memory (`"builtin"`) handles local Markdown and SQLite storage (`MEMORY.md` and `USER.md`).
- At most **one** external provider is allowed. If an external provider is already registered, any subsequent attempt is rejected ([agent/memory_manager.py:483-495](file:///home/eins0fx/development/hermes-agent-port/agent/memory_manager.py#L483-L495)).
- The system prompt block returned by `provider.system_prompt_block()` provides static instructions and status.
- Tool schemas returned by `provider.get_tool_schemas()` are injected into the agent's tool surface ([agent/memory_manager.py:167-226](file:///home/eins0fx/development/hermes-agent-port/agent/memory_manager.py#L167-L226)).
- Recall context is prefetched prior to the turn via `prefetch()`.
- Completed turns are synced via `sync_turn()` asynchronously on a dedicated background thread ([agent/memory_manager.py:744-800](file:///home/eins0fx/development/hermes-agent-port/agent/memory_manager.py#L744-L800)).

### 4.2 The Narrow Rust Trait Contract

In Rust, built-in memory is already completely handled by [`crate::memory_snapshot::MemorySnapshot`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/memory_snapshot.rs#L1-L50) and staged in `sections.set_memory_snapshot` in [system_prompt.rs](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/system_prompt.rs#L440-L455).

Therefore, the external memory provider abstraction in Rust needs only to represent external backends. The narrow trait contract is:

```rust
pub trait ExternalMemoryProvider: Send + Sync {
    /// Canonical provider name (e.g. "fixture-memory", "honcho", "hindsight").
    fn name(&self) -> &str;

    /// Fast, non-blocking check whether credentials and configuration are present.
    fn is_available(&self) -> bool;

    /// Return static instructions or status to inject into system prompt.
    /// Must be fast and non-blocking. Returns None if no prompt block is needed.
    fn system_prompt_block(&self) -> Option<String>;

    /// Return executable tools exposed by this provider.
    /// Uses the existing native_tools::Tool trait.
    fn tools(&self) -> Vec<std::sync::Arc<dyn crate::native_tools::Tool>>;

    /// Return prefetched recall context for the incoming query, or None.
    fn prefetch(&self, query: &str, session_id: &str) -> Option<String>;

    /// Synchronize a completed turn. May perform background network I/O.
    fn sync_turn(
        &self,
        user_content: &str,
        assistant_content: &str,
        session_id: &str,
    ) -> Result<(), String>;

    /// Clean shutdown to flush pending background writes.
    fn shutdown(&self);
}
```

Notice that provider tools reuse [`crate::native_tools::Tool`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/native_tools.rs#L37-L40) directly:
```rust
pub trait Tool: Send + Sync {
    fn spec(&self) -> ToolSpec;
    fn call(&self, args: &Value) -> Result<String>;
}
```
This guarantees that external memory tools integrate seamlessly into tool execution, tool pairing, and frozen tool prefix restoration without separate dispatch logic.

### 4.3 The Single-Provider Manager Boundary

The manager enforces the single external provider invariant:

```rust
pub struct ExternalMemoryManager {
    provider: Option<std::sync::Arc<dyn ExternalMemoryProvider>>,
}

impl ExternalMemoryManager {
    pub fn new() -> Self {
        Self { provider: None }
    }

    pub fn register_provider(
        &mut self,
        provider: std::sync::Arc<dyn ExternalMemoryProvider>,
    ) -> Result<(), String> {
        if let Some(existing) = &self.provider {
            return Err(format!(
                "Rejected memory provider '{}' (external provider '{}' is already registered. \
                 Only one external memory provider is allowed at a time.)",
                provider.name(),
                existing.name()
            ));
        }
        if provider.is_available() {
            self.provider = Some(provider);
        }
        Ok(())
    }

    pub fn active_provider(&self) -> Option<&std::sync::Arc<dyn ExternalMemoryProvider>> {
        self.provider.as_ref()
    }
}
```

Selection is driven by configuration:
- Read `config["memory"]["provider"]`.
- If unset or empty, `self.provider` remains `None`.
- If configured and matching a registered provider that reports `is_available() == true`, it becomes active.

### 4.4 Toolset Gating

In Python, issue #81014 established that if the `"memory"` toolset is gated off in configuration, **both** the provider's tools and its system prompt block must be withheld ([agent/system_prompt.py:943-955](file:///home/eins0fx/development/hermes-agent-port/agent/system_prompt.py#L943-L955) and [agent/memory_manager.py:179-196](file:///home/eins0fx/development/hermes-agent-port/agent/memory_manager.py#L179-L196)).

The Rust port already implements this exact policy in [rust/crates/hermes-gateway/src/toolset_resolution.rs](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/toolset_resolution.rs#L111-L130):
```rust
pub fn memory_provider_enabled(
    &self,
    enabled: Option<&[String]>,
    disabled: &[String],
    memory_tool_present: bool,
) -> bool
```

The gateway must evaluate this gate before populating prompt sections or exposing tools:
1. If `memory_provider_enabled` returns `false`:
   - `sections.external_memory` remains `None`.
   - Provider tools are omitted from `available_native_tools`.
2. If `memory_provider_enabled` returns `true`:
   - `sections.external_memory` receives `provider.system_prompt_block()`.
   - Provider tools are added to `available_native_tools`.

### 4.5 Isolation of Network Work from SQLite and Prompt Locks

To preserve system reliability and prevent deadlocks or request stalls, the manager boundary enforces three strict concurrency and lock isolation invariants:

1. **Prompt Assembly and Prompt-Cache Lock Isolation**:
   - `provider.system_prompt_block()` must return static text or locally cached parameters. It must never perform network requests or wait on remote sockets.
   - Assembling `ResolvedPromptSections` and calculating prompt parts must execute purely in-memory.
   - Any prompt-cache lock or per-session single-flight initialization future must not hold locks across network calls.

2. **SQLite Transaction Boundary Isolation**:
   - Database reads and writes in [`session_db::SessionDb`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/session_db.rs) (`get_session`, `update_session_prompt`, `update_session_tool_names`) are short, single-statement operations executed on pooled connections.
   - SQLite connections are never held open across external memory operations.
   - Persisting the assembled prompt occurs before model requests, and persisting tool names occurs immediately after, with no external memory I/O nested inside.

3. **Decoupled Asynchronous Turn Sync**:
   - As documented in Python's [agent/memory_manager.py:754-764](file:///home/eins0fx/development/hermes-agent-port/agent/memory_manager.py#L754-L764), network calls during `sync_turn` can block for hundreds of seconds if a remote daemon or API is degraded.
   - In Rust, `sync_turn` must be dispatched into a background task (such as `tokio::spawn` or a bounded MPSC channel) after the turn response is delivered to the client.
   - Turn execution, prompt construction, and SQLite persistence complete independently of external memory sync outcomes.

---

## 5. Question 4: Deferred Python-Only Behaviors

The following features must be explicitly deferred in this checkpoint because their current implementations rely on Python libraries, dynamic import mechanisms, or external services:

1. **Concrete Third-Party Provider Implementations**:
   - The eight bundled external memory plugins in [plugins/memory/](file:///home/eins0fx/development/hermes-agent-port/plugins/memory) (`honcho`, `hindsight`, `mem0`, `openviking`, `holographic`, `byterover`, `retaindb`, `supermemory`) are written in Python.
   - They rely on Python SDK packages (such as `honcho-ai`, `mem0ai`, `requests`), local Python daemons, or Python entry points (`hermes_agent.memory_providers`).
   - Implementing native Rust network clients for these eight third-party services is deferred.

2. **Dynamic Plugin Directory Discovery**:
   - The filesystem discovery logic in [plugins/memory/\_\_init\_\_.py](file:///home/eins0fx/development/hermes-agent-port/plugins/memory/__init__.py#L1-L100) dynamically inspects `sys.modules`, creates synthetic namespace packages, and scans user directories (`$HERMES_HOME/plugins/`) and project directories (`./.hermes/plugins/`).
   - Rust cannot dynamically import Python modules without embedding a full CPython runtime.

3. **Interactive CLI Setup Wizards and Schemas**:
   - Configuration schema validation and interactive CLI prompts ([plugins/memory/config_schema.py](file:///home/eins0fx/development/hermes-agent-port/plugins/memory/config_schema.py)) are tied to the Python CLI terminal interface and are deferred.

4. **Advanced Lifecycle and Secondary LLM Hooks**:
   - Checkpoint API version 2 evidence extraction during `on_pre_compress` ([agent/memory_provider.py:113-117](file:///home/eins0fx/development/hermes-agent-port/agent/memory_provider.py#L113-L117)).
   - Session-end background summarization (`on_session_end`) that spawns secondary LLM extraction tasks.
   - Query rewriting before prefetch ([plugins/memory/query_rewrite.py](file:///home/eins0fx/development/hermes-agent-port/plugins/memory/query_rewrite.py)).

5. **Streaming Context Scrubber**:
   - Filtering `<memory-context>...</memory-context>` tags from real-time streaming model tokens ([agent/memory_manager.py:232-235](file:///home/eins0fx/development/hermes-agent-port/agent/memory_manager.py#L232-L235)).

---

## 6. Question 5: Ordered File-Level Implementation and Tests

To deliver an honest, verifiable runtime checkpoint without scope creep, the implementation proceeds in five file-level steps followed by integration testing.

### 6.1 Step 1: Plugin Prompt Registry Sharing in `plugin_prompt.rs`

- **File**: [`rust/crates/hermes-gateway/src/plugin_prompt.rs`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/plugin_prompt.rs)
- **Changes**:
  - Ensure `Registry` can be shared across thread boundaries, either via `Arc<Registry>` for immutable/pre-configured setups or `Arc<std::sync::RwLock<Registry>>` if runtime registration is mutated.
  - Implement a helper `Registry::fixture_example()` that registers owner `"example-plugin"`, ID `"example.rules"`, position `"after_memory"`, and max chars `1000`, matching Python's test suite.

### 6.2 Step 2: New Module `external_memory.rs`

- **File**: `rust/crates/hermes-gateway/src/external_memory.rs` (new file)
- **Changes**:
  - Define `ExternalMemoryProvider` trait with methods: `name`, `is_available`, `system_prompt_block`, `tools`, `prefetch`, `sync_turn`, and `shutdown`.
  - Define `ExternalMemoryManager` enforcing at most one external provider.
  - Define a concrete test fixture provider: `FixtureMemoryProvider`:
    - Name: `"fixture-memory"`
    - System prompt block: `"External Memory: User prefers deterministic execution."`
    - Tool: `memory_recall` tool implementing `native_tools::Tool`.

### 6.3 Step 3: Wire Prompt Loading in `conversation_prompt.rs`

- **File**: [`rust/crates/hermes-gateway/src/conversation_prompt.rs`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/conversation_prompt.rs)
- **Changes**:
  - Update `FreshPromptInputs` to accept:
    - `plugin_registry: Option<&plugin_prompt::Registry>`
    - `memory_manager: Option<&external_memory::ExternalMemoryManager>`
    - `toolset_resolver: Option<&toolset_resolution::ToolsetResolver>`
  - In `Initializer::build_fresh`:
    - After built-in memory is staged, inspect `memory_manager`.
    - If `memory_manager` has an active provider and `toolset_resolver.memory_provider_enabled(...)` is true, set `sections.external_memory = provider.system_prompt_block()`.
    - Capture `session_info = plugin_prompt::session_info(...)`.
    - If `plugin_registry` is provided, call `sections.load_plugin_sections(&mut snapshot, None, registry, &session_info)`.

### 6.4 Step 4: Tool Registration and Client Construction in `main.rs`

- **File**: [`rust/crates/hermes-gateway/src/main.rs`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/main.rs)
- **Changes**:
  - Declare `mod external_memory;`.
  - Update `registered_native_tools`: include memory provider tools when an external provider is present.
  - Update `available_native_tools`: filter memory tools through `toolset_resolver.memory_provider_enabled(...)`.
  - In `build_conversation_client`:
    - Pass the configured `Registry` and `ExternalMemoryManager` into `conversation_prompt::FreshPromptInputs`.
    - When `restore_or_build` resolves, construct the agent client with the resulting system prompt, tools, and `Snapshot`.

### 6.5 Step 5: Integration Tests in `main.rs`

- **File**: [`rust/crates/hermes-gateway/src/main.rs`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/main.rs#L970-L1103)
- **Test Plan**: Extend the existing SQLite + local HTTP mock integration test:
  1. **Fresh Build with Plugin Section and External Memory**:
     - Register fixture plugin `"example-plugin"` with section `"example.rules"`.
     - Configure `"memory.provider": "fixture-memory"`.
     - Enable toolsets including `"memory"`.
     - Run turn 1.
     - Verify stored prompt in `SessionDb` contains:
       - Identity prefix from `SOUL.md`.
       - External memory block `"External Memory: User prefers deterministic execution."`.
       - Plugin section framing with header `## Plugin Context: example.rules`.
       - Terminal footer anchor `Conversation started:`.
     - Verify outgoing mock HTTP request receives both `current_time` and `memory_recall` tools.
  2. **Gated Toolset Test**:
     - Disable `"memory"` in `tools.disabled_toolsets`.
     - Verify neither the external memory prompt block nor the `memory_recall` tool is present.
  3. **Resumed Session State Reuse**:
     - Re-instantiate the process with altered plugin callback content.
     - Run turn 2.
     - Verify turn 2 reuses the exact byte-for-byte system prompt from SQLite and does not re-invoke the plugin callback.

---

## 7. Verification Invariants Checklist

| Invariant | Requirement | Verification Method |
| :--- | :--- | :--- |
| **Section ID Regex** | Matches `^[a-z0-9][a-z0-9._-]{0,127}$` | Validated in `plugin_prompt::Registry::register` |
| **Section Position** | Must be `"after_memory"` | Validated in `plugin_prompt::Registry::register` |
| **Section Length** | 1 to 4,000 characters | Validated in `plugin_prompt::Registry::register` |
| **Duplicate ID Guard** | Fail-closed rejection | Validated in `plugin_prompt::Registry::register` |
| **Framing Integrity** | Exact start and end comments with character counts | Tested via `plugin_prompt::restore` roundtrip |
| **Terminal Anchor** | Must end before `\n\nConversation started:` | Verified in `plugin_prompt::restore` |
| **Single External Provider** | At most one external memory provider active | Enforced by `ExternalMemoryManager::register_provider` |
| **Toolset Policy** | Disabling memory toolset withholds tools and prompt block | Gated by `toolset_resolution::ToolsetResolver` |
| **ACID Boundary** | No network calls inside SQLite transactions | SessionDb queries execute as short standalone statements |
| **Prompt-Cache Safety** | Static prompt blocks, no network in assembly | Assembly executes synchronously without awaiting sockets |
| **Async Turn Sync** | Slow or failing memory sync never stalls turns | Dispatched via background tasks or decoupled channels |
