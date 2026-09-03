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
- [ ] Wire dispatcher + a first platform adapter into main() (needs an adapter)
- [ ] Platform adapters: Telegram, Discord, Slack, WhatsApp, Signal
- [ ] Native (non-subprocess) agent client, once run_agent.py is ported
- [ ] Session lifecycle + delivery ledger
- [ ] Control socket / drain control
```
```

## Running

```bash
cd rust
cargo run -p hermes-gateway          # serves on 127.0.0.1:8787 by default
HERMES_GATEWAY_BIND=0.0.0.0:8080 cargo run -p hermes-gateway
```
