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
3. **State + search** — `hermes_state*.py`, `hermes_state_search.py` onto
   rusqlite + FTS5 (reuse the existing C ext in `native/fts5_cjk/`).
4. **Agent core loop** — `run_agent.py`. Last, once contracts are frozen.
5. **TUI / web frontend** — left in TS/React unless there's a reason to move it.

## Deferred (port later, with the subsystem they belong to)

- `turn_context.py` — a Python closure-extraction artifact (~60 opaque handles,
  `[None]` single-element cells simulating shared closure state). Its Rust shape
  will be nothing like this; port it with the TurnRunner/_run_agent_inner loop.
- `session_state.py` legacy dict-view adapters — backward-compat shim for
  pre-refactor Python tests only; no Rust equivalent needed. (Data model ported.)

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

1. `stream_events.py` (171 LOC) — the agent->gateway delivery contract; pins
   down the real `AgentEvent` shape (currently a minimal guess in agent.rs).
2. `turn_context.py` (150), `session_state.py` (475), `turn_lease.py` (355) —
   per-turn/session state + the lease that serializes a session's turns.
3. `response_filters.py` (147), `display_config.py` (322) — pure leaves.
4. `platform_registry.py` (699) — done (platform.rs).
5. hubs last: `config.py`, `session.py`, `stream_consumer.py`, `status.py`,
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
- [ ] More platform adapters: WhatsApp (Baileys bridge), Signal

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
