# Extension-host checkpoint: compatibility and correctness review

Scope: the uncommitted working-tree changes that add the Rust-native legacy
extension host. Analysis only. No implementation files were edited, no Cargo
run, no commit, no push.

Files traced for the real runtime path:

- `hermes_cli/rust_extension_host.py` (the subprocess child)
- `rust/crates/hermes-gateway/src/extension_host.rs` (the Rust client)
- `rust/crates/hermes-gateway/src/main.rs` (`extensions_configured`,
  `merge_native_tools`, `build_conversation_client`, `build_agent_client_for_home`)
- `rust/crates/hermes-gateway/src/conversation_prompt.rs` (`build_fresh`,
  `restore_or_build`)
- `rust/crates/hermes-gateway/src/plugin_prompt.rs` (`validate_rendered`, framing)
- `rust/crates/hermes-gateway/src/native_tools.rs` (tool trait, tool loop,
  `restore_tool_prefix`, `tool_spec_json`)
- `rust/crates/hermes-gateway/src/native_agent.rs`, `message.rs`,
  `system_prompt.rs` (`assemble`), `tool_result.rs`, `conversation_agent.rs`
- Python contracts: `hermes_cli/plugins.py`, `plugins/memory/__init__.py`,
  `agent/memory_manager.py`, `agent/memory_provider.py`, `agent/secret_scope.py`,
  `hermes_cli/env_loader.py`, `agent/agent_init.py`, `agent/system_prompt.py`,
  `agent/tool_dispatch_helpers.py`, `tools/registry.py`, `run_agent.py`

Verdict up front: this is a strong checkpoint. The prompt assembly, tool-freeze,
profile-secret isolation, and default byte-neutral path all hold and match
Python. There is one high-severity correctness gap (multimodal results) plus a
handful of narrower divergences worth fixing or consciously deferring before
this becomes the live path for a home with extensions.

---

## Confirmed bugs, ranked by severity

### 1. HIGH: multimodal tool results are not normalized before the wire

`native_tools.rs:669-691`. A tool now returns `serde_json::Value`. The loop
takes whatever `tool.call().await` returns and hands it straight to
`tool_result::build` as `content`:

```rust
Some(tool) => match tool.call(&call.arguments).await {
    Ok(out) => (out, true),
    ...
messages.push(crate::tool_result::build(&call.name, &content, ...));
```

Python plugin tools legitimately return a multimodal envelope. `registry.dispatch`
documents that handler results are "normalized to a string or supported
multimodal envelope" (`tools/registry.py:1176-1177`), and the checkpoint's own
fixture returns `{"_multimodal": true, "content": [...]}`
(`extension_host.rs:542`, asserted verbatim at `extension_host.rs:617-619`). So
the envelope really does arrive at the Rust client as the tool result.

Python never puts that envelope on the wire. `run_agent.py:7943-7998`
(`_tool_result_content_for_active_model`) unwraps it before building the message:

- not a multimodal envelope: pass through unchanged
- envelope with no image parts: use the `content` list
- envelope with image parts + vision-capable model/provider: use the `content` list
- otherwise: fall back to `_multimodal_text_summary(result)` (a plain string)

The Rust loop does none of this. For any plugin tool that returns a multimodal
envelope it sends the raw `{"_multimodal": true, "content": [...]}` dict as the
tool message `content`, which is not a valid OpenAI tool-content shape (must be a
string or a parts array), and there is no text-only fallback for non-vision
providers. `tool_result::build` only handles string and array content
(`tool_result.rs:265-279`); a dict passes through untouched.

Second-order effect: a large image envelope also trips the 4 MB per-line
response cap in the client (`extension_host.rs:30`, `284-298`), which returns
`Err` and marks the worker unhealthy (see finding 4).

Minimal fix: port `_tool_result_content_for_active_model` into the tool loop (or
into the extension `Tool::call` boundary), unwrap `_is_multimodal_tool_result`
to the content list when the active model/provider supports list-type tool
content, and to `_multimodal_text_summary` otherwise. Text-only string results
are already correct and need no change.

Note: memory-provider tools are contractually string-returning
(`agent/memory_provider.py:241-247`, "Must return a JSON string"), so this gap is
scoped to plugin tools, but plugin tools returning envelopes are a supported,
tested shape, so this is a real divergence, not a theoretical one.

---

### 2. MEDIUM: the spawn gate misses auto-loaded bundled backend plugins

`main.rs:219-230`. `extensions_configured` spawns the host only when
`memory.provider` is set or `plugins.enabled` contains a non-empty name.

Python's `discover_and_load` loads more than the `plugins.enabled` allow-list.
Bundled plugins with `kind: backend` load automatically, independent of
`plugins.enabled` (`hermes_cli/plugins.py:4448-4454`). The repo ships one:
`plugins/spotify/plugin.yaml` (`kind: backend`), whose `register` unconditionally
registers a `spotify` toolset (`plugins/spotify/__init__.py:56-66`, gated later
by `check_fn`, not by `plugins.enabled`).

So a home with `tools.enabled_toolsets: [spotify]` but an empty/absent
`plugins.enabled` and no `memory.provider` will, under Python, expose the spotify
tools; under this checkpoint the host is never spawned and those tools silently
disappear. Standalone bundled plugins (`disk-cleanup`, `security-guidance`)
default to `kind: standalone` (`plugins.py:1121`, `4794-4812`) and do require
`plugins.enabled`, so they are correctly covered; only the auto-loading
`backend`/`platform` kinds are missed.

Minimal fix: treat a configured `tools.enabled_toolsets` that resolves to an
auto-loading bundled backend toolset as "extensions configured" too, or simply
spawn the host whenever a bundled backend plugin exists on disk. The cheapest
honest option is to also spawn when `tools.enabled_toolsets` is non-empty, then
let the host's own resolver decide whether anything is actually exposed (it
already returns empty lists when nothing qualifies).

---

### 3. MEDIUM: worker health conflates application errors with transport death

`extension_host.rs:234-244` (`run_worker`) plus `259-311` (`exchange`).

`exchange` returns `Err` for three unrelated situations: a genuine transport
failure (short read, timeout, invalid JSON, id mismatch), an oversized response
(`>4 MB`, line 294-298), and a protocol-level application error where the child
is alive and answered with `{"ok": false}` (line 304-310). `run_worker` then
does `healthy &= result.is_ok();`, so any one of those permanently disables the
host for the rest of the conversation. Every later call returns "extension host
is not running" with no recovery short of rebuilding the conversation client.

The common tool-failure path is safe, which is why this is medium rather than
high: `registry.dispatch` swallows handler exceptions and returns a `tool_error`
string (`tools/registry.py:1184-1205`), and `MemoryManager.handle_tool_call`
does the same (`agent/memory_manager.py:948-966`), so a failing tool answers
`ok: true` with an error payload. The `ok: false` path is reached only by an
unknown-tool call, `call_tool` param validation
(`rust_extension_host.py:256-270`), a `_normalize_handler_result` failure, or the
oversized-response cap, which finding 1's image envelopes can hit.

Minimal fix: in `exchange`, distinguish a well-formed `{"ok": false}` response
(the child is healthy, return the error to the caller without poisoning the
worker) from a transport-level failure (poison the worker and kill the child).
Only the latter should clear `healthy`.

---

### 4. MEDIUM: `gateway_session_key` is set to the internal session id

`main.rs:536`. The host forwards `gateway_session_key` into
`MemoryManager.initialize_all`, which threads it to providers. In production
Python this is a distinct value (`agent._gateway_session_key`,
`agent/agent_init.py:2004-2006`) that Honcho and similar providers use for
"stable per-chat session isolation". The checkpoint passes
`Some(session_id.clone())` instead.

If the Rust `session_id` is stable across session resets this is harmless. If a
reset mints a new `session_id` (the usual meaning of a reset), then external
providers keyed on `gateway_session_key` will re-bucket memory on every reset,
whereas Python keeps the same key. Worth confirming against
`session_db::message_session_id` stability, and threading the real per-chat key
if they differ.

---

### 5. LOW: `user_id_alt`, `user_name`, `chat_name` are never threaded

`main.rs:530-533` hardcode these to `None`. Python's `initialize_all` threads
all three when present (`agent/agent_init.py:1992-1999`). The Rust
`hermes_core::Message` (`rust/crates/hermes-core/src/lib.rs:49-87`) carries none
of them, so this is a structural data gap rather than a wiring oversight: the
values do not exist to forward. Providers that scope memory by alternate user id
or by display names lose that granularity. Fix requires carrying the fields on
`Message` first, so it is reasonable to defer, but it should be recorded as a
known behavioral difference rather than parity.

Related and acceptable: the CLI-only `warning_callback` / `status_callback`
kwargs (`agent/agent_init.py:1977-1979`) are callables and cannot cross the JSON
protocol, so the host omits them. Providers no-op when they are absent, so this
is fine to leave as-is; note it so it is not mistaken for a bug later.

---

## Items checked and found correct

- Registered vs available tools. The host returns `registered_plugin_tools`
  (all plugin names from the registry, `rust_extension_host.py:198-207`) and
  `plugin_tools` (only names the resolver currently exposes, `179-191`), with
  `memory_tools` folded into both when exposed. Rust maps these to
  `registered_tools` and `available_tools` (`extension_host.rs:82-93`) and merges
  each with the native catalog (`main.rs:559-572`). This matches the native
  freeze model: the registered superset feeds `restore_tool_prefix`, the
  available surface drives the fresh tool list. Dedup keeps the first occurrence
  (`extension_host.rs:74-79`), consistent with Python's first-registration-wins.

- Memory-provider gating. The host computes `memory_exposed` with
  `memory_provider_tools_enabled(enabled, disabled, memory_tool_present=False)`
  (`rust_extension_host.py:210-217`), and it withholds both the memory tools and
  the memory prompt block when gated off (`218-224`, `242-245`). This mirrors
  Python exactly: the external memory block is gated on the same
  `memory_provider_tools_exposed` check that gates the tools
  (`agent/system_prompt.py:939-954`, `agent/memory_manager.py:117-141`). The
  hard veto (`disabled` contains `memory`) and the `enabled_toolsets is None ->
  True`, empty-list `-> False` semantics all line up with `_string_list`
  returning `None` vs `[]` (`rust_extension_host.py:22-27`).

- External prompt ordering and budgets. `build_fresh` assigns
  `external_memory` after the built-in memory snapshot and appends the plugin
  block (`conversation_prompt.rs:358-367`). `assemble` orders the volatile tail
  skills, memory, user_profile, external_memory, plugin_sections, footer
  (`system_prompt.rs:684-694`), which matches Python's order in
  `agent/system_prompt.py:925-961`. `validate_rendered`
  (`plugin_prompt.rs:297-312`) re-applies the 4000-char per-section and
  8000-char aggregate / 32-section budgets that the Python host already enforced
  (`plugins.py:558-563`, `5917-6005`), so the boundary output is re-validated as
  untrusted subprocess data without changing well-formed sections.

- Frozen prompt and tool-prefix restoration. On reuse the build closure is not
  invoked, so `client.snapshot()` never runs and plugin prompt callbacks do not
  re-execute; the live test asserts exactly one callback across a fresh turn plus
  a resumed reuse (`main.rs:1369-1373`). `restore_tool_prefix`
  (`native_tools.rs:53-109`) resolves saved names against the non-consuming fresh
  map first, keeps still-registered unavailable tools across probe flaps, and
  appends new tools, matching the Python merge intent.

- Profile secret isolation. `_install_profile_secrets`
  (`rust_extension_host.py:30-65`) clears every non-global managed/credential key
  before any import can read it, then installs only the task-local snapshot,
  using the same `_is_global_env` classification
  (`agent/secret_scope.py:142-146`) that `build_profile_secret_scope` uses to
  exclude globals (`agent/secret_scope.py:289-310`). Rust only sends
  `profile_secrets` under an active scope and asserts a scope exists under
  multiplex (`main.rs:476-480`). The live test proves a foreign env secret does
  not cross and the right profile secret is present
  (`extension_host.rs:497-537`, `620-625`).

- dotenv and external-secret precedence. The child sets `HERMES_HOME` and the
  home override, installs the snapshot, then calls
  `load_hermes_dotenv(hermes_home=...)` and restores `HERMES_HOME`
  (`rust_extension_host.py:86-98`). Because the child never calls
  `set_multiplex_active`, `is_multiplex_active()` is `False` inside it
  (`agent/secret_scope.py:37-52`), so `load_hermes_dotenv` does not take its
  multiplex short-circuit and instead loads the profile `.env` with
  `override=True` and applies external sources (`hermes_cli/env_loader.py:535-575`).
  Since `HERMES_HOME` points at the correct profile, this reloads the same
  profile's secrets and cannot pull a foreign profile's values, so the earlier
  snapshot injection plus this reload converge on the right identity. No leak.

- Host lifetime within a conversation. The client is cloned into each `Tool`
  and into the conversation state; when the last clone drops, `run_worker` sends
  a `shutdown` and waits, then kills the process group on timeout
  (`extension_host.rs:246-257`, `121-132`). The child is process-group leader and
  gets `SIGKILL` to the whole tree on drop or timeout, so cancelled turns cannot
  orphan plugin-spawned descendants. `shutdown` drains the sync executor and
  calls `shutdown_all` then `unload` (`rust_extension_host.py:272-283`). The 7 s
  shutdown budget (`extension_host.rs:29`) is tight against the 5 s sync-executor
  drain (`agent/memory_manager.py:80`) but adequate.

- Default no-extension byte-neutral path. When `extensions_configured` is
  false, `extension` is `None`, both merges are no-ops on the native catalog
  (`main.rs:559-572`, `merge_native_tools` returns `base` unchanged), the build
  closure gets `PromptSnapshot::default()`, so `external_memory` is `None` and
  `plugin_sections` is empty and `validate_rendered([])` yields nothing
  (`conversation_prompt.rs:358-367`). `build_fresh` therefore produces the same
  bytes as before. The existing verbatim-reuse test still passes with
  `tool_names == ["current_time"]` (`main.rs:1077`, `1203-1205`).

  One behavior change to record even though it is inert: `plugin_prompt.restore`
  was moved out of the `restore_frozen_sections` branch and is now called on
  every build (`main.rs:614-615`). On a fresh build it now parses the
  just-built prompt into the snapshot. The snapshot is dead ownership today
  (`native_agent.rs:207-209`), so this changes nothing observable, but it is a
  deliberate change worth noting.

---

## Deferred lifecycle gaps (correctly outside this checkpoint)

These are not regressions in this diff, but they become load-bearing once this
is the live path, so they belong on the record.

- Unbounded conversation-client cache. `ConversationAgent` caches one client
  per `(home, session_id)` in a `OnceCell` and never evicts
  (`conversation_agent.rs:25-31`, `83-105`). Each cached conversation now pins a
  live Python subprocess plus its provider connections and sqlite handles for the
  lifetime of the gateway. A session reset creates a new cell but leaves the old
  child alive until process exit. This amplifies a pre-existing no-eviction gap;
  it needs a TTL or reset-driven teardown before production.

- No mid-conversation respawn. Once the worker is marked unhealthy (finding 3,
  or a genuine crash) there is no reconnect; the conversation loses all extension
  tools until the client is rebuilt.

- Dead ownership held for a future rebuild. `_plugin_prompt` and
  `_extension_host` are retained on `NativeAgentClient` for a native compression
  rebuild that does not exist yet (`native_agent.rs:205-211`, `247`). Expected
  and documented.

---

## Merge readiness

Close, but not yet for a home that actually uses extensions.

- For the default (no extensions) path: ready. The byte-neutral guarantee holds
  and is tested.
- For homes with extensions: fix finding 1 (multimodal normalization) before
  merge; it produces malformed tool messages for a supported plugin return shape
  and can brick the host via the size cap. Findings 2, 3, and 4 should either be
  fixed or explicitly accepted as known limitations with a follow-up, since each
  is a real behavioral difference from the Python monolith a user could hit with
  ordinary configuration.

## Missing tests

- A plugin tool that returns a multimodal envelope driven through the full tool
  loop, asserting the tool message `content` is the unwrapped parts list (vision
  path) or a text summary (non-vision path). Today only the raw envelope out of
  `Tool::call` is asserted (`extension_host.rs:617-619`); it is never fed to
  `tool_result::build`.
- A bundled `kind: backend` plugin (spotify-style) with `tools.enabled_toolsets`
  set but `plugins.enabled` empty and no `memory.provider`, asserting the host is
  spawned and the tools reach the model (covers finding 2).
- A well-formed `{"ok": false}` response from the child mid-conversation,
  asserting the next tool call still succeeds (covers finding 3).
- An oversized (>4 MB) tool result, asserting graceful degradation rather than a
  dead host.
- `gateway_session_key` threading across a session reset, asserting the value
  passed to `initialize` matches the intended per-chat key (covers finding 4).
- A single assembled prompt that carries the external memory block and plugin
  sections together, pinned against a Python golden, to lock the volatile-tail
  ordering with both present at once.
- A multi-turn conversation asserting the host is spawned and `initialize`d
  exactly once (cache reuse), not per turn.
