# Native terminal execution: the Rust production seam

Read-only audit of the Rust tree (`rust/crates/hermes-gateway`) for the smallest
deep seam that can own native terminal execution. Scope is the Rust side only.
The provider-visible schema, foreground lifecycle semantics, approval/sudo
boundaries, and exact success/error JSON fields are owned by the Python contract
lane (`native-terminal-python-contract-agy.md`); this document treats that
contract as the authority those fields must match and does not restate them.

All line numbers verified against the tree on 2026-09-09 (branch `rust-rewrite`).
`native_agent.rs` is being edited by a parallel lane, so its line refs may drift.

---

## 1. Reusable Rust modules and exact call sites

### The tool abstraction the terminal must implement

- `native_tools::Tool` trait, `native_tools.rs:39-43`: `fn spec(&self) -> ToolSpec`
  plus `async fn call(&self, args: &Value) -> Result<Value>`. This is the whole
  surface a native tool exposes. It carries no per-call context: any cwd/session
  state must be captured when the tool object is constructed.
- `native_tools::ToolSpec`, `native_tools.rs:28-35`: `name`, `description`,
  `parameters` (JSON Schema), `extra` (provider function-schema fields). This is
  the immutable schema half.
- The working model for a *stateful* native tool already exists:
  `extension_host::Tool` (`extension_host.rs:690-753`) captures a `Client` at
  construction (`from_definition`, `690-719`), returns a fixed `spec()`
  (`724-743`), and routes `call()` to the captured client (`745-752`). A terminal
  tool follows the same shape, capturing a session-scoped execution runtime
  instead of a Python `Client`. Note the name validation it applies
  (`702-709`: non-empty, `<= 64` bytes, `[A-Za-z0-9_-]`) and the parameters
  coercion to an object (`710-717`); the terminal schema must satisfy both.

### The loop that will drive the tool

- `run_tool_loop_with_messages`, `native_tools.rs:546`. Dispatch and the
  execution guard live at `743-763`:
  - name lookup `tools.iter().find(|t| t.spec().name == call.name)` (`743`),
  - non-object arguments short-circuit to `INVALID_TOOL_ARGUMENTS`
    (`native_tools.rs:756-758`, constant at `514`) before the tool ever runs,
  - `tool.call(args).await` maps `Ok(v) -> (v, true)` and
    `Err(e) -> (json!("tool error: {e}"), false)` (`759-762`).
  - Result row assembled by `tool_result::build` (`776-782`).
- Consequence for the contract: the `Err` arm produces the bare string
  `"tool error: {e}"`, which is **not** the Python terminal's structured
  success/error JSON. A native terminal must return its full contract-shaped
  result (both the ran-and-succeeded and the ran-and-failed cases) as
  `Ok(Value)`, and reserve `Err` strictly for internal faults it never expects
  the model to parse. Requirement 6 pins a test on this.
- Concurrency ceiling for child work: `ChatModel::max_concurrent_children`
  (`native_tools.rs:138`) plus `cap_delegate_calls` (`460-474`). Only relevant
  if the terminal later spawns delegated children; foreground exec does not use
  it.

### Tool registration and prefix freezing

- `registered_native_tools()`, `main.rs:223-225`: today `vec![CurrentTimeTool]`.
  Takes no arguments, so it can only build context-free tools.
- `available_native_tools(&Config)`, `main.rs:227-233`: gated on
  `config.agent_tools` (`config.rs:48,104,127`, env `HERMES_AGENT_TOOLS`).
- `merge_native_tools(base, extensions)`, `main.rs:305-318`: replaces a base slot
  in place when an extension re-declares the same name, else appends. Base is
  `registered_native_tools()`; extensions come from the Python host. **This
  means a Python-host tool named `terminal` would overwrite a native `terminal`**
  (`main.rs:863,866`). See section 5.
- `restore_tool_prefix(saved, fresh, registered)`, `native_tools.rs:53-109`,
  called at `main.rs:937`. Folds the freshly resolved surface onto the persisted
  session prefix. Still-registered-but-unavailable saved names fall back to their
  `registered` instance (`native_tools.rs:98`), so the *registered* terminal
  instance must be a working, session-bound tool, not a stub.
- `native_tool_names` handshake: `main.rs:828` sends
  `tool_names(&registered_native_tools())` to the Python extension host at spawn
  so the host can suppress tools the native side owns. This is the existing lever
  for section 5.

### Conversation initialization (where runtime state is known)

- `build_conversation_client`, `main.rs:752-990`. By the time tools are built it
  has resolved: `session_id` (`783`), `platform` (`782`), `provider`/`model`
  (`766-781`), and crucially `runtime_cwd` (`805`, via
  `initializer.runtime_cwd(home, session_cwd)`), plus `home` and
  `config.agent_cwd` (the launch/repo root). Tools are assembled at `862-866`,
  frozen at `933-954`, and packed into `NativeConversationState` at `980-988`
  (`tools` field, `main.rs:215`), which the native client receives via
  `with_tools` (`native_agent.rs:780`, applied `main.rs:722`).

### cwd and environment helpers (ready, currently dead_code)

- `runtime_cwd::CwdInputs`, `runtime_cwd.rs:8-63`: `agent_cwd()` (session ->
  terminal -> launch fallback, `32-39`), `context_cwd()` (`43-53`), `coding_cwd()`
  (explicit bypasses validation, `57-62`). Validates against a captured launch
  cwd, never a mutable global cwd. `#![allow(dead_code)]` at `runtime_cwd.rs:4`.
- `cwd_placeholder::resolve_placeholder_terminal_cwd`, `cwd_placeholder.rs:29-64`:
  resolves `.`/`auto`/`cwd` placeholders per backend (local -> messaging cwd or
  home; docker+mount -> host path; other -> `None`). Dead_code at `:4`.
- `coding_context::RuntimeMode`, `coding_context.rs:220-290`: `from_scope`,
  `cwd()`, `profile()`, `toolset_selection()`. The profile/cwd decision a coding
  surface makes; the terminal's session cwd should agree with `mode.cwd()`.
- `file_read_safety` (`expand_user`, `check_read`, `file_read_safety.rs:199`):
  path expansion and read-block guards. Reusable for validating a `cwd` argument,
  not for command approval.
- `environment_prompt.rs`: `REMOTE_TERMINAL_BACKENDS` (`62-70`),
  `render_environment_prompt` (`421-529`). The prompt tells the model that
  `terminal` operates *locally* (host block, `444-483`) or *inside the remote
  backend* (`484-515`). A native-local terminal must be paired with the local
  rendering; this is a correctness coupling, not just cosmetic (section 5).

### Subprocess primitives already in the tree (the execution pattern)

The canonical foreground pattern is `git_probe::run` (`git_probe.rs:56-127`):
`process_group(0)` (unix) / `creation_flags(0x08000000)` (windows),
`kill_on_drop(true)`, piped stdout/stderr read concurrently with `child.wait()`
under `tokio::time::timeout`, and on timeout `libc::kill(-(pid), SIGKILL)` to the
whole group (`taskkill /T /F` on windows), then CRLF normalization. The same
shape recurs in:

- `extension_host.rs:171,212-235` (persistent child, group kill on drop),
- `hooks.rs:259-410` (per-handler exec: `process_group(0)` `:367`, 30s timeout,
  detached stdin writer, group SIGKILL `:392`),
- `environment_probe.rs:84-158` (synchronous variant),
- `webhook_filters.rs:803-884` (sync `try_wait` deadline loop, kill+reap).

None of these is a public, reusable command runner. They are copies of one idea.

---

## 2. Missing primitives

For safe **foreground local** execution:

1. **A public foreground-exec primitive.** Extract the `git_probe::run` pattern
   into a shared helper (spawn under a fresh process group, concurrent bounded
   stdout/stderr capture, wall-clock timeout, group SIGKILL + reap on timeout,
   CRLF normalization). Today it is private and git-specific
   (`git_probe.rs:56`). Four modules reimplement it; the terminal must not be a
   fifth copy.
2. **An output-bounds/spill primitive.** The probes read to EOF
   (`git_probe.rs:78-83`). A terminal needs a byte/line cap with a documented
   truncation marker, and (per the Python contract) possibly a spill-to-file
   path. No such primitive exists.
3. **A session-scoped cwd cell.** `cd` persistence across calls means an owned,
   mutable cwd behind the tool, seeded from `runtime_cwd` (`main.rs:805`) and
   updated per command. `CwdInputs` computes an initial value but has no mutable
   session state (`runtime_cwd.rs`).
4. **A dangerous-command / sudo approval gate.** There is no Rust equivalent of
   the Python approval boundary. `threat_patterns.rs` scans content for injection
   (`threat_patterns.rs:1-13`), which is unrelated. This gate is a hard
   prerequisite (section 7), and its exact rules come from the Python lane.
5. **A profile/session env assembly for the child.** `hooks.rs:305` already
   builds an isolated child env from the profile scope (`env_clear` +
   `is_global_env` re-inherit + profile layer + forced `HERMES_HOME`). The
   terminal needs the same discipline so it never leaks another profile's
   secrets. Reuse, do not reinvent.

For eventual **remote/background** adapters:

6. **A backend abstraction.** A trait with a `LocalBackend` as the only first
   implementation, so remote (`docker`/`ssh`/... from
   `environment_prompt.rs:62-70`) and background execution can slot in without
   touching the tool's public schema.
7. **A process registry.** Background/long-lived processes need an owned,
   session-scoped registry with cleanup on session end. Nothing like it exists
   (`ls` shows no process/registry/background module for execution;
   `session_registry.rs` and `provider_registry.rs` are unrelated). This is
   explicitly out of the first checkpoint (section 5) but the interface must not
   preclude it.

---

## 3. Recommended narrow public interface

One provider-visible tool, one constant schema, everything else hidden.

```
// native tool, session-scoped construction
pub struct TerminalTool {
    schema: &'static TerminalSchema,   // immutable, shared across conversations
    runtime: Arc<TerminalRuntime>,     // session-scoped
}

struct TerminalRuntime {
    cwd: Mutex<PathBuf>,               // cd persistence, seeded from runtime_cwd
    backend: Box<dyn ExecBackend>,     // LocalBackend for the first checkpoint
    bounds: OutputBounds,              // byte/line cap + truncation policy
    approval: ApprovalPolicy,          // dangerous-command / sudo gate
    child_env: Arc<BTreeMap<..>>,      // profile-isolated, built once per session
    // later: processes: ProcessRegistry
}

#[async_trait]
trait ExecBackend: Send + Sync {
    async fn run_foreground(&self, cmd, cwd, timeout, bounds) -> ExecOutcome;
}
```

`impl native_tools::Tool for TerminalTool`:
- `spec()` returns only from `self.schema`, so it is byte-identical regardless of
  session cwd, backend, or host OS. Environment specifics stay in the
  `environment_prompt` tier, never in the schema.
- `call()` validates args, consults `approval`, resolves the effective cwd from
  the `Mutex<PathBuf>`, runs through `backend.run_foreground`, applies `bounds`,
  updates cwd on `cd`, and returns the contract-shaped `Ok(Value)` for both
  success and command failure.

What this hides: cwd/session state (the `Mutex` cell), process cleanup (inside
`ExecBackend` + the future registry), output bounds (`OutputBounds`), and backend
choice (`dyn ExecBackend`). The model and the loop see only `spec()`/`call()`.
Do not expose the backend, the runtime, or any speculative hook publicly; the
loop needs nothing more than the `Tool` trait.

---

## 4. Where to instantiate: immutable schema, session-scoped state

The schema and the runtime state have different lifetimes, and the current
registration functions conflate them.

- **Immutable schema**: a single `&'static TerminalSchema` (or a
  `LazyLock`). Its name and parameters feed the system-prompt tool list
  (`main.rs:881` -> `build_fresh`), the prompt-cache prefix, and the
  `native_tool_names` handshake (`main.rs:828`). It must not vary per session or
  the frozen tool-prefix and cache stability break (section 7).
- **Session-scoped runtime**: constructed inside `build_conversation_client`,
  where `runtime_cwd` (`main.rs:805`), `session_id` (`783`), `platform`,
  `provider`, and `home`/`config.agent_cwd` are all in hand. This is the single
  correct construction site.

Required change to the registration seam: `registered_native_tools()`
(`main.rs:223`) and `available_native_tools()` (`main.rs:227`) currently take
`()` and `&Config`, which cannot supply a session cwd. They must accept the
session runtime (or a small `TerminalRuntime` handle). Two of their callers need
only names, not runtime:

- `main.rs:828` (`native_tool_names`) needs names only,
- `main.rs:863` builds `registered_tools` used by `restore_tool_prefix` as
  *fallback implementations* (`native_tools.rs:98`).

Because the fallback path can hand back the registered instance for a saved
`terminal` name, the registered instance must be a real session-bound terminal,
not a placeholder. Simplest resolution: add a schema-only name accessor for the
`:828` handshake, and build the real session-bound instance once in
`build_conversation_client` for both the `registered` and `available` sets.

---

## 5. Native-local vs compatibility-host routing without duplicate names

There is exactly one provider-visible name, `terminal`. It must be owned by
exactly one side per conversation.

- The existing `native_tool_names` handshake (`main.rs:828`) is the lever: names
  in that list are suppressed by the Python extension host. So when the native
  side owns terminal (local backend), it adds `terminal` to `native_tool_names`
  and the host stops advertising its own terminal plugin. Duplicate name
  avoided.
- When the backend is remote or the request needs background execution
  (backends in `environment_prompt.rs:62-70`, or a background flag the first
  checkpoint does not implement), the native side does **not** register
  `terminal`, leaves it out of `native_tool_names`, and the Python compatibility
  host advertises and executes it against the remote/background backend. The
  frozen tool surface is unchanged; only the implementation behind the name
  differs.
- Guard against `merge_native_tools` (`main.rs:305-318`) silently overriding:
  today an extension-declared `terminal` would replace the base slot
  (`main.rs:863`), i.e. the Python host would win even when native intends to
  own it. The `native_tool_names` suppression must happen *before* the merge so
  the extension set never contains `terminal` when native owns it. If for any
  reason both sides present it, treat that as a registration bug and fail closed
  (do not register native), because a name collision here corrupts the frozen
  prefix.
- Prompt coupling: register the native-local terminal only alongside the local
  environment rendering (`environment_prompt.rs:444-483`). Registering a
  local-executing terminal while the prompt claims a remote backend
  (`:484-515`) would mislead the model about where commands run.

---

## 6. Tests for the first production checkpoint

Unit (in a new `terminal` module, following the golden-vector style already used
across the crate):

1. **Schema is constant.** `spec()` name/description/parameters are byte-identical
   for two runtimes with different cwd, backend, and host OS. Directly protects
   the frozen-prefix and prompt-cache invariants.
2. **Contract-shaped results, both outcomes.** A command that exits 0 and one
   that exits non-zero both return `Ok(Value)` whose fields match the Python
   contract (fields owned by the Python lane). Assert the loop's `Err` string
   `"tool error: ..."` (`native_tools.rs:761`) is never produced for ordinary
   command failure.
3. **cwd persistence.** A `cd`-style call mutates the session cwd; a following
   command runs in the new cwd; a sibling `TerminalTool` for a different session
   is unaffected. Seed from `runtime_cwd` (`main.rs:805`) semantics.
4. **cwd validation / placeholder resolution.** Feed `cwd_placeholder`
   (`cwd_placeholder.rs:29`) and `CwdInputs` (`runtime_cwd.rs`) cases and confirm
   the runtime seeds the expected directory; an invalid explicit cwd is rejected,
   not silently ignored.
5. **Timeout kills the whole group.** A command that spawns a child which
   outlives it: assert the child's post-timeout side effect never lands (mirror
   `hooks.rs` timeout test), proving group SIGKILL + reap.
6. **Output bounds.** Output over the cap is truncated with the documented marker
   and the byte length is bounded; assert no unbounded read.
7. **Approval gate.** A command the Python contract classifies as dangerous / a
   sudo invocation is refused (or gated) and never reaches `ExecBackend`.
8. **Profile env isolation.** The child env is built from the session profile
   scope and does not carry a foreign profile's secret (reuse the `hooks.rs`
   `apply_isolated_env` assertions).

Integration (production path, gated behind the native opt-ins so it does not
disturb the coexisting Python gateway):

9. **Registration and freeze.** With native-local enabled, `terminal` appears
   once in the resolved surface, is present in `native_tool_names` sent to the
   host (`main.rs:828`), and survives `restore_tool_prefix` (`main.rs:937`)
   across a rebuild with byte-stable ordering. Assert the Python host does not
   also advertise `terminal` (no duplicate).
10. **End-to-end turn.** A stub `ChatModel` requesting a single `terminal` call
    on a real temp cwd runs through `run_tool_loop_with_messages`
    (`native_agent.rs:1769`) and emits `ToolCallChunk` / `ToolCallFinished` /
    `MessageStop`, with the tool result row assembled by `tool_result::build`.

---

## 7. Risks that must block native registration until resolved

1. **No approval/sudo gate.** Registering a local-executing terminal without the
   Python contract's dangerous-command and sudo boundaries would let the model
   run destructive commands the Python path refuses. Hard blocker. (Rust has no
   equivalent today; `threat_patterns.rs` is content scanning, not command
   approval.)
2. **Schema drift breaking the frozen prefix / prompt cache.** If `spec()` varies
   with host OS, cwd, or backend, the tool-prefix freeze (`restore_tool_prefix`,
   `native_tools.rs:53`), the persisted `saved_tool_names`, and the prompt-cache
   prefix all destabilize. The schema must be a constant (section 4). Blocker
   until proven constant by test 1.
3. **Duplicate provider-visible name.** If both native and the Python host present
   `terminal`, `merge_native_tools` (`main.rs:305`) silently picks one and the
   frozen prefix is corrupted. The `native_tool_names` suppression must run
   before the merge and collisions must fail closed (section 5). Blocker until
   the handshake ordering is enforced and tested (test 9).
4. **Missing process-group cleanup on the shared primitive.** If the terminal
   spawns without a fresh process group + timeout + group kill (the
   `git_probe.rs:56` pattern), a runaway child survives the turn and leaks into
   the long-lived gateway. Blocker until the shared foreground primitive exists
   and test 5 passes.
5. **Error-shape leak.** Returning `Err` for ordinary command failure surfaces
   the loop's `"tool error: {e}"` string (`native_tools.rs:761`) instead of the
   contract JSON, breaking the model's ability to read exit status / stderr.
   Blocker until test 2 passes.
6. **Registered fallback is a real terminal.** Because `restore_tool_prefix` can
   hand back the *registered* instance for a saved `terminal` name
   (`native_tools.rs:98`), a stub registered instance would execute (or fail
   wrongly) on a restored session. The registered and available instances must be
   the same session-bound implementation (section 4). Blocker.

Non-blocking but track: background/remote execution and the process registry are
deliberately deferred to the Python compatibility host for the first checkpoint
(section 5); the interface reserves room for them (section 3) but they need not
ship to register a native-local terminal.
