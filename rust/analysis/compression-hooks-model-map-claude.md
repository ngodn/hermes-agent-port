# Compression summarizer-selection & hook-contract map (Claude)

Scope for the next Rust checkpoint after `bab208de9d`. Covers only:
auxiliary compression model/provider routing, fallback/cooldown/error
behavior, `checkpoint_required` memory behavior, context-engine/plugin hooks,
and in-place vs rotation policy. It deliberately does **not** cover automatic
token thresholds or the pruning/micro-compaction trigger algorithms (owned by
another helper). No source was edited.

`trajectory_compressor.py` at repo root is a **separate, standalone OpenRouter
trajectory tool** (`summarization_model` default `google/gemini-3-flash-preview`),
not the live conversation compressor. Ignore it. The live path is
`agent/conversation_compression.py` + `agent/context_compressor.py` +
`agent/auxiliary_client.py` + `agent/memory_manager.py` + `agent/memory_provider.py`.

---

## 1. Summarizer model / provider selection

### Python contract

The summary call is one `call_llm(task="compression", ...)` from
`ContextCompressor` (`agent/context_compressor.py:5500-5548`):

```python
call_kwargs = {
    "task": "compression",
    "main_runtime": {"model": self.model, "provider": self.provider,
                     "base_url": self.base_url, "api_key": self.api_key,
                     "api_mode": self.api_mode},
    "messages": [{"role": "user", "content": prompt}],   # NO max_tokens
}
if self.summary_model:
    call_kwargs["model"] = self.summary_model            # per-compressor override
call_kwargs["route_info"] = _aux_route                   # call_llm records final route
```

Route resolution lives in `_resolve_task_provider_model("compression")`
(`agent/auxiliary_client.py:8696-8870`). Priority order:

1. explicit `provider`/`model`/`base_url`/`api_key` args (always win),
2. config `auxiliary.compression.{provider,model,base_url,api_key,key_env,api_mode}`
   via `_get_auxiliary_task_config` (`auxiliary_client.py:8887-8928`),
3. `"auto"` → inherit `main_runtime` (the main conversation model).

Key normalization facts to preserve:
- literal `model: "auto"` is dropped to `None` so the provider-side auto path
  (main-runtime inheritance) runs; otherwise `"auto"` hits the wire as a model id
  (`auxiliary_client.py:8737-8754`).
- `provider: moa` is unwrapped to the aggregator slot for aux tasks
  (`auxiliary_client.py:8759-8793`).
- direct-API aliases (`openai`, etc.) rewrite to `custom` + base URL
  (`_expand_direct_api_alias`, `auxiliary_client.py:8801-8807`).
- **when nothing is configured, compression legitimately runs on the main model.**
  This is the honest baseline the Rust port already matches.

Config also feeds a **timeout floor**: `_COMPRESSION_TIMEOUT_FLOOR_SECONDS = 300`
(`auxiliary_client.py:8884`), a minimum applied only to config-derived
compression timeouts (a reasoning aux model can exceed the 120s default).

`get_text_auxiliary_client("compression", main_runtime=...)` builds the actual
client; `_try_configured_fallback_for_unavailable_client("compression", provider)`
(`auxiliary_client.py:6303`) supplies a substitute when the configured provider
can't build a client.

### Startup feasibility probe

`check_compression_model_feasibility(agent)`
(`conversation_compression.py:2573-2822`), called from `AIAgent.__init__`:
- resolves the aux compression client, then, if unavailable, tries
  `_try_configured_fallback_for_unavailable_client`;
- if still none → stores `agent._compression_warning` ("compression will drop
  middle turns without a summary") and returns;
- **hard floor**: aux context `< MINIMUM_CONTEXT_LENGTH` (64K) → raises
  `ValueError` and refuses to start the session;
- if aux context `< threshold_tokens` → **auto-lowers this session's
  threshold** to the aux window, keeps `tail_token_budget` /
  `threshold_percent` in lockstep, and warns with a config suggestion.
`replay_compression_warning` (`:2825`) re-emits the stored warning through the
gateway `status_callback` on the first turn (callback isn't wired at init).

### Current Rust state (`bab208de9d`)

- `NativeAgentClient::summarize_context` (`native_agent.rs:700-716`) calls
  `<Self as ChatModel>::step` on **its own `self.model`** - i.e. the main
  conversation model. There is **no aux route, no config lookup, no
  `main_runtime`/`auto` inheritance, no per-compressor `summary_model`
  override.**
- `summary_temperature(model)` (`native_agent.rs:60-69`) mirrors the Python
  aux temperature policy (omit temperature generally; Arcee `trinity-large-thinking`
  fixed at 0.5) and is applied inside `ChatModel::step` at `native_agent.rs:859-876`
  when tools are empty (the summary path). Goldens: `tools/summary-temperature-goldens.json`.
- `provider_registry::ProviderProfile.default_aux_model` exists
  (`provider_registry.rs:71`) but is empty and unused for compression.
- No feasibility probe / threshold auto-lower exists.

**Seam:** `AgentClient::summarize_context` (`agent.rs:97-106`) is the single
extension point. Aux routing would be added *inside* the native backend (build
a second `NativeAgentClient` from `auxiliary.compression.*` when configured,
falling back to `self` when `auto`/unset), not in the gateway `compress_session`.

---

## 2. Fallback / cooldown / error behavior

### Fallback chain (`auxiliary.compression.fallback_chain`)

- `resolve_compression_fallback_route()`
  (`conversation_compression.py:1278-1339`) returns the first structurally
  complete entry (`provider` + `model` both required) as explicit `call_llm`
  route kwargs (`provider/model/base_url/api_key/api_mode/timeout`).
- The aux client applies the *full* chain from its own exception handler
  (`_try_configured_fallback_chain`, `auxiliary_client.py:6184`), but a **stall**
  (connection open, no tokens, host-timeout abort) never reaches that handler.
  `_retry_compression_on_fallback_chain` (`:1342-1452`) handles the stall path:
  re-runs the complete worker **once** with the route pinned via
  `pin_summary_route` (`context_compressor.py:100`, consumed by
  `_pinned_summary_call_kwargs` at `:129`). Accepted limitation: the retry
  repeats memory/plugin pre-compress callbacks (built-ins are idempotent).

### Cooldowns (durable, session-DB backed)

- `_SUMMARY_FAILURE_COOLDOWN_SECONDS = 600` (`context_compressor.py:1347`) -
  armed when the summary provider genuinely fails
  (`context_compressor.py:5655-5663`).
- `_SPLIT_FAILURE_COOLDOWN_SECONDS = 60` (`conversation_compression.py:100`) -
  armed when the rotation/split fails (transient lease/DB), deliberately the
  first rung of the 60/300/900 ladder.
- Persistence API on the session DB: `record_compression_failure_cooldown` /
  `clear_compression_failure_cooldown` / `get_compression_failure_cooldown_row`
  (`context_compressor.py:3106-3197`; captured/rolled back under lease in
  `conversation_compression.py:501-668`).
- `force=True` (manual `/compress`) clears/bypasses the cooldown;
  `bypass_cooldown=True` ignores only the summary-failure cooldown for a
  provider-proven overflow retry without clearing it
  (`conversation_compression.py:3381-3391`).

### Error → outcome contract

- Aux summary that fails to produce usable text → `compress_context` returns
  the **original messages unchanged** and the existing prompt; callers detect
  the no-op via `len(returned) == len(input)` and stop retrying
  (`conversation_compression.py:3398-3403`).
- Terminal provenances `AGENT_COMPRESSION_TIMEOUT` / `AGENT_COMPRESSION_COOLDOWN`
  latch so a late worker can't overwrite them (`:88-93`).

### Current Rust state

- `compress_session` (`session_commands.rs:215-235`) treats a summary error as
  "conversation not changed" + a user-facing warning, and treats
  `Ok(None)`/empty as "no native summarization surface." Correct no-op
  semantics, but there is **no fallback chain, no stall retry, no durable
  cooldown, and no `force`/`bypass_cooldown`**. There is an anti-growth guard
  (`session_commands.rs:262-268`) and a split-failed no-op
  (`published == None`, `:299-305`), but neither arms a cooldown row.

---

## 3. `checkpoint_required` memory behavior

### Python contract

Config `compression.checkpoint_required` → `agent.compression_checkpoint_required`.
Read strictly with `is True` (MagicMock guard) at
`conversation_compression.py:3463-3465`.

When set:
- codex app-server route → `_checkpoint_blocked(...)` raised (`:3466-3471`).
- the memory provider path (`:4127-4163`):
  - requires `memory_manager` with `supports_pre_compress_checkpoint`
    (`memory_manager.py:1093-1111` - true iff some provider's
    `pre_compress_checkpoint_api_version >= PRE_COMPRESS_CHECKPOINT_API_VERSION`);
  - `PRE_COMPRESS_CHECKPOINT_API_VERSION = 2` (`memory_provider.py:48`);
    base default on `MemoryProvider` is `1` (implicit legacy, `:117`);
  - calls `memory_manager.on_pre_compress(messages, evidence_messages=...,
    require_checkpoint=True, checkpoint_api_version=2)`;
  - any absence/probe-failure/exception → raise `_checkpoint_blocked(...)`
    (`conversation_compression.py:1820-1824`, `4131-4161`), which **preserves the
    uncompressed transcript** (compression refuses rather than losing evidence).

`MemoryManager.on_pre_compress` (`memory_manager.py:1113-1186`): per provider,
v2+ providers get `evidence_messages` (host-normalized) and, if their signature
accepts it, `require_checkpoint=True`; raises if `require_checkpoint` and no
v2 provider succeeded. The returned string is sanitized
(`sanitize_memory_context`) and injected into the summary prompt as
`memory_context` (see §4).

`_direct_messages_for_pre_compress_memory` (`conversation_compression.py:2448-2481`):
the normalized evidence list = only `user`/`assistant` direct messages,
drops compression summaries, drops tool rows, strips `tool_calls` from
assistant messages (keeps their prose), drops pure tool-call wrappers.

Non-`checkpoint_required` path (`:4164-4172`): best-effort `on_pre_compress`,
swallow exceptions.

### Current Rust state

- `checkpoint_required` is plumbed from `compression.checkpoint_required`
  (`dispatch.rs:410-412`, `message.rs:243-244`) into `CompressCommand`
  (`session_commands.rs:30`), but it **only hard-blocks** compression with:
  "…the native pre-compression memory checkpoint hook is not connected yet…"
  (`session_commands.rs:200-201`). Verified by the `NoSummary` test case
  expecting `"checkpoint_required"` (`session_commands.rs:1593`).
- No `on_pre_compress`, no `evidence_messages` normalization, no
  `supports_pre_compress_checkpoint`, no `memory_context` injection.
- An extension-host external-memory seam already exists on the native agent
  (`native_agent.rs:729-741` `turn_complete`, `:744-749` `close_conversation`)
  but is **not** wired to a pre-compress checkpoint.

---

## 4. Context-engine / plugin hooks

### Python contract

- **on_pre_compress return → summary prompt.** The sanitized provider string
  becomes `memory_context`, passed to `context_compressor.compress(...)` via
  `_supported_compression_kwargs` (`conversation_compression.py:4174-4198`); the
  compressor renders it as a `_memory_section` in the prompt
  (`context_compressor.py:5486`). Engines that don't accept `memory_context`
  get a one-time warning and continue without it.
- **on_session_start (boundary) after commit.**
  `_notify_context_engine_compression_complete`
  (`conversation_compression.py:3280-3320`): calls
  `context_compressor.on_session_start(new_session_id,
  boundary_reason="compression", old_session_id=..., platform=..., conversation_id=...)`.
  Observer semantics - failures are logged, never undo the committed rotation.
  It also fires relay `SESSION_COORDINATOR.notify_session_compacted(...)`
  (`:3291-3300`).
- **deferred notification** for hosts that commit an outer transaction:
  `_queue_context_engine_compression_notification` (`:3323-3340`) stages exactly
  one call; `finalize_context_engine_compression_notification(committed=...)`
  (`:3343-3353`) emits (commit) or discards (rollback), idempotent. Gated by the
  `defer_context_engine_notification` arg.
- **memory provider on_session_switch** on rotation: called with
  `parent_session_id`, `reset=False`, `rewound=False` for compression lineage
  (`memory_provider.py:273-315`; wired e.g. at `conversation_compression.py:2182-2189`).
- Thread-safety contract for all these extension points is documented in the
  module docstring (`conversation_compression.py:28-49`): under the host
  progress-aware timeout the whole pass (engines + providers) runs on a pooled
  daemon thread; publication happens only on an admitted `CompressionCommitFence`
  commit.

### Current Rust state

- `finalize_context_engine_compression_notification` is imported/wired on the
  Python side (`gateway/slash_commands.py:4594-4597`) but the Rust
  `compress_session` performs **no context-engine notification, no
  `on_session_start` boundary call, no relay `notify_session_compacted`, and no
  memory `on_session_switch`** after `publish_compression`. It only releases the
  conversation cache: `agent.release_conversation(context, &session_id)`
  (`session_commands.rs:318`; trait default `agent.rs:142-145`).
- No deferred-notification staging exists.

---

## 5. In-place vs rotation policy

### Python contract

- Config `compression.in_place` → `agent.compression_in_place`, **default
  `True`** (`conversation_compression.py:3535-3548`). A missing attribute must
  NOT fall back to rotation (that re-enables the pre-lease drift bug cluster).
- In-place: rewrite the message list + refresh the system prompt, **keep the
  same `session_id`** - no `end_session`, no child, no `name #N` renumber, no
  contextvar/env re-sync, no memory/context-engine session-switch.
- Rotation (`in_place=False`): archive parent + create a `parent_session_id`
  child, then run the session-switch + context-engine notifications.
- Outcome signals: `agent._last_compaction_in_place` (run-level, read by the
  gateway) and `agent._last_compression_attempt_in_place` (per-attempt, `None`
  = aborted/no-boundary) (`:3421-3425`, `3546-3548`).

### Current Rust state

- Rust is **rotation-only**: `publish_compression(...)` always creates a
  replacement child entry and rebinds the transcript lease to
  `published.entry.session_id` (`session_commands.rs:287-317`). There is no
  `compression.in_place` config read and no in-place branch. This matches the
  prior map's plan (rotation-mode slice first;
  `native-compression-map-claude.md:11-18`), but diverges from the Python
  **default** (`in_place=True`).

---

## 6. Smallest production-wired next checkpoint

Ranked, each independently shippable with behavior tests. All are additive to
the existing `AgentClient::summarize_context` + `compress_session` seams.

1. **Auxiliary compression model routing (highest value, matches "aux
   model/provider routing").**
   - Add an optional aux summary client to `NativeAgentClient`, resolved once
     from `auxiliary.compression.{provider,model,base_url,api_key,key_env,
     api_mode,timeout}` with the Python priority (explicit > config > `auto`),
     `auto`/unset → reuse `self` (main model). Apply the 300s timeout floor.
     Keep `summary_temperature` policy.
   - `summarize_context` routes through the aux client when present, else `self`.
   - Tests: config with explicit compression model → request hits that model +
     base_url; `model: auto` and empty config → falls back to `self.model`
     (assert against a recorded body, like
     `native_agent.rs:1014` compression request test); `moa`/direct-alias
     normalization goldens.
   - Optionally port `check_compression_model_feasibility` (64K hard floor →
     refuse start; aux `<` threshold → warn). Can be a follow-up.

2. **Durable summary-failure cooldown + no-op contract (matches
   "fallback/cooldown/error").**
   - On summary error/stall, arm a session-DB cooldown (600s summary-failure,
     60s split-failure) and skip auto-compression until it lapses; `/compress`
     (`force`) bypasses/clears it. Manual path already returns the correct
     no-op ("conversation not changed").
   - Tests: two failures within 600s → second auto attempt skipped, cooldown
     row written; `force` clears it; split-failure arms the 60s rung.
   - Fallback-chain / stall single-retry (`resolve_compression_fallback_route`
     + `pin_summary_route` semantics) can be a later add-on.

3. **`checkpoint_required` real hook (matches "checkpoint_required memory").**
   - Replace the hard-block string with a real pre-compress checkpoint over the
     existing extension-host seam: normalize `evidence_messages`
     (port `_direct_messages_for_pre_compress_memory`), require a v2-capable
     provider (`supports_pre_compress_checkpoint`), call `on_pre_compress(...,
     require_checkpoint=true)`, inject the sanitized return as `memory_context`
     in `compression_prompt::build`. On any failure → refuse compression,
     preserve transcript (the current no-op reply is the correct fallback while
     unwired).
   - Tests: v2 provider present + succeeds → compression proceeds and prompt
     carries `memory_context`; provider absent/fails → refuse, transcript
     unchanged; non-`checkpoint_required` path swallows provider errors.

4. **Context-engine / on_session_switch notification after commit (matches
   "context-engine/plugin hooks").**
   - After `publish_compression`, fire an observer notification (session-switch
     with `parent_session_id`, `reset=false`; context-engine boundary
     `boundary_reason="compression"`) that can never undo the committed
     rotation. Stage/finalize variant only if a host outer-transaction path is
     added.
   - Tests: successful compression fires exactly one notification with the new
     + old session ids; a notification failure does not roll back the rotation.

5. **In-place mode (optional; aligns Rust default with Python).**
   - Add `compression.in_place` (default `true`) with an in-place branch that
     rewrites history under the held leases without minting a child or running
     session-switch. Keep rotation for `in_place=false`. Larger change; only if
     parity with the Python default is required this checkpoint.
   - Tests: `in_place=true` → same `session_id`, no child row, no session-switch
     notification; `in_place=false` → current rotation behavior.

---

## 7. Exact source reference index

Python:
- Summary call / route: `agent/context_compressor.py:5500-5548`; task-route
  resolver `agent/auxiliary_client.py:8696-8870`; task config
  `:8887-8928`; timeout floor `:8884`; unavailable-fallback `:6303`.
- Feasibility: `agent/conversation_compression.py:2573-2822`; replay `:2825-2840`.
- Fallback chain / stall retry: `conversation_compression.py:1278-1452`;
  pin route `context_compressor.py:100-160`.
- Cooldowns: `context_compressor.py:1347`, `3106-3197`, `5655-5663`;
  `conversation_compression.py:100`, `501-668`; force/bypass `:3381-3391`.
- No-op contract / terminal provenance: `conversation_compression.py:88-93`,
  `3398-3403`.
- checkpoint_required: `conversation_compression.py:3463-3471`, `4127-4172`,
  `2448-2481`, `1820-1824`; `memory_manager.py:1093-1186`;
  `memory_provider.py:48`, `117`, `273-327`.
- Context-engine hooks: `conversation_compression.py:3280-3353`; docstring
  contract `:28-49`.
- In-place vs rotation: `conversation_compression.py:3535-3548`, `3421-3425`.

Rust (after `bab208de9d`):
- Command / flow: `session_commands.rs:22-31` (`CompressCommand`),
  `55-340` (`compress_session`); dispatch wiring `dispatch.rs:396-424`,
  `message.rs:240-250`.
- Summarizer trait seam: `agent.rs:97-106`; native impl
  `native_agent.rs:700-716`; step + temperature `native_agent.rs:60-69`,
  `859-876`; client struct `native_agent.rs:229-289`.
- checkpoint_required hard-block: `session_commands.rs:200-201`,
  `1593`.
- Prompt build / redaction: `compression_prompt.rs` (`build`, `SUMMARY_PREFIX`,
  `wrap`, `SUMMARY_ACK`); `compression_redact.rs`.
- Cache release seam (no notification): `session_commands.rs:318`,
  `agent.rs:142-145`.
- Unused aux field: `provider_registry.rs:71`.
- Prior planning docs: `rust/analysis/native-compression-map-claude.md`,
  `native-compression-fix-review-claude.md`.
