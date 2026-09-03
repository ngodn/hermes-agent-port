# Hermes gateway (Rust)

A Rust rewrite of the Hermes agent, starting with the gateway. The goal is a
smaller-footprint, single-binary deployment. This is a strangler-fig port: the
Rust gateway is built up one component at a time and drives the existing Python
agent as a subprocess until the agent itself is ported. See [PORT.md](PORT.md)
for the full plan, status, and module map.

## What works today

The gateway is a runnable multi-platform service:

- **HTTP surface** (axum): `/healthz`, `/readyz`, `POST /message` (a local
  request/response turn), `GET /display/:platform` (resolved display config).
- **Platform push paths**, each started when its token is set, all flowing
  through one `PlatformAdapter -> Dispatcher` seam:
  - **Telegram** — HTTP long-poll (`getUpdates` / `sendMessage`).
  - **Discord** — Gateway WebSocket (HELLO/heartbeat/IDENTIFY/dispatch) +
    REST send.
  - **Slack** — Socket Mode (`apps.connections.open` -> WS -> acked
    `events_api` envelopes) + `chat.postMessage`.
- **Turn dispatch**: per-session serialization (turn lease), intentional-
  silence suppression, and slash-command handling.
- **Slash commands**: per-platform/per-scope access gating (`slash_access`)
  plus built-in `/help`, `/whoami`, `/status` answered without an agent turn.
- **Config**: loads `$HERMES_HOME/config.yaml` and resolves per-platform
  display settings against tiered defaults.
- **Agent bridge**: runs the Python agent via `python -m hermes_cli.stream_turn`
  (a JSONL streaming shim added for the port) and maps its events to the
  internal stream contract.
- **Operations**: coordinated graceful shutdown (SIGINT/SIGTERM drains push
  paths + HTTP together), ~5 MB release binary, a multi-stage Dockerfile
  (non-root), clippy-clean, ~100 unit/integration tests.

## Layout

```
crates/
  hermes-core/     shared types: Platform, Message, StreamEvent, error
  hermes-gateway/  the gateway binary and all its modules
```

## Run

```bash
cargo run -p hermes-gateway            # 127.0.0.1:8787 by default
curl -s localhost:8787/healthz
curl -s -X POST localhost:8787/message \
  -H 'content-type: application/json' -d '{"text":"hello"}'
```

To run a real turn, point the agent bridge at a Python hermes checkout:

```bash
HERMES_AGENT_CWD=/path/to/hermes-agent-port \
HERMES_AGENT_PYTHON=python3 \
cargo run -p hermes-gateway
```

Start platform push paths by setting tokens: `HERMES_TELEGRAM_TOKEN`,
`HERMES_DISCORD_TOKEN`, `HERMES_SLACK_APP_TOKEN` + `HERMES_SLACK_BOT_TOKEN`.

## Container

```bash
docker build -t hermes-gateway .
docker run -p 8787:8787 -e HERMES_TELEGRAM_TOKEN=... hermes-gateway
```

## Test

```bash
cargo test           # unit + integration (one ignored test needs python3)
cargo clippy --all-targets
```
