# Gateway Package Analysis & Python-to-Rust Port Map

This document provides a complete dependency and structural analysis of the 68 Python modules located directly in `gateway/` (excluding subdirectories) to plan a safe, incremental Python-to-Rust migration for `crates/hermes-gateway`.

## Executive Summary

- **Total Python files in `gateway/`**: 68
- **Leaf modules (0 intra-gateway dependencies)**: 38
- **Tier-1 leaf extensions (1 intra-gateway dependency)**: 13
- **Intermediate modules (2-3 intra-gateway dependencies)**: 10
- **Subsystem hubs (4-5 intra-gateway dependencies)**: 5
- **Major coordinator / Root hub (10+ intra-gateway dependencies)**: 2 (`slash_commands.py`, `run.py`)

## Ranked Port Order List

Modules are ranked from safest leaf nodes (0 intra-gateway dependencies and minimal external dependencies) to deep architectural hubs. Early tiers can be ported in parallel as self-contained Rust structs/traits before tackling coordinator hubs.

| Rank | Module | LOC | intra-deps count | external-deps count | why |
| :--- | :--- | :--- | :--- | :--- | :--- |
| 1 | `startup_watchdog.py` | 42 | 0 | 1 | Pure leaf node: 0 intra-deps, minimal external deps (1), self-contained. |
| 2 | `cwd_placeholder.py` | 49 | 0 | 1 | Pure leaf node: 0 intra-deps, minimal external deps (1), self-contained. |
| 3 | `display_config.py` | 322 | 0 | 2 | Pure leaf node: 0 intra-deps, minimal external deps (2), self-contained. |
| 4 | `code_skew.py` | 64 | 0 | 3 | Pure leaf node: 0 intra-deps, minimal external deps (3), self-contained. |
| 5 | `response_filters.py` | 147 | 0 | 3 | Pure leaf node: 0 intra-deps, minimal external deps (3), self-contained. |
| 6 | `turn_context.py` | 150 | 0 | 3 | Pure leaf node: 0 intra-deps, minimal external deps (3), self-contained. |
| 7 | `stream_events.py` | 171 | 0 | 3 | Pure leaf node: 0 intra-deps, minimal external deps (3), self-contained. |
| 8 | `slash_access.py` | 229 | 0 | 3 | Pure leaf node: 0 intra-deps, minimal external deps (3), self-contained. |
| 9 | `session_stall.py` | 125 | 0 | 4 | Leaf node: 0 intra-deps, moderate external deps (4), safe early port. |
| 10 | `message_timestamps.py` | 166 | 0 | 4 | Leaf node: 0 intra-deps, moderate external deps (4), safe early port. |
| 11 | `runtime_footer.py` | 187 | 0 | 4 | Leaf node: 0 intra-deps, moderate external deps (4), safe early port. |
| 12 | `restart.py` | 278 | 0 | 4 | Leaf node: 0 intra-deps, moderate external deps (4), safe early port. |
| 13 | `turn_lease.py` | 355 | 0 | 4 | Leaf node: 0 intra-deps, moderate external deps (4), safe early port. |
| 14 | `session_state.py` | 475 | 0 | 4 | Leaf node: 0 intra-deps, moderate external deps (4), safe early port. |
| 15 | `media_policy.py` | 88 | 0 | 5 | Leaf node: 0 intra-deps, moderate external deps (5), safe early port. |
| 16 | `cgroup_cleanup.py` | 81 | 0 | 6 | Leaf node: 0 intra-deps, moderate external deps (6), safe early port. |
| 17 | `rich_sent_store.py` | 83 | 0 | 6 | Leaf node: 0 intra-deps, moderate external deps (6), safe early port. |
| 18 | `disk_status.py` | 117 | 0 | 6 | Leaf node: 0 intra-deps, moderate external deps (6), safe early port. |
| 19 | `sticker_cache.py` | 124 | 0 | 6 | Leaf node: 0 intra-deps, moderate external deps (6), safe early port. |
| 20 | `systemd_notify.py` | 176 | 0 | 6 | Leaf node: 0 intra-deps, moderate external deps (6), safe early port. |
| 21 | `whatsapp_identity.py` | 206 | 0 | 6 | Leaf node: 0 intra-deps, moderate external deps (6), safe early port. |
| 22 | `restart_loop_guard.py` | 214 | 0 | 6 | Leaf node: 0 intra-deps, moderate external deps (6), safe early port. |
| 23 | `mirror.py` | 227 | 0 | 6 | Leaf node: 0 intra-deps, moderate external deps (6), safe early port. |
| 24 | `hooks.py` | 229 | 0 | 6 | Leaf node: 0 intra-deps, moderate external deps (6), safe early port. |
| 25 | `session_context.py` | 525 | 0 | 6 | Leaf node: 0 intra-deps, moderate external deps (6), safe early port. |
| 26 | `status_phrases.py` | 227 | 0 | 7 | Leaf node: 0 intra-deps, moderate external deps (7), safe early port. |
| 27 | `platform_registry.py` | 699 | 0 | 7 | Leaf node: 0 intra-deps, moderate external deps (7), safe early port. |
| 28 | `readiness.py` | 138 | 0 | 8 | Leaf node: 0 intra-deps, moderate external deps (8), safe early port. |
| 29 | `dead_targets.py` | 143 | 0 | 8 | Leaf node: 0 intra-deps, moderate external deps (8), safe early port. |
| 30 | `browser_control_broker.py` | 1067 | 0 | 8 | Leaf node: 0 intra-deps, moderate external deps (8), safe early port. |
| 31 | `scale_to_zero.py` | 314 | 0 | 9 | Leaf node: 0 intra-deps, higher external deps (9) but isolated within gateway. |
| 32 | `drain_control.py` | 370 | 0 | 9 | Leaf node: 0 intra-deps, higher external deps (9) but isolated within gateway. |
| 33 | `memory_monitor.py` | 230 | 0 | 10 | Leaf node: 0 intra-deps, higher external deps (10) but isolated within gateway. |
| 34 | `browser_control_artifacts.py` | 531 | 0 | 11 | Leaf node: 0 intra-deps, higher external deps (11) but isolated within gateway. |
| 35 | `hosted_room_driver.py` | 1778 | 0 | 11 | Leaf node: 0 intra-deps, higher external deps (11) but isolated within gateway. |
| 36 | `shutdown_flush.py` | 530 | 0 | 12 | Leaf node: 0 intra-deps, higher external deps (12) but isolated within gateway. |
| 37 | `hosted_rooms.py` | 2446 | 0 | 12 | Leaf node: 0 intra-deps, higher external deps (12) but isolated within gateway. |
| 38 | `status.py` | 2624 | 0 | 25 | Leaf module: 0 intra-deps, high external footprint (25 deps, 2624 LOC). |
| 39 | `stream_dispatch.py` | 132 | 1 | 3 | Tier-1 leaf extension: 1 intra-dep (`stream_events`), 3 external deps. |
| 40 | `media_repair.py` | 213 | 1 | 5 | Tier-1 leaf extension: 1 intra-dep (`platforms.base`), 5 external deps. |
| 41 | `profile_routing.py` | 246 | 1 | 5 | Tier-1 leaf extension: 1 intra-dep (`whatsapp_identity`), 5 external deps. |
| 42 | `wake.py` | 272 | 1 | 5 | Tier-1 leaf extension: 1 intra-dep (`platforms.base`), 5 external deps. |
| 43 | `hosted_room_replicas.py` | 570 | 1 | 6 | Tier-1 leaf extension: 1 intra-dep (`hosted_rooms`), 6 external deps. |
| 44 | `session_db_recovery.py` | 189 | 1 | 7 | Tier-1 leaf extension: 1 intra-dep (`status`), 7 external deps. |
| 45 | `hosted_room_policy_checkpoint.py` | 682 | 1 | 7 | Tier-1 leaf extension: 1 intra-dep (`hosted_rooms`), 7 external deps. |
| 46 | `agent_cache_pressure.py` | 310 | 1 | 8 | Tier-1 leaf extension: 1 intra-dep (`cgroup_cleanup`), 8 external deps. |
| 47 | `streaming_tts_consumer.py` | 423 | 1 | 8 | Tier-1 leaf extension: 1 intra-dep (`platforms.base`), 8 external deps. |
| 48 | `shutdown_forensics.py` | 476 | 1 | 9 | Tier-1 leaf extension: 1 intra-dep (`restart`), 9 external deps. |
| 49 | `hosted_room_execution_policy.py` | 190 | 1 | 10 | Tier-1 leaf extension: 1 intra-dep (`run`), 10 external deps. |
| 50 | `control_socket.py` | 560 | 1 | 13 | Tier-1 leaf extension: 1 intra-dep (`status`), 13 external deps. |
| 51 | `delivery_ledger.py` | 562 | 1 | 13 | Tier-1 leaf extension: 1 intra-dep (`status`), 13 external deps. |
| 52 | `memory_status.py` | 203 | 2 | 5 | Intermediate module: 2 intra-deps, 5 external deps; requires prerequisite leaf modules. |
| 53 | `hosted_room_links.py` | 249 | 2 | 7 | Intermediate module: 2 intra-deps, 7 external deps; requires prerequisite leaf modules. |
| 54 | `hosted_room_discussion.py` | 1461 | 2 | 8 | Intermediate module: 2 intra-deps, 8 external deps; requires prerequisite leaf modules. |
| 55 | `lifecycle_ledger.py` | 384 | 2 | 11 | Intermediate module: 2 intra-deps, 11 external deps; requires prerequisite leaf modules. |
| 56 | `channel_directory.py` | 675 | 2 | 11 | Intermediate module: 2 intra-deps, 11 external deps; requires prerequisite leaf modules. |
| 57 | `hosted_room_peer.py` | 887 | 2 | 17 | Intermediate module: 2 intra-deps, 17 external deps; requires prerequisite leaf modules. |
| 58 | `__init__.py` | 35 | 3 | 0 | Intermediate module: 3 intra-deps, 0 external deps; requires prerequisite leaf modules. |
| 59 | `pairing.py` | 936 | 3 | 14 | Intermediate module: 3 intra-deps, 14 external deps; requires prerequisite leaf modules. |
| 60 | `shutdown_watchdog.py` | 649 | 3 | 15 | Intermediate module: 3 intra-deps, 15 external deps; requires prerequisite leaf modules. |
| 61 | `config.py` | 2981 | 3 | 16 | Intermediate module: 3 intra-deps, 16 external deps; requires prerequisite leaf modules. |
| 62 | `delivery.py` | 657 | 4 | 8 | Core subsystem hub: 4 intra-deps, 8 external deps; integrates multiple gateway subsystems. |
| 63 | `stream_consumer.py` | 3616 | 4 | 13 | Core subsystem hub: 4 intra-deps, 13 external deps; integrates multiple gateway subsystems. |
| 64 | `authz_mixin.py` | 1084 | 5 | 4 | Core subsystem hub: 5 intra-deps, 4 external deps; integrates multiple gateway subsystems. |
| 65 | `kanban_watchers.py` | 1849 | 5 | 16 | Core subsystem hub: 5 intra-deps, 16 external deps; integrates multiple gateway subsystems. |
| 66 | `session.py` | 4574 | 5 | 23 | Core subsystem hub: 5 intra-deps, 23 external deps; integrates multiple gateway subsystems. |
| 67 | `slash_commands.py` | 6576 | 10 | 73 | Central slash command dispatcher; heavy coordination hub with 10 intra-deps and 73 external-deps. |
| 68 | `run.py` | 34847 | 56 | 158 | Root process entrypoint and orchestrator; deepest hub depending on almost all gateway modules and external components. |

## Detailed Module Analysis

Analysis of every `.py` file directly inside `gateway/` with line count, docstring summary, intra-package dependencies, and external dependencies.

### `__init__.py`

- **LOC**: 35
- **Purpose**: Hermes Gateway - Multi-platform messaging integration.
- **Intra-package dependencies** (3): `gateway.config`, `gateway.delivery`, `gateway.session`
- **External dependencies** (0): None

### `agent_cache_pressure.py`

- **LOC**: 310
- **Purpose**: Memory-pressure bounds for the gateway's per-session AIAgent cache.
- **Intra-package dependencies** (1): `gateway.cgroup_cleanup`
- **External dependencies** (8): `__future__`, `dataclasses`, `hermes_cli.mem_trim`, `os`, `pathlib`, `psutil`, `sys`, `typing`

### `authz_mixin.py`

- **LOC**: 1084
- **Purpose**: User-authorization methods for ``GatewayRunner``.
- **Intra-package dependencies** (5): `gateway.config`, `gateway.platform_registry`, `gateway.run`, `gateway.session`, `gateway.whatsapp_identity`
- **External dependencies** (4): `__future__`, `agent.secret_scope`, `os`, `typing`

### `browser_control_artifacts.py`

- **LOC**: 531
- **Purpose**: One-shot artifact transport for browser control (Gateway side).
- **Intra-package dependencies** (0): None
- **External dependencies** (11): `__future__`, `dataclasses`, `hashlib`, `logging`, `os`, `pathlib`, `re`, `secrets`, `threading`, `time`, `typing`

### `browser_control_broker.py`

- **LOC**: 1067
- **Purpose**: Transport-neutral browser-control broker core.
- **Intra-package dependencies** (0): None
- **External dependencies** (8): `__future__`, `dataclasses`, `hermes_cli.config`, `logging`, `secrets`, `threading`, `time`, `typing`

### `cgroup_cleanup.py`

- **LOC**: 81
- **Purpose**: SIGKILL any process left in this systemd unit's cgroup.
- **Intra-package dependencies** (0): None
- **External dependencies** (6): `__future__`, `os`, `pathlib`, `re`, `signal`, `sys`

### `channel_directory.py`

- **LOC**: 675
- **Purpose**: Channel directory -- cached map of reachable channels/contacts per platform.
- **Intra-package dependencies** (2): `gateway.config`, `gateway.platform_registry`
- **External dependencies** (11): `asyncio`, `datetime`, `discord`, `hermes_cli.config`, `hermes_state`, `json`, `logging`, `pathlib`, `time`, `typing`, `utils`

### `code_skew.py`

- **LOC**: 64
- **Purpose**: Detect when the gateway is running stale code after a hot ``git pull``.
- **Intra-package dependencies** (0): None
- **External dependencies** (3): `__future__`, `hermes_cli.main`, `pathlib`

### `config.py`

- **LOC**: 2981
- **Purpose**: Gateway configuration management.
- **Intra-package dependencies** (3): `gateway.platform_registry`, `gateway.profile_routing`, `gateway.shutdown_watchdog`
- **External dependencies** (16): `agent.secret_scope`, `dataclasses`, `enum`, `hermes_cli`, `hermes_cli.auth`, `hermes_cli.config`, `hermes_cli.plugins`, `hermes_cli.profiles`, `json`, `logging`, `math`, `os`, `pathlib`, `typing`, `utils`, `yaml`

### `control_socket.py`

- **LOC**: 560
- **Purpose**: Gateway control socket — the gateway-owned local coordination surface.
- **Intra-package dependencies** (1): `gateway.status`
- **External dependencies** (13): `__future__`, `asyncio`, `contextlib`, `hashlib`, `json`, `logging`, `os`, `pathlib`, `socket`, `sys`, `tempfile`, `time`, `typing`

### `cwd_placeholder.py`

- **LOC**: 49
- **Purpose**: Resolve gateway ``terminal.cwd`` placeholder values to ``TERMINAL_CWD``.
- **Intra-package dependencies** (0): None
- **External dependencies** (1): `__future__`

### `dead_targets.py`

- **LOC**: 143
- **Purpose**: Persistent registry of delivery targets that are confirmed unreachable.
- **Intra-package dependencies** (0): None
- **External dependencies** (8): `__future__`, `hermes_cli.config`, `json`, `logging`, `pathlib`, `threading`, `time`, `typing`

### `delivery.py`

- **LOC**: 657
- **Purpose**: Delivery routing for cron job outputs and agent responses.
- **Intra-package dependencies** (4): `gateway.config`, `gateway.dead_targets`, `gateway.platforms.base`, `gateway.session`
- **External dependencies** (8): `dataclasses`, `datetime`, `hermes_cli.config`, `logging`, `os`, `pathlib`, `re`, `typing`

### `delivery_ledger.py`

- **LOC**: 562
- **Purpose**: Durable delivery-obligation ledger for gateway final responses.
- **Intra-package dependencies** (1): `gateway.status`
- **External dependencies** (13): `__future__`, `contextlib`, `hashlib`, `hermes_cli.config`, `hermes_constants`, `hermes_state`, `json`, `logging`, `os`, `sqlite3`, `threading`, `time`, `typing`

### `disk_status.py`

- **LOC**: 117
- **Purpose**: Disk-usage rollup for ``/api/status`` (NS-656).
- **Intra-package dependencies** (0): None
- **External dependencies** (6): `__future__`, `hermes_constants`, `logging`, `pathlib`, `shutil`, `typing`

### `display_config.py`

- **LOC**: 322
- **Purpose**: Per-platform display/verbosity configuration resolver.
- **Intra-package dependencies** (0): None
- **External dependencies** (2): `__future__`, `typing`

### `drain_control.py`

- **LOC**: 370
- **Purpose**: External drain-control marker contract (dashboard → gateway).
- **Intra-package dependencies** (0): None
- **External dependencies** (9): `__future__`, `datetime`, `functools`, `hermes_constants`, `json`, `logging`, `pathlib`, `typing`, `utils`

### `hooks.py`

- **LOC**: 229
- **Purpose**: A lightweight event-driven system that fires handlers at key lifecycle points.
- **Intra-package dependencies** (0): None
- **External dependencies** (6): `asyncio`, `hermes_cli.config`, `importlib.util`, `sys`, `typing`, `yaml`

### `hosted_room_discussion.py`

- **LOC**: 1461
- **Purpose**: Deterministic policy for same-gateway hosted-room Discussions.
- **Intra-package dependencies** (2): `gateway.hosted_room_driver`, `gateway.hosted_rooms`
- **External dependencies** (8): `__future__`, `collections.abc`, `dataclasses`, `hashlib`, `json`, `re`, `tools.bot_failure_reasons`, `typing`

### `hosted_room_driver.py`

- **LOC**: 1778
- **Purpose**: Durable execution state for a same-gateway hosted room driver.
- **Intra-package dependencies** (0): None
- **External dependencies** (11): `__future__`, `contextlib`, `dataclasses`, `hashlib`, `hermes_state`, `json`, `math`, `pathlib`, `re`, `sqlite3`, `typing`

### `hosted_room_execution_policy.py`

- **LOC**: 190
- **Purpose**: Target-issued execution authority for RoomLink member turns.
- **Intra-package dependencies** (1): `gateway.run`
- **External dependencies** (10): `__future__`, `contextvars`, `dataclasses`, `hashlib`, `hermes_cli.config`, `hermes_cli.tools_config`, `json`, `re`, `tools.approval`, `typing`

### `hosted_room_links.py`

- **LOC**: 249
- **Purpose**: Private SQLite storage for negotiated hosted-room links.
- **Intra-package dependencies** (2): `gateway.hosted_room_peer`, `gateway.hosted_rooms`
- **External dependencies** (7): `__future__`, `dataclasses`, `json`, `os`, `pathlib`, `time`, `typing`

### `hosted_room_peer.py`

- **LOC**: 887
- **Purpose**: Typed contracts for autonomous cross-gateway hosted-room members.
- **Intra-package dependencies** (2): `gateway.config`, `gateway.hosted_room_execution_policy`
- **External dependencies** (17): `__future__`, `base64`, `dataclasses`, `functools`, `hashlib`, `hermes_constants`, `hmac`, `ipaddress`, `json`, `math`, `os`, `pathlib`, `re`, `stat`, `time`, `typing`, `urllib.parse`

### `hosted_room_policy_checkpoint.py`

- **LOC**: 682
- **Purpose**: Durable bounded policy projection for hosted Group Chat preparation.
- **Intra-package dependencies** (1): `gateway.hosted_rooms`
- **External dependencies** (7): `__future__`, `dataclasses`, `hermes_state`, `json`, `pathlib`, `sqlite3`, `typing`

### `hosted_room_replicas.py`

- **LOC**: 570
- **Purpose**: Replica store and takeover primitives for hosted Group Chat rooms.
- **Intra-package dependencies** (1): `gateway.hosted_rooms`
- **External dependencies** (6): `__future__`, `json`, `pathlib`, `sqlite3`, `time`, `typing`

### `hosted_rooms.py`

- **LOC**: 2446
- **Purpose**: Durable state for gateway-hosted Bot Mode rooms.
- **Intra-package dependencies** (0): None
- **External dependencies** (12): `__future__`, `contextlib`, `hashlib`, `hermes_cli.install_identity`, `hermes_constants`, `hermes_state`, `json`, `pathlib`, `re`, `sqlite3`, `time`, `typing`

### `kanban_watchers.py`

- **LOC**: 1849
- **Purpose**: Kanban board watcher methods for GatewayRunner.
- **Intra-package dependencies** (5): `gateway.config`, `gateway.platforms.base`, `gateway.session`, `gateway.status`, `gateway.wake`
- **External dependencies** (16): `__future__`, `agent.estop`, `agent.i18n`, `agent.redact`, `asyncio`, `contextvars`, `hermes_cli`, `hermes_cli.config`, `logging`, `os`, `pathlib`, `re`, `sqlite3`, `time`, `typing`, `urllib.parse`

### `lifecycle_ledger.py`

- **LOC**: 384
- **Purpose**: Gateway lifecycle ledger — durable termination-reason evidence (NS-608).
- **Intra-package dependencies** (2): `gateway.shutdown_watchdog`, `gateway.status`
- **External dependencies** (11): `__future__`, `datetime`, `hermes_constants`, `json`, `logging`, `os`, `pathlib`, `sqlite3`, `time`, `typing`, `utils`

### `media_policy.py`

- **LOC**: 88
- **Purpose**: Shared config→env bridge for media-delivery policy.
- **Intra-package dependencies** (0): None
- **External dependencies** (5): `__future__`, `hermes_cli.config`, `logging`, `os`, `typing`

### `media_repair.py`

- **LOC**: 213
- **Purpose**: Repair model-mangled ``computer_use`` screenshot paths in final responses.
- **Intra-package dependencies** (1): `gateway.platforms.base`
- **External dependencies** (5): `__future__`, `json`, `logging`, `re`, `typing`

### `memory_monitor.py`

- **LOC**: 230
- **Purpose**: Periodic process memory usage logging for the gateway.
- **Intra-package dependencies** (0): None
- **External dependencies** (10): `__future__`, `gc`, `logging`, `os`, `psutil`, `resource`, `sys`, `threading`, `time`, `typing`

### `memory_status.py`

- **LOC**: 203
- **Purpose**: Memory status rollup for ``/api/status`` (NS-656).
- **Intra-package dependencies** (2): `gateway.lifecycle_ledger`, `gateway.shutdown_watchdog`
- **External dependencies** (5): `__future__`, `datetime`, `logging`, `pathlib`, `typing`

### `message_timestamps.py`

- **LOC**: 166
- **Purpose**: Helpers for rendering gateway message timestamps exactly once.
- **Intra-package dependencies** (0): None
- **External dependencies** (4): `__future__`, `datetime`, `re`, `typing`

### `mirror.py`

- **LOC**: 227
- **Purpose**: Session mirroring for cross-platform message delivery.
- **Intra-package dependencies** (0): None
- **External dependencies** (6): `datetime`, `hermes_cli.config`, `hermes_state`, `json`, `logging`, `typing`

### `pairing.py`

- **LOC**: 936
- **Purpose**: Code-based approval flow for authorizing new users on messaging platforms.
- **Intra-package dependencies** (3): `gateway.platform_registry`, `gateway.run`, `gateway.whatsapp_identity`
- **External dependencies** (14): `agent.secret_scope`, `hashlib`, `hermes_cli.config`, `hermes_constants`, `json`, `logging`, `os`, `pathlib`, `secrets`, `tempfile`, `threading`, `time`, `typing`, `utils`

### `platform_registry.py`

- **LOC**: 699
- **Purpose**: .
- **Intra-package dependencies** (0): None
- **External dependencies** (7): `dataclasses`, `hermes_constants`, `logging`, `sys`, `threading`, `tools.registry`, `typing`

### `profile_routing.py`

- **LOC**: 246
- **Purpose**: Profile-based routing for the gateway with hierarchical matching.
- **Intra-package dependencies** (1): `gateway.whatsapp_identity`
- **External dependencies** (5): `__future__`, `dataclasses`, `hermes_cli.profiles`, `logging`, `typing`

### `readiness.py`

- **LOC**: 138
- **Purpose**: Bounded, non-destructive readiness probes for authenticated health surfaces.
- **Intra-package dependencies** (0): None
- **External dependencies** (8): `__future__`, `contextlib`, `hermes_constants`, `pathlib`, `shutil`, `sqlite3`, `typing`, `yaml`

### `response_filters.py`

- **LOC**: 147
- **Purpose**: Gateway response filtering helpers.
- **Intra-package dependencies** (0): None
- **External dependencies** (3): `__future__`, `typing`, `unicodedata`

### `restart.py`

- **LOC**: 278
- **Purpose**: Shared gateway restart constants and supervisor detection helpers.
- **Intra-package dependencies** (0): None
- **External dependencies** (4): `collections.abc`, `hermes_cli.config`, `math`, `os`

### `restart_loop_guard.py`

- **LOC**: 214
- **Purpose**: Auto-resume restart-loop breaker (#30719, defense-3).
- **Intra-package dependencies** (0): None
- **External dependencies** (6): `__future__`, `hermes_constants`, `json`, `logging`, `time`, `typing`

### `rich_sent_store.py`

- **LOC**: 83
- **Purpose**: Local index of text we've sent via ``sendRichMessage`` (Bot API 10.1).
- **Intra-package dependencies** (0): None
- **External dependencies** (6): `__future__`, `hermes_constants`, `json`, `os`, `time`, `typing`

### `run.py`

- **LOC**: 34847
- **Purpose**: Gateway runner - entry point for messaging platform integrations.
- **Intra-package dependencies** (56): `gateway.agent_cache_pressure`, `gateway.authz_mixin`, `gateway.channel_directory`, `gateway.code_skew`, `gateway.config`, `gateway.control_socket`, `gateway.cwd_placeholder`, `gateway.delivery`, `gateway.delivery_ledger`, `gateway.display_config`, `gateway.drain_control`, `gateway.hooks`, `gateway.kanban_watchers`, `gateway.lifecycle_ledger`, `gateway.media_policy`, `gateway.media_repair`, `gateway.message_timestamps`, `gateway.pairing`, `gateway.platform_registry`, `gateway.platforms.api_server`, `gateway.platforms.base`, `gateway.platforms.bluebubbles`, `gateway.platforms.msgraph_webhook`, `gateway.platforms.qqbot`, `gateway.platforms.signal`, `gateway.platforms.webhook`, `gateway.platforms.weixin`, `gateway.platforms.whatsapp_cloud`, `gateway.platforms.yuanbao`, `gateway.profile_routing`, `gateway.relay`, `gateway.response_filters`, `gateway.restart`, `gateway.restart_loop_guard`, `gateway.runtime_footer`, `gateway.scale_to_zero`, `gateway.session`, `gateway.session_context`, `gateway.session_db_recovery`, `gateway.session_stall`, `gateway.session_state`, `gateway.shutdown_flush`, `gateway.shutdown_forensics`, `gateway.shutdown_watchdog`, `gateway.slash_access`, `gateway.slash_commands`, `gateway.startup_watchdog`, `gateway.status`, `gateway.status_phrases`, `gateway.stream_consumer`, `gateway.streaming_tts_consumer`, `gateway.systemd_notify`, `gateway.turn_context`, `gateway.turn_lease`, `gateway.wake`, `gateway.whatsapp_identity`
- **External dependencies** (158): `agent`, `agent.async_utils`, `agent.auxiliary_client`, `agent.compaction_display`, `agent.context_references`, `agent.conversation_compression`, `agent.conversation_loop`, `agent.curator`, `agent.display`, `agent.estop`, `agent.i18n`, `agent.image_routing`, `agent.interrupt_compat`, `agent.learn_prompt`, `agent.memory_manager`, `agent.message_sanitization`, `agent.model_metadata`, `agent.monitoring.gateway_health_export`, `agent.onboarding`, `agent.outbound_webhooks`, `agent.plan_prompt`, `agent.prompt_builder`, `agent.redact`, `agent.replay_cleanup`, `agent.secret_scope`, `agent.session_activity`, `agent.shell_hooks`, `agent.skill_bundles`, `agent.skill_commands`, `agent.skill_utils`, `agent.turn_context`, `aiohttp`, `argparse`, `asyncio`, `atexit`, `certifi`, `cli`, `collections`, `concurrent.futures`, `contextlib`, `contextvars`, `cron`, `cron.jobs`, `cron.scheduler`, `cron.scheduler_provider`, `dataclasses`, `datetime`, `difflib`, `dotenv`, `faulthandler`, `functools`, `hashlib`, `hermes_bootstrap`, `hermes_cli`, `hermes_cli._subprocess_compat`, `hermes_cli.active_sessions`, `hermes_cli.auth`, `hermes_cli.blueprint_cmd`, `hermes_cli.commands`, `hermes_cli.config`, `hermes_cli.config_defaults`, `hermes_cli.debug`, `hermes_cli.env_loader`, `hermes_cli.fallback_config`, `hermes_cli.gateway`, `hermes_cli.goals`, `hermes_cli.heartbeat`, `hermes_cli.init_command`, `hermes_cli.lifecycle`, `hermes_cli.loops`, `hermes_cli.mem_trim`, `hermes_cli.moa_config`, `hermes_cli.model_catalog`, `hermes_cli.model_normalize`, `hermes_cli.model_switch`, `hermes_cli.models`, `hermes_cli.nous_auth_keepalive`, `hermes_cli.plugins`, `hermes_cli.process_identity`, `hermes_cli.profiles`, `hermes_cli.proxy_cli`, `hermes_cli.resource_limits`, `hermes_cli.route_identity`, `hermes_cli.runtime_provider`, `hermes_cli.security_advisories`, `hermes_cli.security_audit_startup`, `hermes_cli.stdio`, `hermes_cli.suggestions_cmd`, `hermes_cli.tools_config`, `hermes_constants`, `hermes_logging`, `hermes_state`, `hermes_state_registry`, `hermes_time`, `importlib.util`, `inspect`, `itertools`, `json`, `logging`, `logging.handlers`, `math`, `mimetypes`, `model_tools`, `mutagen.oggopus`, `os`, `pathlib`, `plugins.memory.honcho.client`, `plugins.teams_pipeline.runtime`, `queue`, `re`, `run_agent`, `shlex`, `shutil`, `signal`, `site`, `ssl`, `subprocess`, `sys`, `textwrap`, `threading`, `time`, `tools`, `tools.ansi_strip`, `tools.approval`, `tools.async_delegation`, `tools.bot_mode_dm`, `tools.bot_relay`, `tools.browser_tool`, `tools.checkpoint_manager`, `tools.clarify_gateway`, `tools.credential_files`, `tools.delegate_tool`, `tools.environments.local`, `tools.mcp_tool`, `tools.process_registry`, `tools.registry`, `tools.skill_manager_tool`, `tools.skills_sync`, `tools.skills_sync_client`, `tools.skills_tool`, `tools.terminal_scope`, `tools.terminal_tool`, `tools.tirith_security`, `tools.tool_result_storage`, `tools.transcription_tools`, `tools.tts_tool`, `tools.vision_tools`, `traceback`, `tui_gateway`, `tui_gateway.server`, `types`, `typing`, `urllib.parse`, `utils`, `uuid`, `wave`, `weakref`, `yaml`

### `runtime_footer.py`

- **LOC**: 187
- **Purpose**: Gateway runtime-metadata footer.
- **Intra-package dependencies** (0): None
- **External dependencies** (4): `__future__`, `os`, `tools.terminal_scope`, `typing`

### `scale_to_zero.py`

- **LOC**: 314
- **Purpose**: Scale-to-zero idle detection + dormant-quiesce for the gateway (Phase 0).
- **Intra-package dependencies** (0): None
- **External dependencies** (9): `__future__`, `hermes_constants`, `json`, `logging`, `os`, `pathlib`, `socket`, `time`, `typing`

### `session.py`

- **LOC**: 4574
- **Purpose**: Session management for the gateway.
- **Intra-package dependencies** (5): `gateway.config`, `gateway.platform_registry`, `gateway.session_db_recovery`, `gateway.shutdown_flush`, `gateway.whatsapp_identity`
- **External dependencies** (23): `agent.context_compressor`, `agent.secret_scope`, `agent.turn_context`, `asyncio`, `dataclasses`, `datetime`, `hashlib`, `hermes_cli.config`, `hermes_cli.profiles`, `hermes_cli.tools_config`, `hermes_constants`, `hermes_state`, `json`, `logging`, `os`, `pathlib`, `sqlite3`, `tempfile`, `threading`, `tools.mcp_tool`, `typing`, `utils`, `uuid`

### `session_context.py`

- **LOC**: 525
- **Purpose**: Session-scoped context variables for the Hermes gateway.
- **Intra-package dependencies** (0): None
- **External dependencies** (6): `agent.delegation_context`, `agent.runtime_cwd`, `contextlib`, `contextvars`, `os`, `typing`

### `session_db_recovery.py`

- **LOC**: 189
- **Purpose**: Recoverable per-path SessionDB handle caches for the gateway.
- **Intra-package dependencies** (1): `gateway.status`
- **External dependencies** (7): `__future__`, `dataclasses`, `pathlib`, `threading`, `time`, `typing`, `weakref`

### `session_stall.py`

- **LOC**: 125
- **Purpose**: Gateway session stall notification policy (#72016 item 2).
- **Intra-package dependencies** (0): None
- **External dependencies** (4): `__future__`, `math`, `time`, `typing`

### `session_state.py`

- **LOC**: 475
- **Purpose**: Per-session gateway state consolidated into one container.
- **Intra-package dependencies** (0): None
- **External dependencies** (4): `__future__`, `collections.abc`, `dataclasses`, `typing`

### `shutdown_flush.py`

- **LOC**: 530
- **Purpose**: Flush pending messages and agent transcripts to disk before shutdown to prevent data loss.
- **Intra-package dependencies** (0): None
- **External dependencies** (12): `__future__`, `hermes_constants`, `hermes_state`, `itertools`, `json`, `logging`, `os`, `pathlib`, `time`, `typing`, `utils`, `uuid`

### `shutdown_forensics.py`

- **LOC**: 476
- **Purpose**: Shutdown forensics — capture context when the gateway receives SIGTERM/SIGINT.
- **Intra-package dependencies** (1): `gateway.restart`
- **External dependencies** (9): `__future__`, `json`, `os`, `pathlib`, `signal`, `subprocess`, `sys`, `time`, `typing`

### `shutdown_watchdog.py`

- **LOC**: 649
- **Purpose**: Out-of-loop shutdown and event-loop liveness backstops (#66892, #69089).
- **Intra-package dependencies** (3): `gateway.lifecycle_ledger`, `gateway.restart`, `gateway.status`
- **External dependencies** (15): `__future__`, `asyncio`, `datetime`, `faulthandler`, `hermes_constants`, `hermes_logging`, `json`, `logging`, `os`, `pathlib`, `sys`, `threading`, `time`, `typing`, `utils`

### `slash_access.py`

- **LOC**: 229
- **Purpose**: Per-platform slash command access control.
- **Intra-package dependencies** (0): None
- **External dependencies** (3): `__future__`, `dataclasses`, `typing`

### `slash_commands.py`

- **LOC**: 6576
- **Purpose**: Gateway slash-command handlers for GatewayRunner.
- **Intra-package dependencies** (10): `gateway.code_skew`, `gateway.config`, `gateway.display_config`, `gateway.platform_registry`, `gateway.platforms.base`, `gateway.restart`, `gateway.run`, `gateway.runtime_footer`, `gateway.session`, `gateway.slash_access`
- **External dependencies** (73): `__future__`, `agent.account_usage`, `agent.context_breakdown`, `agent.context_compressor`, `agent.conversation_compression`, `agent.i18n`, `agent.insights`, `agent.manual_compression_feedback`, `agent.model_metadata`, `agent.rate_limit_tracker`, `agent.redact`, `agent.review_engine`, `agent.side_question`, `agent.skill_commands`, `agent.turn_context`, `asyncio`, `cli`, `dataclasses`, `datetime`, `hashlib`, `hermes_cli`, `hermes_cli._subprocess_compat`, `hermes_cli.approval_mode`, `hermes_cli.config`, `hermes_cli.context_switch_guard`, `hermes_cli.debug`, `hermes_cli.goals`, `hermes_cli.heartbeat`, `hermes_cli.kanban`, `hermes_cli.lifecycle`, `hermes_cli.loops`, `hermes_cli.model_selection_guards`, `hermes_cli.model_switch`, `hermes_cli.models`, `hermes_cli.partial_compress`, `hermes_cli.personality`, `hermes_cli.providers`, `hermes_cli.route_identity`, `hermes_cli.session_export`, `hermes_cli.session_export_md`, `hermes_cli.session_listing`, `hermes_cli.slash_exec`, `hermes_cli.tips`, `hermes_cli.write_approval_commands`, `hermes_constants`, `hermes_state`, `inspect`, `json`, `logging`, `os`, `pathlib`, `re`, `run_agent`, `shlex`, `shutil`, `subprocess`, `sys`, `tempfile`, `textwrap`, `time`, `tools`, `tools.approval`, `tools.async_delegation`, `tools.checkpoint_manager`, `tools.credential_files`, `tools.env_passthrough`, `tools.memory_tool`, `tools.process_registry`, `tools.terminal_scope`, `tools.working_diff`, `typing`, `utils`, `uuid`

### `startup_watchdog.py`

- **LOC**: 42
- **Purpose**: Compatibility shim — the real implementation is ``hermes_startup_watchdog``.
- **Intra-package dependencies** (0): None
- **External dependencies** (1): `hermes_startup_watchdog`

### `status.py`

- **LOC**: 2624
- **Purpose**: Gateway runtime status helpers.
- **Intra-package dependencies** (0): None
- **External dependencies** (25): `agent.monitoring.gateway_health`, `copy`, `ctypes`, `dataclasses`, `datetime`, `fcntl`, `hashlib`, `hermes_cli._subprocess_compat`, `hermes_cli.build_info`, `hermes_constants`, `json`, `logging`, `msvcrt`, `os`, `pathlib`, `psutil`, `re`, `shlex`, `signal`, `subprocess`, `sys`, `threading`, `time`, `typing`, `utils`

### `status_phrases.py`

- **LOC**: 227
- **Purpose**: Human-friendly generic gateway status phrases.
- **Intra-package dependencies** (0): None
- **External dependencies** (7): `__future__`, `collections.abc`, `hermes_constants`, `pathlib`, `random`, `typing`, `yaml`

### `sticker_cache.py`

- **LOC**: 124
- **Purpose**: Sticker description cache for Telegram.
- **Intra-package dependencies** (0): None
- **External dependencies** (6): `hermes_cli.config`, `json`, `os`, `tempfile`, `time`, `typing`

### `stream_consumer.py`

- **LOC**: 3616
- **Purpose**: Gateway streaming consumer — bridges sync agent callbacks to async platform delivery.
- **Intra-package dependencies** (4): `gateway.config`, `gateway.platforms.base`, `gateway.platforms.helpers`, `gateway.response_filters`
- **External dependencies** (13): `__future__`, `asyncio`, `concurrent.futures`, `dataclasses`, `inspect`, `logging`, `queue`, `re`, `secrets`, `threading`, `time`, `typing`, `uuid`

### `stream_dispatch.py`

- **LOC**: 132
- **Purpose**: Adapter-driven dispatch of structured stream events to a delivery sink.
- **Intra-package dependencies** (1): `gateway.stream_events`
- **External dependencies** (3): `__future__`, `logging`, `typing`

### `stream_events.py`

- **LOC**: 171
- **Purpose**: Structured streaming events — the agent→gateway delivery contract.
- **Intra-package dependencies** (0): None
- **External dependencies** (3): `__future__`, `dataclasses`, `typing`

### `streaming_tts_consumer.py`

- **LOC**: 423
- **Purpose**: Gateway streaming-TTS consumer — LLM deltas to adapter PCM audio sink.
- **Intra-package dependencies** (1): `gateway.platforms.base`
- **External dependencies** (8): `__future__`, `asyncio`, `logging`, `queue`, `threading`, `tools.tts_streaming`, `tools.tts_tool`, `typing`

### `systemd_notify.py`

- **LOC**: 176
- **Purpose**: Minimal, optional systemd ``sd_notify`` support for the gateway.
- **Intra-package dependencies** (0): None
- **External dependencies** (6): `__future__`, `asyncio`, `math`, `os`, `socket`, `typing`

### `turn_context.py`

- **LOC**: 150
- **Purpose**: Per-turn context shared between ``GatewayRunner._run_agent_inner`` and the ``TurnRunner`` collaborator (gateway/run.py).
- **Intra-package dependencies** (0): None
- **External dependencies** (3): `__future__`, `dataclasses`, `typing`

### `turn_lease.py`

- **LOC**: 355
- **Purpose**: Per-session turn lease — serializes the [load history → run → flush] region.
- **Intra-package dependencies** (0): None
- **External dependencies** (4): `asyncio`, `logging`, `time`, `typing`

### `wake.py`

- **LOC**: 272
- **Purpose**: Wake an existing agent session from a background completion event.
- **Intra-package dependencies** (1): `gateway.platforms.base`
- **External dependencies** (5): `__future__`, `aiohttp`, `asyncio`, `logging`, `typing`

### `whatsapp_identity.py`

- **LOC**: 206
- **Purpose**: Shared helpers for canonicalising WhatsApp sender identity.
- **Intra-package dependencies** (0): None
- **External dependencies** (6): `__future__`, `hermes_constants`, `json`, `logging`, `re`, `typing`

