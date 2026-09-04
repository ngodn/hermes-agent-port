# Hermes Rust rewrite

Full rewrite of hermes-agent from Python to Rust. Goal: lower memory/startup
footprint and a single deployable binary. Strategy is strangler-fig, not
big-bang: we stand up Rust components one at a time behind the boundaries that
already exist in the Python codebase, keep everything running, and stay in
sync with upstream by pinning a known commit as the porting spec.

## Layout

```
rust/
  Cargo.toml            workspace
  crates/
    hermes-core/        shared types + error (no async, no IO)
    hermes-gateway/     the long-lived network process (first target)
```

## Phase order

1. **Gateway** (`crates/hermes-gateway`) — the long-lived, latency-sensitive,
   most self-contained process. Ports `gateway/`. *In progress.*
2. **Tool runtime + RPC host** — tool-calling layer and subprocess/RPC plumbing.
3. **State + search** — SessionDb (conversation history) + FTS5 message search
   done (session_db.rs); full schema/migrations + CJK C ext remain. *In progress.*
4. **Agent core loop** — `run_agent.py`. Last, once contracts are frozen.
5. **TUI / web frontend** — left in TS/React unless there's a reason to move it.

## Deferred (port later, with the subsystem they belong to)

- `turn_context.py` — a Python closure-extraction artifact (~60 opaque handles,
  `[None]` single-element cells simulating shared closure state). Its Rust shape
  will be nothing like this; port it with the TurnRunner/_run_agent_inner loop.
- `session_state.py` legacy dict-view adapters — backward-compat shim for
  pre-refactor Python tests only; no Rust equivalent needed. (Data model ported.)
- `message_timestamps.py` — leans on Python local-tz `%Z` abbreviation (CEST,
  etc.); port with the Phase-4 context-building path where tz handling is
  decided (chrono/chrono-tz), not as a stray leaf.
- `media_policy.py` — a config->env bridge whose only purpose is aligning
  separate Python processes; in Rust the media-path validator reads config
  in-process, so port it together with `platforms/base.py`
  `validate_media_delivery_path`.
- `media_repair.py` — repairs model-mangled computer_use screenshot paths in a
  MEDIA: response; needs `BasePlatformAdapter.extract_media` (MEDIA: directive
  parsing). Port with the media-delivery subsystem (base.py) alongside
  media_policy.
- `wake.py` — wakes a session on a background completion; coupled to the adapter
  base (`MessageEvent`/`handle_message`), the API-server adapter internals, and
  `SessionDB.append_message` with display metadata. Port with the adapter /
  API-server subsystem. (`_delegation_display_metadata` is a pure helper that
  can come along then.)

## Needs a design decision (not a mechanical port)

- `hooks.py` — the event-hook system discovers `~/.hermes/hooks/<name>/` dirs and
  dynamically imports+executes a user-authored Python `handler.py` per event.
  Executing arbitrary user Python is exactly the interpreter capability the
  rewrite drops, so this needs a new hook model in Rust: spawn `python
  handler.py` as a subprocess per event (context as JSON on stdin), support
  executable-script / webhook hooks instead, or drop the feature. The event
  vocabulary + wildcard resolution (`command:*`) port trivially once the
  execution model is chosen. Flagged for the user rather than guessed.
- `stream_dispatch.py` — the event router hangs off the adapter render-hooks and
  the `GatewayStreamConsumer` sink, both in the `stream_consumer.py` hub (3.6k
  LOC, unported). Port it with that subsystem, not against stubs.
- `startup_watchdog.py` — a re-export shim for the repo-root
  `hermes_startup_watchdog` (a pre-import deadlock guard). A compiled binary has
  no import-time deadlock; if a boot-liveness watchdog is wanted it is a separate
  process-level concern, not this shim.

## Not applicable (no Rust analogue)

- `code_skew.py` — detects a Python interpreter running stale code after a hot
  `git pull` (frozen `sys.modules`, first-time lazy imports resolving against a
  stale cached dep). A compiled Rust binary is fully loaded into memory and has
  no lazy imports, so it can never run stale code this way. Nothing to port.

## Stack mapping

| Python            | Rust                         |
|-------------------|------------------------------|
| asyncio           | tokio                        |
| httpx / aiohttp   | reqwest / hyper              |
| fastapi / uvicorn | axum                         |
| pydantic          | serde (+ validator)          |
| sqlite3 + FTS5    | rusqlite (keep C FTS5 ext)   |
| prompt_toolkit    | ratatui + crossterm          |
| jinja2            | minijinja                    |
| croniter          | tokio-cron-scheduler / cron  |

## Gateway port order (from analysis/gateway-map.md)

68 modules directly in `gateway/`. 38 are pure leaves (0 intra-package deps),
2 are the hubs everything hangs off: `run.py` (34,847 LOC, 56 intra + 158
external deps) and `slash_commands.py` (6,576 LOC). Port leaves first, hubs
last. Next concrete targets, in order:

1. `stream_events.py` (171 LOC) — DONE. Ported verbatim as the `StreamEvent`
   enum in `hermes-core/src/stream.rs` (all 7 variants, exhaustiveness compiler-
   enforced). Only deviation: `GatewayNotice.kind` is `notice_kind` in Rust to
   avoid colliding with the enum's `kind` discriminator tag; it is an internal
   representation the bridge maps into, not a direct Python-dataclass decode.
2. `turn_context.py` (150), `session_state.py` (475), `turn_lease.py` (355) —
   per-turn/session state + the lease that serializes a session's turns.
   (turn_lease done; session_state data model done; turn_context deferred.)
3. `response_filters.py` (147), `display_config.py` (322) — done (leaves).
4. `platform_registry.py` (699) — done (platform.rs).
5. Status rollups (`disk_status.py`, `memory_status.py`) — done.
6. hubs last: `config.py`, `session.py`, `stream_consumer.py`, `status.py`,
   `slash_commands.py`, `run.py`.

## Gateway status

- [x] Workspace + toolchain + build
- [x] Server skeleton (axum) with `/healthz`, `/readyz`, graceful shutdown
- [x] Env-driven config (`HERMES_GATEWAY_BIND`)
- [x] Agent boundary trait (`AgentClient`) + `StreamEvent` contract
- [x] Strangler bridge: Rust drives the Python agent via
      `python -m hermes_cli.stream_turn` (JSONL over stdio), tested end to end
      (agent::tests::empty_prompt_terminates_cleanly)
- [x] Turn-dispatch loop (`Dispatcher`)
- [x] Runnable end to end: `POST /message` runs a turn through the agent and
      returns the reply (the "local" adapter over HTTP). Config via
      `HERMES_AGENT_PYTHON` / `HERMES_AGENT_CWD` / `HERMES_AGENT_MODEL`.
- [x] Dispatcher wired into main() with a real push-based adapter
- [x] Telegram adapter (long-poll getUpdates -> Dispatcher -> sendMessage),
      started when `HERMES_TELEGRAM_TOKEN` is set. Boot + graceful-backoff
      verified against the live API.
- [x] Discord adapter (Gateway WebSocket: HELLO/heartbeat/IDENTIFY/dispatch ->
      Dispatcher -> REST sendMessage), started when `HERMES_DISCORD_TOKEN` set.
      WS handshake verified against the live gateway.
- [x] Slack adapter (Socket Mode: apps.connections.open -> WS -> events_api
      envelopes with ack -> Dispatcher -> chat.postMessage), started when both
      `HERMES_SLACK_APP_TOKEN` and `HERMES_SLACK_BOT_TOKEN` are set. Handshake
      verified against the live API.
- [ ] More platform adapters: WhatsApp (Baileys bridge), Signal
- [x] Native (in-Rust) agent client for plain chat turns: opt-in via
      `HERMES_AGENT_NATIVE=1` + `HERMES_LLM_API_KEY`, calls an OpenAI-compatible
      `/chat/completions` and streams the reply (no Python). Narrow scope: no
      tools/history/memory yet. Auth path verified against OpenRouter (401 on a
      bad key, no spend); request-building + SSE parsing unit-tested.
- [x] CLI-backend agent client (`HERMES_AGENT_CLI`, e.g. claude/agy): runs a
      turn via an external agent CLI, no Python and no HTTP key. VERIFIED live:
      POST /message -> agy (gemini-3.8-flash-high) -> real reply, zero Python.
      This is the path that works for a CLI-backend hermes setup.
- [x] Native HTTP tool loop wired into run_turn (opt-in `HERMES_AGENT_TOOLS=1`;
      native_tools.rs ChatModel + run_tool_loop + CurrentTimeTool).
- [x] Conversation history: SessionDb (state.db) persists per-session
      user/assistant messages; the turn path loads prior history and threads it
      into stateless backends (native HTTP messages array, CLI transcript). The
      Python bridge opts out (manages_history) since it owns its own history.
      VERIFIED live multi-turn via agy: "my name is Denny" then "what is my
      name?" -> "Denny". Memory/skills (rest of run_agent.py) still remain.
- [x] Delivery ledger (durable outbound obligations): SQLite ledger over
      state.db (record/attempting/delivered/failed + startup sweep_recoverable
      that claims dead-owner rows via pid+/proc start-time liveness, with
      at-least-once markers; attempts cap + stale/retention pruning). Not yet
      wired into the delivery path; runtime-reconnect sweep deferred.
- [x] Drain control (drain_control.py): external drain-marker contract
      (.drain_request.json), with instantiation-epoch (boot_id + PID1 start) and
      max-age staleness so a restart/orphan clears the drain. Contract + tests
      done; the drain watcher that flips the gateway to "draining" is not wired.
- [x] Control socket (control_socket.py): local owner-only UDS answering
      identify/status (one JSON line in/out), with the sun_path fallback +
      pointer file. Wired live and cleaned up on shutdown; verified via a real
      UDS client (identify payload, unknown-verb listing, 0600 perms).

### Leaf modules ported (self-contained, tested; wiring into their real call
sites tracked separately)

- [x] `rich_sent_store.py` -> rich_sent_store.rs (Telegram rich-send text index)
- [x] `restart_loop_guard.py` -> restart_loop_guard.rs (auto-resume respawn breaker)
- [x] `sticker_cache.py` -> sticker_cache.rs (Telegram sticker description cache)
- [x] `systemd_notify.py` -> systemd_notify.rs (sd_notify READY/WATCHDOG/STOPPING
      + a tokio watchdog that feeds only while the runtime keeps making progress)
- [x] `cwd_placeholder.py` -> cwd_placeholder.rs (TERMINAL_CWD placeholder resolution)
- [x] `status_phrases.py` -> status_phrases.rs (generic status-line catalog;
      built-in asset embedded, profile-relative user catalogs, recent-repeat avoidance)
- [x] `runtime_footer.py` -> runtime_footer.rs (final-message metadata footer)
- [x] `disk_status.py` -> disk_status.rs (/api/status disk block; statvfs sample)
- [x] `memory_status.py` -> memory_status.rs (/api/status memory block; reads the
      persisted heartbeat + lifecycle sentinel, no new sampling)
- [x] `cgroup_cleanup.py` -> cgroup_cleanup.rs (ExecStopPost cgroup reaper)
- [x] `session_db_recovery.py` -> session_db_recovery.rs (recoverable per-path
      handle cache; single-flight opens, exponential backoff, health aggregate)
- [x] `profile_routing.py` -> profile_routing.rs (+ profile_name.rs for the
      profile-id path-traversal guard from hermes_cli/profiles.py)
- [x] `agent_cache_pressure.py` -> agent_cache_pressure.rs (pure/OS parts:
      bounds resolution, cgroup/total-mem limits, anon RSS, eviction planner;
      the AIAgent-shaped guard + sweep land in Phase 4)

## Running

    cd rust && cargo run -p hermes-gateway
    # then, from the hermes repo root as agent cwd:
    HERMES_AGENT_CWD=/path/to/hermes-agent-port cargo run -p hermes-gateway
    curl -s localhost:8787/healthz
    curl -s -X POST localhost:8787/message \
      -H 'content-type: application/json' -d '{"text":"hello"}'

Platform push paths start when their tokens are set: `HERMES_TELEGRAM_TOKEN`,
`HERMES_DISCORD_TOKEN`, `HERMES_SLACK_APP_TOKEN` + `HERMES_SLACK_BOT_TOKEN`.

## Footprint / deploy

Release binary is ~5 MB (LTO + strip), TLS roots compiled in. Build a
container with `rust/Dockerfile` (multi-stage -> debian-slim, non-root):

    cd rust && docker build -t hermes-gateway .
    docker run -p 8787:8787 -e HERMES_TELEGRAM_TOKEN=... hermes-gateway

The gateway runs standalone (health + adapters); to also run the strangler
agent bridge, mount the hermes repo + Python and set HERMES_AGENT_CWD /
HERMES_AGENT_PYTHON.
