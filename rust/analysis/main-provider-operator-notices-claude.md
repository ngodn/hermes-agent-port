# Main-provider operator notices: Python map and Rust delivery seam

Checkpoint after run-budget-aware stale scaling. This maps only the user/operator-facing
notices the **main-provider chat inference path** emits (waits, retry/fallback status,
provider-switch, refusals), plus the Rust delivery seam they need. Out of scope by
instruction and not analyzed here: run-budget scaling itself, dropped-stream recovery,
general retry policy, non-chat transports (Codex/Bedrock), OAuth mechanics, plugins,
memory. Where those touch a notice they are noted only as neighbours.

A note on quoting: several Python notice strings contain a literal U+2014 em dash. To keep
this document em-dash-free while staying byte-faithful, those are written as `\u2014` in the
quoted text below. The real source byte is the em dash character.

All line references were verified against the current checkout (branch `rust-rewrite`).

---

## 1. The notice spine (Python)

`AIAgent` (`run_agent.py`) exposes four driver-agnostic callbacks, set at construction
(`run_agent.py:523` thinking, `:536` status, `:537` notice, `:538` notice_clear; forwarded
`:616`, `:629-631`). Seven helpers sit on top, every one fail-open (a callback exception is
swallowed and must never break the agent loop):

| Helper | Line | Kind | Channels |
|---|---|---|---|
| `_emit_status(msg)` | `run_agent.py:1083` | immediate | `_vprint(force=True)` + `status_callback("lifecycle", msg)` |
| `_emit_warning(msg)` | `run_agent.py:1103` | immediate | `_vprint(force=True)` + `status_callback("warn", msg)` |
| `_emit_wait_notice(text)` | `run_agent.py:1210` | immediate, live | `_touch_activity(text)` + `thinking_callback(text)` |
| `_emit_notice(notice)` | `run_agent.py:1189` | immediate, structured | `notice_callback(AgentNotice)` |
| `_emit_notice_clear(key)` | `run_agent.py:1202` | immediate, structured | `notice_clear_callback(key)` |
| `_buffer_status` / `_buffer_vprint` | `run_agent.py:1245` / `:1265` | deferred | append `(kind, text)` to `self._retry_status_buffer` |
| `_clear_status_buffer` / `_flush_status_buffer` | `run_agent.py:1276` / `:1315` | deferred sink | drop on success / replay on terminal failure |
| `_emit_pending_fallback_notice()` | `run_agent.py:1285` | one-shot durable | replays `self._pending_fallback_notice` via `_emit_status` once |

There are two payload shapes:

- **Plain string** through `status_callback(kind, text)` where `kind` is `"lifecycle"` or
  `"warn"`. This is the whole main-provider retry/fallback/wait surface.
- **Structured `AgentNotice`** (`agent/credits_tracker.py:199`) through
  `notice_callback`/`notice_clear_callback`. Fields: `text`, `level` (`info|warn|error|success`),
  `kind` (`sticky|ttl`), `ttl_ms`, `key`, `id`. This channel is used today only by the credits
  family (`evaluate_credits_notices`, `credits_tracker.py:269`), which is a separate concern from
  main-provider inference and is out of scope here except as the shape a future structured notice
  would reuse.

`_emit_wait_notice` is deliberately a third thing: it does not go through `status_callback`. It
rewrites the **live** status/spinner line via `thinking_callback` and stamps the activity
tracker, because a wait is a transient "still alive, here is what I am waiting on" signal, not a
turn-log line.

---

## 2. Delivery seam (Python): who consumes each callback

Three drivers bind the callbacks differently. This is the key input for the Rust seam because
each has different affordances (a live editable line vs an append-only chat).

**CLI REPL** (`hermes_cli/cli_agent_setup_mixin.py:570,583,584`): binds `thinking_callback`,
`notice_callback`, `notice_clear_callback`, and deliberately does **not** bind `status_callback`.
So on the CLI, `_emit_status`/`_emit_warning` are console-only via `_vprint`. Handlers in
`cli.py`: `_on_thinking` (`:8073`) sets the prompt_toolkit spinner text; `_on_notice` (`:8081`)
queues color-coded lines flushed at turn end by `_flush_credit_notices` (`:8101`);
`_on_notice_clear` (`:8120`) is a no-op (a printed REPL line cannot be retracted).

**Messaging gateway** (`gateway/run.py`): `status_callback = ctx._status_callback_sync` (`:6426`)
posts or edits a status **message on the messaging platform**; `notice_callback` (`:6456`)
renders and pushes a platform notice; `notice_clear_callback = None` (`:6457`) because a sent
message cannot be retracted. It also feeds the `"⏳ Working \u2014 N min"` heartbeat: any
`_emit_wait_notice` calls `_touch_activity`, which stamps `_last_activity_desc`
(`run_agent.py:4543`) and projects it durably via `touch_session_activity`
(`run_agent.py:4584`, rate-limited to >=30s); the gateway heartbeat reads that description back
into the "Working" line.

**TUI / Desktop gateway** (`tui_gateway/server.py`): each callback maps to a WebSocket event:
`thinking_callback` -> `thinking.delta` (`:8478`), `status_callback` -> `_status_update`
(`:3113`), `notice_callback` -> `notification.show` (`:8494`), `notice_clear_callback` ->
`notification.clear` (`:8506`).

Takeaway for the port: the notice text is authored once in the agent; each surface decides
rendering. The live-editable surfaces (CLI spinner, TUI status line) are the only ones that can
show and later clear a wait; the append-only surfaces (messaging) can only add a line, which is
why `notice_clear_callback` is `None` there.

---

## 3. Main-provider chat notice inventory

Scope filter applied: only notices reachable on the chat completions / anthropic-messages main
path. The chat call funnels through `agent/conversation_loop.py:3638`
(`_interruptible_streaming_api_call`), which either streams (its nested `_monitor_loop`) or falls
through to the non-stream `_interruptible_api_call` at `agent/chat_completion_helpers.py:3708`.
Codex TTFB/idle kills (`chat_completion_helpers.py:1772,1778,1788,1829`, gated on
`api_mode == "codex_responses"` at `:1609`) and Bedrock (`:3926`) are transport-specific and
excluded; they are listed once at the end as neighbours only.

### 3a. Live wait notices (streaming path, via `_emit_wait_notice`)

Emitted from `_monitor_loop` inside `_interruptible_streaming_api_call`
(`chat_completion_helpers.py:3640`), which polls every 0.3s.

| Line | Trigger / timing | Verbatim text |
|---|---|---|
| `:5593` | Local managed server, no chunk >=2s, ~1s poll cadence; `_managed_local_load_notice` returns non-None | `_load_notice`, e.g. `"⏳ loading {model} into memory \u2014 {N}% (responses start once the model is loaded)"` (`:1059`) |
| `:5611` | Load phase ended: 3 sustained missed load samples (`_load_notice_misses >= 3`) | `""` (clears the live load line) |
| `:5636` | 30s heartbeat AND no chunk for >=30s (`_HEARTBEAT_INTERVAL = 30.0`, `:5561`); re-fires every 30s | `"⏳ waiting on {model} \u2014 {N}s with no output yet (provider may be slow or overloaded, or the model is thinking{_recovery})"` where `_recovery` = `"; auto-reconnect at {int(stale)}s"` when the stream stale timeout is finite, else `""` |
| `:5698` | Stale-stream kill fired (see 3b) | `"⚠ no output from provider for {N}s \u2014 reconnecting..."` |

The 30s heartbeat has a quiet else branch (`:5644`): when chunks are flowing it only touches
activity (`"waiting for stream response ({N}s, no chunks yet)"`) and leaves the live line alone.

### 3b. Buffered retry/fallback status (via `_buffer_status` / `_buffer_vprint`)

Deferred: written to `_retry_status_buffer`, shown only if the whole turn ultimately fails
(section 4). Main-path entries only:

| Line | Path | Trigger | Verbatim (abridged where dynamic) |
|---|---|---|---|
| `chat_completion_helpers.py:787` | non-stream | wall-clock stale kill (`_report_stale_nonstream_kill`) | `"⚠️ No response from provider for {N}s (non-streaming, model: {model}). {hint or 'Aborting call.'}"` |
| `chat_completion_helpers.py:5660` | streaming | stale-stream kill, `_stale_elapsed > _stream_stale_timeout` (`:5652`) | `"⚠️ No response from provider for {N}s (model: {model}, context: ~{T} tokens). Reconnecting..."` |
| `chat_completion_helpers.py:5395` | streaming | stream retries exhausted (`_max_stream_retries`, default 2) | `"❌ Connection to provider failed after {N} attempts. The provider may be experiencing issues \u2014 try again in a moment."` (and an empty-stream variant at `:5382`) |
| `chat_completion_helpers.py:3159` | control | fallback activated (also appended to `_pending_fallback_notice`) | `"⚠️ Model fallback: {old_model} via {old_provider} unavailable ({reason}); using {fb_model} via {fb_provider}."` |
| `conversation_loop.py:3850` | either | empty/malformed response, eager fallback available | `"⚠️ Empty/malformed response \u2014 switching to fallback..."` |
| `conversation_loop.py:3915-3919` | either | invalid API response, no eager fallback (multi-line vprint: attempt, provider, message, hint) | `"⚠️  Invalid API response (attempt {n}/{max}): ..."` + provider/message/hint lines |
| `conversation_loop.py:3924` | either | invalid-response retries exhausted, fallback pending | `"⚠️ Max retries ({max}) for invalid responses \u2014 trying fallback..."` |
| `conversation_loop.py:3950` | either | below max retries, backoff before retry | `"⏳ Retrying in {t}s ({hint})..."` |
| `conversation_loop.py:4100` | either | content-policy refusal, fallback pending | `"⚠️ Model declined to respond (safety refusal) \u2014 trying fallback..."` |
| `conversation_loop.py:4524,4529` | streaming | truncated tool call, retry (<4): stub-stall vs plain | `"⚠️  Stream interrupted mid tool-call \u2014 retrying ({n}/4)..."` / `"⚠️  Truncated tool call detected \u2014 retrying API call ({n}/4)..."` |
| `conversation_loop.py:5064,5068` | either | surrogate strip / encode error, retry | `"⚠️  Stripped invalid surrogate characters from messages. Retrying..."` / `"⚠️  Surrogate encoding error \u2014 retrying after full-payload sanitization..."` |
| `conversation_loop.py:5466,5476,5486,5517` | either | 401 credential refresh succeeded, per provider (xAI/Codex, Vertex, Nous, Copilot) | `"🔐 {label} auth refreshed after 401. Retrying request..."` etc. |
| `conversation_loop.py:5780-5831` | either | generic API-call-failed diagnostic block (attempt, provider, endpoint, error, elapsed/context, plus OpenRouter tool-routing and model-prefix hints) | multi-line `_buffer_vprint` |
| `conversation_loop.py:5957` | either | Anthropic 1M-tier not entitled, reduce context | `"⚠️  Anthropic long-context tier requires extra usage \u2014 reducing context: {old} → {new} tokens"` |
| `conversation_loop.py:5977` | either | compression retry, context reduced | `COMPRESSION_RETRY_CONTEXT_REDUCED_STATUS_TEMPLATE` |
| `conversation_loop.py:6061,6068,6074,6078,6082` | either | rate-limit/billing/transport failover decision (upstream / billing verified / billing unverified / unreachable / generic) | e.g. `"⚠️ Rate limited \u2014 switching to fallback provider..."` |
| `conversation_loop.py:6113` | either | auth-failover to fallback provider | `"🔐 Authentication failed and could not be refreshed \u2014 switching to fallback provider..."` |
| `conversation_loop.py:6251` | either | 413 payload too large, compress and retry | `"⚠️  Request payload too large (413) \u2014 compression attempt {n}/{max}..."` |

### 3c. Immediate status / warning (via `_emit_status` / `_emit_warning`)

These fire now, not buffered, because they are terminal or an unmissable one-shot:

| Line | Trigger | Verbatim |
|---|---|---|
| `conversation_loop.py:2945` | Ollama runtime context too small (terminal) | `"❌ Ollama runtime context is too small for Hermes tool use"` |
| `conversation_loop.py:3113` | pre-API compression actually ran | dynamic (`automatic_compaction_status_message`, `PRE_API_COMPRESSION_STATUS_TEMPLATE`) |
| `conversation_loop.py:3935` | invalid-response retries exceeded, giving up (after flush) | `"❌ Max retries ({max}) exceeded for invalid responses. Giving up."` |
| `conversation_loop.py:4124` | refusal terminal (after flush) | `"⚠️ The model declined to respond to this request (safety refusal)."` |
| `conversation_loop.py:4324` | streaming content-filter termination, fallback available | `"Content filter terminated stream; switching to fallback..."` |
| `conversation_loop.py:2767` | pending sanitizer-heal notice at send | dynamic `_heal_notice` |

Note: the session-turn-lease status/warnings at `run_agent.py:9432,9437,9496,9520` are a
different family (session coordination, not main-provider inference) and are excluded.

### 3d. The durable fallback-switch notice

`_pending_fallback_notice` is set where a fallback activates (`chat_completion_helpers.py:3158`)
to the same `"⚠️ Model fallback: ..."` text that is also buffered. It is a **list** so multiple
switches in one turn are retained in order. On a recovered turn it is surfaced exactly once by
`_emit_pending_fallback_notice` (section 4), so a provider/model switch stays visible even though
the noisy retry buffer is dropped.

---

## 4. Buffer / flush / clear lifecycle

This is the load-bearing behavior and the part most likely to be gotten wrong in a port.

- **On successful content** (`conversation_loop.py:8805-8806`, verified verbatim): call
  `_emit_pending_fallback_notice()` first, then `_clear_status_buffer()`. So a turn that
  recovered surfaces the one-shot switch line and silently drops all the transient retry
  chatter.
- **On terminal failure**: `_flush_status_buffer()` replays the full buffered trace so the user
  can see everything that was tried. Flush also nulls `_pending_fallback_notice`
  (`run_agent.py:1325`) so the switch line surfaces via the buffered trace and is never
  duplicated. Flush is not centralized; it is duplicated at every give-up return. Verified main-path
  flush sites: `conversation_loop.py:1657,1682,3370,3934,4112,4550,4600,5896,6235`. Additional
  flush sites exist beyond line 6251 (reported around `:6348,6499,6590,6696,6838,7047,7557,7808`);
  a port must flush on every terminal main-path return, not just one.

Invariant: `_flush_status_buffer` and `_emit_pending_fallback_notice` are mutually exclusive for
the fallback line. Recovery emits the pending notice and clears the buffer; failure flushes the
buffer (which already carries the switch line) and discards the pending notice. Either way the
switch is shown exactly once.

---

## 5. Behavioral vs presentation-only

The central classification the port needs:

**Presentation-only diagnostics (no saved state, no control-flow dependency):** every notice in
section 3. `_emit_status`, `_emit_warning`, `_emit_notice`, `_emit_wait_notice`, and the entire
`_buffer_*` / `_flush` / `_clear` family only call driver callbacks and `_vprint`. None writes to
`self.messages`, the transcript, or the session DB. The retry/fallback control flow is driven by
exceptions, classifier results, `_try_activate_fallback`, and retry counters, not by whether a
notice fired. Dropping every notice would change nothing an assistant reply or a resumed session
can observe.

**The one behavioral edge is the activity projection, not a notice.** `_emit_wait_notice` calls
`_touch_activity` (`run_agent.py:4491`), which stamps `_last_activity_desc` (`:4543`) and
projects it durably via `touch_session_activity` (`:4584`, rate-limited >=30s). That projection
is observation-only (it feeds the gateway inactivity watchdog and the "Working \u2014 N min"
heartbeat) and is explicitly not conversation history. It is a liveness signal, not transcript.
So even here the *notice text* is presentation; only the *liveness stamp* has a downstream
consumer, and that consumer is a watchdog, not the model.

Consequence for the port: operator notices are a delivery concern only. Getting a notice text
slightly wrong is a UX regression, not a correctness bug. The only thing that must stay faithful
for behavior is that a wait keeps the liveness/heartbeat alive during long provider stalls (which
the Rust stale/inactivity timeouts already handle structurally).

---

## 6. Cache / transcript invariants

- No notice is ever appended to messages or persisted to the transcript. The Rust port must keep
  this: a `GatewayNotice` or any status event must not enter conversation history, must not
  affect the prompt-cache key, and must not be replayed on resume. This matches the existing Rust
  `StreamEvent` module contract ("These carry transport/presentation only. Nothing here is
  conversation history", `rust/crates/hermes-core/src/stream.rs:13-14`).
- The buffered trace is a private in-memory list per turn; it is cleared on success and drained on
  failure. It has no durable form. A port should keep it turn-scoped and never persist it.
- A wait notice must be idempotent with respect to state: emitting or clearing the live line must
  not touch cache, usage, or history. Only the liveness timestamp updates, which the Rust timeout
  layer already owns.

---

## 7. Retry / fallback interaction (scope boundary)

Only the notice coupling is in scope. The retry counters, backoff, classifier, and
`_try_activate_fallback` decision are the general retry policy and are out of scope. What matters
for notices: the buffer is the join point. Each retry/fallback branch writes a buffered line; the
single success path (`8805-8806`) decides drop-plus-one-shot; each failure path flushes. Any Rust
port of the notice surface must hook the same three lifecycle moments (per-attempt buffer,
on-success clear-after-pending, on-terminal-failure flush) rather than emitting eagerly, or it
will reproduce the pre-buffer "10+ retry lines for one transient 429" noise the buffer was built
to fix (`run_agent.py:1234-1243`).

---

## 8. Current Rust surface

**Event vocabulary** (`rust/crates/hermes-core/src/stream.rs:25-105`): `MessageChunk`,
`MessageStop`, `Commentary`, `ToolCallChunk`, `ToolCallFinished`, `LongToolHint`,
`ApprovalRequest`, `GatewayNotice { notice_kind, text, extra }`. There is **no** dedicated
thinking / wait / warning / retry / fallback variant. `GatewayNotice` (`:98-104`) is the only
generic notice channel; its doc already anticipates stable `notice_kind` switch strings
(`"restart" / "online" / "long_run" / ...`).

**Emission today**: the sole production `GatewayNotice` is the memory-recall indicator
(`native_agent.rs:5099-5105`, `notice_kind: "memory_recall"`), gated on the extension host. The
main-provider inference emits only `MessageChunk` + `MessageStop` (terminal/refusal/error text at
`native_agent.rs:4773-5047`) and passes through tool events. `forward_sse`
(`native_agent.rs:5641`) emits `MessageChunk`/`MessageStop` only; it scrubs think blocks
(`ThinkScrubber`, `:5660`) and emits no status. `LongToolHint` is defined but never emitted
anywhere.

**Consumers**: both gateway sinks explicitly drop notices.
- Dispatcher `run_admitted_turn` recv loop (`dispatch.rs:717-758`): renders `MessageChunk`,
  `Commentary`, `MessageStop`, `ApprovalRequest`; `ToolCallChunk | ToolCallFinished |
  LongToolHint | GatewayNotice => {}` (`:752-756`, "not rendered in this pass").
- HTTP `post_message` recv loop (`message.rs:465-484`): same, with `ApprovalRequest` only a
  `warn!`; `LongToolHint | GatewayNotice => {}` (`:479-482`).
- Bridge mapper (`agent.rs`): `BridgeEvent::ThinkingDelta` maps to nothing (dropped).

**Timeout policy** (`main_provider_timeouts.rs`): `Policy` with `buffered_stale_timeout`
(`:227`, the non-stream watchdog the run-budget checkpoint caps), `stream_stale_timeout`
(`:183`), `stream_inactivity_timeout` (`:210`), plus `request_timeout`/`stream_attempts`/
`stale_giveup`. Consumers at `native_agent.rs:2638-2657` and around `:5882`. This is where a
Rust wait/stale notice would be sourced from, since the timeouts already compute the exact
thresholds the Python notice text quotes (`auto-reconnect at {stale}s`).

---

## 9. The gap

1. **No notice reaches a user.** `GatewayNotice` and `LongToolHint` are discarded at both sinks.
   Any ported operator notice needs both an emission site and a rendering branch at
   `dispatch.rs:752-756` and `message.rs:479-482`, which today swallow them.
2. **No wait/liveness notice.** Python surfaces "waiting on {model} \u2014 {N}s with no output"
   every 30s and "no output ... reconnecting" on a stale kill. Rust has the stale/inactivity
   timeouts that fire the underlying recovery but emits no operator text while waiting.
3. **No buffered retry/fallback trace and no lifecycle.** Rust has no per-turn status buffer, no
   drop-on-success, no flush-on-failure, and no one-shot fallback-switch notice. The main-provider
   fallback/retry checkpoints already ported the *control flow*; the *operator-visible trace* of
   what was tried is absent.
4. **No structured-notice channel parity.** `AgentNotice` (level/kind/ttl/key/id, and the
   clear-by-key path) has no Rust analogue; `GatewayNotice` carries only `notice_kind`/`text`/
   `extra` and there is no clear-by-key event. This mostly matters for the live-editable surfaces
   (a wait line that is shown then cleared).

---

## 10. Narrowest deep Rust interface

The whole surface is presentation, so the seam should be one pure formatter plus one thin
per-turn buffer, keeping `native_agent.rs` free of notice-string logic.

- **A pure notice module** (e.g. `operator_notices.rs`) with no I/O:
  - `fn wait_notice(model: &str, waited: Duration, stale: Option<Duration>, phase: WaitPhase) -> String`
    reproducing the 30s heartbeat and the reconnect line, including the `auto-reconnect at {stale}s`
    suffix only when the stale timeout is finite. `WaitPhase` covers no-output, local-load, and
    reconnecting.
  - `fn fallback_switch(old: &Route, new: &Route, reason: FailoverReason) -> String` and the
    per-reason failover lines (rate limit / billing / transport / auth) as pure functions over the
    already-classified failure, so the exact Python text is a table, testable without a provider.
  - Return owned `String`s; the caller wraps them in `StreamEvent::GatewayNotice` with a stable
    `notice_kind` (`"provider_wait"`, `"provider_reconnect"`, `"fallback_switch"`,
    `"retry_trace"`).
- **A turn-scoped buffer** owned by the run-turn task (not the client): push buffered lines,
  `clear()` on success after emitting the one-shot fallback notice, `flush()` (emit all) on
  terminal failure. This mirrors `8805-8806` exactly. Keep it a plain `Vec<(NoticeKind, String)>`;
  do not persist it.
- **Two rendering branches** at the existing sinks (`dispatch.rs:752-756`, `message.rs:479-482`):
  switch on `notice_kind` and deliver. The messaging surface appends (no clear); a future live
  surface can honor a clear-by-key if `GatewayNotice` gains a `clear` companion. Start with
  append-only to match the gateway's current `notice_clear_callback = None` semantics.

Deep-module payoff: `native_agent.rs` calls `notices.buffer(wait_notice(...))` /
`notices.fallback(...)` and, at the two lifecycle points, `notices.clear_after_pending(events)` /
`notices.flush(events)`. All string shapes and the drop/flush rule live behind that interface;
the SSE loop and the pool loop stay about transport.

---

## 11. Public end-to-end tests

**Python focused tests to mirror** (all verified present):
- `tests/run_agent/test_retry_status_buffer.py`: buffering emits nothing then flush replays exact
  texts; clear drops silently; vprint replays with log prefix and `force=True`; mixed kinds route
  to the right channels; **pending fallback emitted once** (`"🔄 Switched to fallback model: m1 via
  p1 → m2 via p2"` in the fixture); all switches in order; flush discards pending fallback so no
  duplicate; callbacks that raise do not break the drain.
- `tests/run_agent/test_wait_state_visibility.py`: `_emit_wait_notice` updates spinner text
  (`"⏳ waiting on test-model \u2014 30s with no response yet"`) and `last_activity_desc`; still
  touches activity with no `thinking_callback`; the non-stream wait loop emits a reconnect notice
  containing "no response from provider".
- `tests/run_agent/test_notice_spine.py`: `_emit_notice` calls the callback with the exact notice;
  `notice_callback` is an `__init__` param; gateway emits `notification.show`; payload is the full
  snake_case dict `{text,level,kind,ttl_ms,key,id}`; clear maps to `notification.clear` with the
  key.
- `tests/gateway/test_notice_rendering.py`: text rendered verbatim with its baked glyph, no double
  glyph; public delivery sends the rendered line to the platform.

**Proposed Rust run-turn tests** (public, end-to-end through the gateway sink):
1. A slow stream that produces no chunk for the heartbeat interval yields a `GatewayNotice`
   wait line quoting the model and elapsed, and (when the stale timeout is finite) the
   `auto-reconnect at {N}s` suffix; a finite vs infinite (local) timeout switches that suffix.
2. A recovered turn after a transient failure surfaces exactly one fallback-switch notice and no
   buffered retry chatter (drop-on-success).
3. A terminal failure flushes the full buffered trace, including the switch line exactly once
   (flush discards the pending notice).
4. A stale-stream kill emits the "no output ... reconnecting" notice and the recovery still
   proceeds (notice does not gate control flow).
5. Notices never enter the transcript: assert the persisted messages and the prompt-cache key are
   identical with and without notices firing.
6. Both sinks (dispatcher and HTTP) render the notice rather than dropping it (guards against the
   current `=> {}` regression).

---

## 12. Risks and cautions

- **Do not emit eagerly.** The buffer exists to prevent retry-line spam
  (`run_agent.py:1234-1243`). A port that sends each attempt as a live `GatewayNotice` reintroduces
  the exact noise the buffer fixed. Buffer, then drop-or-flush.
- **Fallback line exactly once.** The mutual exclusion between `_emit_pending_fallback_notice`
  (recovery) and `_flush_status_buffer` (failure) is the invariant to preserve; a naive port that
  both buffers the switch and separately emits a pending notice on failure will double it.
- **Keep the liveness stamp even if the text is dropped.** The only behavioral thread is
  `_touch_activity` feeding the heartbeat/watchdog. Whatever the port does with notice text, a long
  provider wait must keep the liveness signal fresh (Rust already does this structurally via the
  timeout layer, so do not couple it to notice rendering).
- **Transport-specific neighbours are not this checkpoint.** Codex TTFB/idle
  (`chat_completion_helpers.py:1772,1778,1788,1829`) and Bedrock (`:3926`) produce similarly worded
  notices but are gated on non-chat transports and belong to their own ports. Do not fold their
  text into the chat seam.
- **Clear-by-key is a live-surface luxury.** The messaging gateway has `notice_clear_callback =
  None` because a sent message cannot be retracted. Start append-only. The empty-string wait clear
  (`chat_completion_helpers.py:5611`) only makes sense on an editable line (CLI spinner / TUI
  status); on the append-only gateway it is simply not emitted, which is correct, not a gap.
