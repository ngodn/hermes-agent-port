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

## Subsystems ported (cohesive units, not leaves)

- **Media delivery** (`platforms/base.py` core -> media.rs) + `media_policy.py`
  -> media_policy.rs + `media_repair.py` -> media_repair.rs. The security gate
  (validate_media_delivery_path: denylist, allowlist, strict/recency, symlink
  resolution), MEDIA extraction (extract_media/extract_local_files with fenced/
  inline/blockquote/JSON masking, char-accurate offsets), the config->env policy
  bridge, and computer_use path repair. Docker container-path translation is
  behind a `SandboxLayout` seam (configured volumes + cwd bind work now; the
  session-scoped sandbox roots plug in with the terminal subsystem).
- **Hooks** (`hooks.py` -> hooks.rs). Design decision made: a compiled binary
  can't import user Python in-process, so hooks execute as SUBPROCESSES
  (executable / interpreter model) with event on argv + JSON context on stdin +
  JSON stdout as the return. HOOK.yaml discovery, `command:*` wildcard routing,
  and emit / emit_collect are preserved.
- **Status / lifecycle core** (`status.py` -> status.rs). gateway_state.json
  writer/reader (StatusUpdate), the pure derivations (normalize_updated_at,
  parse_active_agents, derive_gateway_busy/_drainable, staleness + no-kill PID
  liveness with a /proc start-time reuse guard), PID file, the exclusive runtime
  flock, and the respawn-storm breaker. session_db_recovery's health aggregate
  is wired into it via a startup sink.
- `message_timestamps.py` -> message_timestamps.rs (chrono; parse/strip/render).
- **Lifecycle / shutdown telemetry**: `lifecycle_ledger.py` -> lifecycle_ledger.rs
  (unclean-death detection via the sentinel, sample_memory, the loop-heartbeat
  writer, state.db quick_check — the write side of what memory_status reads;
  wired live into the singleton path: record_startup on boot, 30s heartbeat,
  mark_exited on shutdown). `restart.py` -> restart.rs (supervisor detection,
  container-restart routing, drain/cron/signal timeout budgets, systemd
  TimeoutStopSec sizing). `shutdown_forensics.py` -> shutdown_forensics.rs
  (SIGTERM/SIGINT /proc snapshot, detached ps/pstree/dmesg diagnostic, systemd
  timing-alignment check).
- **Data-loss flush/recover**: `shutdown_flush.py` -> shutdown_flush.rs
  (flush_pending_to_file / recover_pending_to_db / flush_agent_history_to_file;
  wired live: recover_pending_to_db runs at singleton startup and is verified to
  reinsert a prior life's flushed messages). SessionDb gained
  `append_message_with` (tool fields + display_kind/display_metadata + explicit
  timestamp) + `get_message`, the shape delivery/TUI rows and cron deliveries
  need. `wake.py` delegation-persistence half -> wake.rs
  (persist_delegation_delivery + delegation_display_metadata over SessionDb).
- `memory_monitor.py` -> memory_monitor.rs (periodic [MEMORY] RSS logging on a
  tokio task, getrusage-based; wired into startup, always on).
- `mirror.py` -> mirror.rs (delivery-mirror into a session transcript; +
  SessionDb::find_session_by_origin unambiguous-match + set_thread_id).
- `scale_to_zero.py` DECISION layer -> scale_to_zero.rs (idle predicate, arming
  precondition, idle-timeout parse, relay-only check, dashboard-client liveness
  marker, self_suspend_available). The Fly Machines suspend POST is deploy I/O.
- `channel_directory.py` READ half -> channel_directory.rs (load the cached
  directory + friendly-name alias overlay, resolve_channel_name, lookup type,
  format_directory_for_display). The adapter-driven BUILD half lands with the
  adapter subsystem.

## Hub cores (bounded slices of the big files)

- `session.py` IDENTITY core -> session.rs: SessionSource (+ to_dict/from_dict
  with the scope_id/guild_id migration + description), build_session_key (the
  single source of truth: DM/group/thread isolation, Slack scope prefix,
  WhatsApp canonicalization, Discord prospective-thread continuity, profile
  namespacing), is_shared_multi_user_session, id hashing, path/key traversal
  guards, sanitize_model_override. The 3k-line SessionStore transcript layer
  (persistence, prompt building, auto-continue) overlaps session_db.rs and lands
  with the agent-core turn path.

## Feature / coupled-module cores (ported in parallel, verified + integrated)

- `authz_mixin.py` primitives -> authz.rs (allowlist parsing, gate-env reads,
  bech32 Nostr npub->hex, verified against a NIP-19 vector). Full
  _is_user_authorized decision needs the adapter registry / pairing store.
- `delivery.py` primitives -> delivery.rs (DeliveryTarget parse/render, silence
  filter, telegram private-chat heuristic). Router is adapter-coupled.
- `stream_consumer.py` display helpers -> stream_consumer.rs (code-fence escape /
  close, StreamConsumerConfig). The async sink is adapter-coupled.
- `browser_control_artifacts.py` -> browser_control_artifacts.rs (one-shot
  artifact store: SHA-256 provenance, MIME/size caps, traversal guard, atomic
  write, TTL + orphan sweep, rate limiter).
- `hosted_room_execution_policy.py` -> hosted_room_execution_policy.rs
  (RoomExecutionPolicy + strict parser + canonical-JSON/sha256 digest, golden-
  tested against real Python; config-resolution half deferred to the runner).
- `hosted_room_peer.py` -> hosted_room_peer.rs (GatewayRoomCatalog + strict
  parser/catalog digest, validate_room_link_url with by-hand urlsplit + IPv4/IPv6
  loopback classification, HostedMemberDispatch, select_room_link, and the grant
  issue/verify/decode machinery with hand-rolled HMAC-SHA256 and byte-exact
  canonical JSON; golden-tested against real Python). Deferred: the filesystem
  grant-secret minting (gateway_room_grant_secret) and the config/env-driven
  catalog/endpoint builders (catalog_mapping, local_catalog_mapping,
  local_room_link_endpoint), which couple to hermes_constants, gateway.config,
  and the deferred front half of execution_policy_mapping.
- `pairing.py` core -> pairing.rs (pairing store: salted-hash codes, rate limit,
  lockout, expiry, atomic 0600 writes, split-dir migration; codes/salts/ids use
  the kernel CSPRNG, fail closed). Operator-allowlist mirror (hermes_cli.config
  + live adapters) deferred to the runner.

## Hosted-room cluster (storage/logic tier COMPLETE; ported in parallel)

Bottom-up: `hosted_room_execution_policy.rs` (RoomExecutionPolicy + digest) ->
`hosted_room_peer.rs` (catalog + grants + validate_room_link_url) ->
`hosted_rooms.rs` (link-store) + `hosted_rooms_log.rs` (the 7-table room-log
authority layer: create_room/append_event/read_events with idempotent ingest +
gap + epoch-regression refusal) -> `hosted_room_links.rs` (StoredRoomLink
management), `hosted_room_replicas.rs` (replica store + promote/demote fencing),
`hosted_room_policy_checkpoint.rs` (bounded policy projection). All golden/vector
tested where crypto or canonical JSON is involved. Remaining in this cluster:
`hosted_room_discussion.py` (1461) + `hosted_room_driver.py` (1778) + the rest of
`hosted_rooms.py` orchestration — these are GatewayRunner/agent-core coupled and
land with the runner. `local_authority_gateway_id` fails closed until the
install-id module (hermes_cli.install_identity) is ported.

## Live HTTP surface

- `GET /healthz`, `GET /readyz` (readiness probes), `GET /status` — the last
  assembles the persisted runtime record (status.rs) + live disk_status +
  memory_status + readiness, so all the ported telemetry is observable over
  HTTP. `POST /message`, `GET /display/:platform`, `GET /search`.

## Deferred (port later, with the subsystem they belong to)

- `turn_context.py` — a Python closure-extraction artifact (~60 opaque handles,
  `[None]` single-element cells simulating shared closure state). Its Rust shape
  will be nothing like this; port it with the TurnRunner/_run_agent_inner loop.
- `session_state.py` legacy dict-view adapters — backward-compat shim for
  pre-refactor Python tests only; no Rust equivalent needed. (Data model ported.)
- `session_context.py` — Python `contextvars` + `os.environ` fallback emulating
  task-local session scope; its consumers are `os.getenv("HERMES_SESSION_*")`
  calls in Python tool code. The Rust dispatcher already threads the session
  scope explicitly (Message + session key), so like turn_context this belongs
  with the agent-core turn path, not a contextvar emulation.
- `wake.py` — the delegation-persistence half is DONE (wake.rs). The remaining
  push-capable and API-server self-POST wake paths are coupled to the adapter
  base (`MessageEvent`/`handle_message`) and the API-server adapter; port them
  with the adapter / API-server subsystem.
- `shutdown_flush.py` transcript-spool WRITER queue (flush_overflow_to_file /
  spool_dropped_transcript_message / drain_transcript_spool) — tied to run.py's
  transcript-cap-drop path; recovery already handles their files by reason.
- `stream_dispatch.py` — the event router hangs off the adapter render-hooks and
  the `GatewayStreamConsumer` sink, both in the `stream_consumer.py` hub (3.6k
  LOC, unported). Port it with that subsystem, not against stubs.
- `status.py` remainder — the scoped credential-lock protocol (acquire/release
  _scoped_lock, cross-profile --replace takeover markers) and the dashboard-side
  liveness ladder (`resolve_gateway_liveness` + cmdline/profile heuristics). Port
  with the CLI/dashboard surface that consumes them; the gateway's own
  process-singleton + status-writer core is done.
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
