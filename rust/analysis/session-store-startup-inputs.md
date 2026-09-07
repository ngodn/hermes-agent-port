# SessionStore Startup Inputs and Call-Site Map

Maintainer notes (2026-09-07): this is a helper map, not a completion audit.
HTTP ingress now consumes an optional AppState coordinator, and start_push_path
passes that coordinator and shared transcript leases to dispatchers. Main still
does not instantiate the store. The bare history DB already uses open_shared.
Use the full config_loader pipeline; GatewayConfig::from_dict(user_config) is
not an equivalent substitute for merged gateway.json/config.yaml/env settings.
The proposed freshness precedence must be corrected: gateway/run.py's config
bridge overwrites HERMES_AUTO_CONTINUE_FRESHNESS when the setting is present,
then gateway/session.py parses it with malformed-value fallback. Do not treat
reading processes.json as a verified substitute for the live process registry.
The running profile is inferred from resolved home/root paths, not HERMES_PROFILE
or the sticky active_profile file. This is now available as
profile_name::active_profile_name; freshness is available as
session_reset::configured_freshness_seconds. Both have inline tests checked
against AST execution of the actual Python functions. The startup sketch below
still needs migration and process-input integration before use.

Analysis of startup inputs, path distinctions, and invocation contracts required to enable [`SessionStore`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/session_store.rs#L16) in the Rust gateway runtime.

Audited source references:
- Rust gateway entry point: [`rust/crates/hermes-gateway/src/main.rs`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/main.rs)
- Rust gateway configuration: [`rust/crates/hermes-gateway/src/config_gateway.rs`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/config_gateway.rs) and [`rust/crates/hermes-gateway/src/config_loader.rs`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/config_loader.rs)
- Rust profile routing: [`rust/crates/hermes-gateway/src/profile_routing.rs`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/profile_routing.rs)
- Python gateway reference: [`gateway/run.py`](file:///home/eins0fx/development/hermes-agent-port/gateway/run.py) and [`gateway/session.py`](file:///home/eins0fx/development/hermes-agent-port/gateway/session.py)
- Supporting Rust runtime modules: [`rust/crates/hermes-gateway/src/session_store.rs`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/session_store.rs), [`rust/crates/hermes-gateway/src/session_db_recovery.rs`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/session_db_recovery.rs), [`rust/crates/hermes-gateway/src/session_reset.rs`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/session_reset.rs), [`rust/crates/hermes-gateway/src/config_file.rs`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/config_file.rs), and [`rust/crates/hermes-gateway/src/dispatch.rs`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/dispatch.rs)

---

## 1. Executive Summary & Runtime Gap

In the Python gateway ([`gateway/run.py:7634-7643`](file:///home/eins0fx/development/hermes-agent-port/gateway/run.py#L7634-L7643)), [`SessionStore`](file:///home/eins0fx/development/hermes-agent-port/gateway/session.py#L1261) is the process-wide coordinator for conversation identity, route persistence (`gateway_routing` in SQLite and legacy `sessions.json`), profile-scoped database resolution, idle/daily reset decisions, and recovery from crash markers.

In the Rust gateway, [`SessionStore`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/session_store.rs#L16) has been ported with full SQLite reconciliation, single-flight joins, and stale-route pruning ([`rust/crates/hermes-gateway/src/session_store.rs:427-473`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/session_store.rs#L427-L473)). In addition, [`Dispatcher`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/dispatch.rs#L34) already contains the integration seam [`Dispatcher::with_session_store`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/dispatch.rs#L103-L110) to resolve durable session IDs and profile database handles on incoming turns ([`rust/crates/hermes-gateway/src/dispatch.rs:286-328`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/dispatch.rs#L286-L328)).

However, the production entry point [`rust/crates/hermes-gateway/src/main.rs`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/main.rs):
1. Does **not** instantiate [`SessionStore::open`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/session_store.rs#L427).
2. Opens a bare standalone [`SessionDb`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/session_db.rs#L104) directly against `<hermes_home>/state.db` ([`main.rs:444-451`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/main.rs#L444-L451)).
3. Does **not** call [`Dispatcher::with_session_store`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/dispatch.rs#L103) in [`start_push_path`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/main.rs#L312-L356).
4. Relies on an environment-only [`Config`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/config.rs#L12) ([`main.rs:367`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/main.rs#L367)) rather than the structured [`GatewayConfig`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/config_gateway.rs#L218) pipeline.

---

## 2. Exact Startup Inputs Required

Instantiating [`SessionStore`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/session_store.rs#L16) and attaching it to the turn runtime requires six distinct inputs partitioned between store initialization ([`SessionStore::open`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/session_store.rs#L427)) and dispatch attachment ([`Dispatcher::with_session_store`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/dispatch.rs#L103)):

| Parameter | Type | Required At | Origin / Construction | Subsystem Purpose & Invariant |
| :--- | :--- | :--- | :--- | :--- |
| `config` | [`GatewayConfig`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/config_gateway.rs#L218) | [`SessionStore::open`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/session_store.rs#L427) | [`load_gateway_config()`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/config_loader.rs#L186) or [`GatewayConfig::from_dict`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/config_gateway.rs#L444) | Supplies `sessions_dir`, reset policies, multiplex flag, per-user session keys, and route table. |
| `root` | [`PathBuf`](https://doc.rust-lang.org/std/path/struct.PathBuf.html) | [`SessionStore::open`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/session_store.rs#L429) | [`config_file::hermes_root()`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/config_file.rs#L120) | Top-level profile root directory (`profiles/` parent). Resolves secondary profile paths. |
| `home` | [`PathBuf`](https://doc.rust-lang.org/std/path/struct.PathBuf.html) | [`SessionStore::open`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/session_store.rs#L430) | [`config_file::hermes_home()`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/config_file.rs#L68) | Ambient/launch directory. Pins the database (`<home>/state.db`) owning table `gateway_routing`. |
| `active_profile` | [`String`](https://doc.rust-lang.org/std/string/struct.String.html) | [`SessionStore::open`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/session_store.rs#L431) | Environment (`HERMES_PROFILE`), `<root>/active_profile`, or path relative to `<root>/profiles/` | Identifies primary profile partition for recovery fencing and key generation fallback. |
| `active_processes` | `impl FnMut(&str) -> anyhow::Result<bool>` | [`SessionStore::open`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/session_store.rs#L432) | Process registry query closure (or stub `\|_\| Ok(false)`) | Prevents idle/daily reset when a background task (e.g. `terminal`) is actively running for the session key. |
| `freshness_seconds` | `f64` | [`Dispatcher::with_session_store`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/dispatch.rs#L106) | `HERMES_AUTO_CONTINUE_FRESHNESS` or `user_config["agent"]["gateway_auto_continue_freshness"]` (default `3600.0`) | Gates resumption of interrupted turns (`resume_pending_expired` boundary). |

---

## 3. Four Core Focus Areas

### 3.1 Config Construction

#### The Two Divergent Config Structs in Rust
The Rust gateway checkout currently defines two separate configuration structures:
1. [`Config`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/config.rs#L12) ([`rust/crates/hermes-gateway/src/config.rs:11-49`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/config.rs#L11-L49)):
   A minimal, environment-only bootstrap structure loaded via [`Config::from_env()`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/config.rs#L58-L130). It only contains HTTP network binds, subprocess paths, platform bot tokens, and native LLM keys. It contains **no** fields for session directories, reset policies, multiplexing, or profile routes.
2. [`GatewayConfig`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/config_gateway.rs#L218) ([`rust/crates/hermes-gateway/src/config_gateway.rs:218-247`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/config_gateway.rs#L218-L247)):
   The full, authoritative port of Python's `GatewayConfig` dataclass ([`gateway/config.py`](file:///home/eins0fx/development/hermes-agent-port/gateway/config.py)).

#### Loading Pipeline for `GatewayConfig`
[`SessionStore::open`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/session_store.rs#L427) requires [`GatewayConfig`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/config_gateway.rs#L218). Construction options in Rust:
- **Full Pipeline**: [`config_loader::load_gateway_config()`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/config_loader.rs#L186) or [`config_loader::load_gateway_config_from(&hermes_home())`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/config_loader.rs#L194). This reads `<home>/gateway.json` (base layer), merges `<home>/config.yaml`, applies [`config_env_overrides::apply_env_overrides`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/config_env_overrides.rs), and runs validation.
- **Direct Conversion**: [`GatewayConfig::from_dict(&user_config)`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/config_gateway.rs#L444) if converting from the already-loaded `state.user_config` JSON value ([`main.rs:417`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/main.rs#L417)).

#### Fields Consumed by `SessionStore`
Inside [`SessionStore::open`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/session_store.rs#L433-L462) and [`SessionStore::get_or_create_session`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/session_store.rs#L57-L77):
- `config.sessions_dir`: Defaults to `<hermes_home>/sessions` ([`config_gateway.rs:260`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/config_gateway.rs#L260)). Passed to [`RoutingIndex::new`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/session_routing.rs#L341).
- `config.multiplex_profiles`: Boolean controlling multi-profile tenancy. Passed to [`SessionDatabases::new`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/session_db_recovery.rs#L43) and [`RecoveryRequest`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/session_routing.rs#L122).
- `config.get_reset_policy(...)`: Computes [`SessionResetPolicy`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/config_types.rs#L14) per platform and chat type for startup pruning.
- `config.group_sessions_per_user` & `config.thread_sessions_per_user`: Control session key granularity in [`session::build_session_key`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/session.rs#L145).
- `config.write_sessions_json`: Controls whether to write the legacy mirror file `sessions.json` during [`index.save`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/session_routing.rs#L380).
- `config.profile_routes`: Parsed via [`parse_profile_routes`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/profile_routing.rs#L181); used at inbound message dispatch to match incoming chats to target profiles.

---

### 3.2 Profile Root versus Active Home

The architecture cleanly decouples the gateway's launch directory from secondary profile storage to maintain database isolation.

```
       hermes_root() (e.g. ~/.hermes)
       ├── active_profile ("default" or named)
       ├── gateway.json / config.yaml
       └── profiles/
           ├── work/
           │   └── state.db   <-- Owned by SessionDatabases::for_key("agent:work:...")
           └── personal/
               └── state.db   <-- Owned by SessionDatabases::for_key("agent:personal:...")

       hermes_home() (ambient working home, e.g. ~/.hermes or ~/.hermes/profiles/work)
       └── state.db           <-- Owned by SessionDatabases::routing() (gateway_routing table)
```

#### Definitions & Resolution in `config_file.rs`
1. **Profile Root** (`root`):
   Resolved via [`config_file::hermes_root()`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/config_file.rs#L120-L145) (port of `hermes_constants.get_default_hermes_root`).
   - If `HERMES_HOME` is unset or points under the standard install, returns `native_hermes_home()` (typically `~/.hermes` on POSIX, `%LOCALAPPDATA%\hermes` on Windows).
   - In container/custom deployments where `HERMES_HOME` is `<root>/profiles/<name>`, unwraps to the grandparent `<root>`.
2. **Active Home** (`home`):
   Resolved via [`config_file::hermes_home()`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/config_file.rs#L68-L91) (port of `hermes_constants.get_hermes_home`).
   - Resolves the ambient profile home of the running gateway process (e.g., `~/.hermes/profiles/work` if launched under profile `work`).

#### Store Invariants in `SessionDatabases` ([`session_db_recovery.rs:34-110`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/session_db_recovery.rs#L34-L110))
- **Routing DB Pinning**: [`SessionDatabases::routing()`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/session_db_recovery.rs#L62-L64) connects to `<routing_home>/state.db` (`routing_home` is initialized to `home`). The process-wide `gateway_routing` index rows always remain in the gateway's launch home.
- **Profile DB Resolution**: [`SessionDatabases::for_key(key, ambient_home)`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/session_db_recovery.rs#L53-L60) inspects the session key:
  - If `multiplex_profiles` is false or the key has no profile namespace, it returns `<ambient_home>/state.db`.
  - If `multiplex_profiles` is true and specifies a named profile (e.g., `agent:finance:...`):
    - Default profile resolves to `<root>/state.db`.
    - Named profile resolves to `<root>/profiles/<name>/state.db`.
- **Fail-Closed Contract**: If a secondary profile directory does not exist or has a tombstone marker (`<root>/profiles/.deleted/<name>`), [`home_for_key`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/session_db_recovery.rs#L99-L102) returns `None`. It **refuses to fall back to the root database**, preventing cross-profile history contamination.

#### Active Profile Name Resolution
In Python, [`gateway/run.py:15937-15943`](file:///home/eins0fx/development/hermes-agent-port/gateway/run.py#L15937-L15943) resolves `active_profile` via `hermes_cli.profiles.get_active_profile_name()`.
In Rust, [`SessionStore::open`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/session_store.rs#L431) takes `active_profile: String`. This string is required for:
1. Enforcing single-profile partition fencing during recovery ([`session_routing.rs:748`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/session_routing.rs#L748): `recovered_profile == active_profile`).
2. Defaulting the profile namespace in [`SessionStore::get_or_create_session`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/session_store.rs#L70) when multiplexing is on but the source has no explicit profile.

---

### 3.3 Process Pinning

#### Python Reference Behavior
In [`gateway/run.py:7627-7638`](file:///home/eins0fx/development/hermes-agent-port/gateway/run.py#L7627-L7638):
```python
from tools.process_registry import process_registry
_bg_max_age_hours = getattr(
    self.config.default_reset_policy, "bg_process_max_age_hours", 24
)
_bg_max_age_seconds = (
    _bg_max_age_hours * 3600 if _bg_max_age_hours and _bg_max_age_hours > 0 else None
)
self.session_store = SessionStore(
    self.config.sessions_dir, self.config,
    has_active_processes_fn=lambda key: process_registry.has_active_for_session(
        key, max_active_age=_bg_max_age_seconds,
    ),
)
```
When background commands (e.g. `terminal` tool with `background=true`) run, they are tracked by `process_registry`. If a process is still active and was started within `bg_process_max_age_hours`, session idle/daily resets are suppressed.

#### Rust Implementation & Safe Probe Boundary
[`SessionStore::open`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/session_store.rs#L432) accepts `mut active_processes: impl FnMut(&str) -> anyhow::Result<bool>`.
In [`session_reset.rs:58-86`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/session_reset.rs#L58-L86):
- [`session_reset::active_processes_safe`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/session_reset.rs#L62-L70):
  - If probe closure is absent/None: returns `false` (an unconfigured registry does **not** pin sessions).
  - If probe closure returns `Err(_)`: logs a warning and returns `true` (fails safe; uncertainty preserves active context).
- [`session_reset::process_age_limit`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/session_reset.rs#L90-L104): extracts `bg_process_max_age_hours * 3600.0` from [`SessionResetPolicy`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/config_types.rs#L14).
- [`session_reset::process_pins_session`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/session_reset.rs#L75-L86): verifies `!exited && (now - started_at < limit)`.

#### Current Status in Rust Gateway
Rust does not yet have an in-memory process registry tracking background processes.
In [`rust/crates/hermes-gateway/src/dispatch.rs:307-309`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/dispatch.rs#L307-L309) and tests ([`dispatch.rs:467`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/dispatch.rs#L467)), the probe is passed as a no-op closure: `|_| Ok(false)`.
When bridging to production:
- Minimal startup closure: `|_: &str| Ok(false)` fulfills the API without pinning sessions.
- Process-checkpoint reader: Alternatively, reading `<hermes_home>/processes.json` (persisted by Python's `process_registry`) and testing running PIDs.

---

### 3.4 Auto-Continue Freshness

#### Python Reference Behavior
When a gateway restarts or crashes with active turns, turns can be auto-continued via `resume_pending` markers or incomplete tool calls. However, resuming an arbitrarily old task after a long downtime is hazardous.
- Python defines `auto_continue_freshness_window() -> float` ([`gateway/session.py:48-66`](file:///home/eins0fx/development/hermes-agent-port/gateway/session.py#L48-L66) and [`gateway/run.py:1368-1383`](file:///home/eins0fx/development/hermes-agent-port/gateway/run.py#L1368-L1383)).
- Default window: `3600.0` seconds (1 hour).
- Config override: `config.yaml` `agent.gateway_auto_continue_freshness`, bridged to `HERMES_AUTO_CONTINUE_FRESHNESS` ([`gateway/run.py:2965-2968`](file:///home/eins0fx/development/hermes-agent-port/gateway/run.py#L2965-L2968)).
- A non-positive value (`<= 0.0`) disables the gate (treats turns as always fresh).

#### Rust Evaluation & Call Sites
- **Reset Gate** ([`session_reset.rs:36-54`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/session_reset.rs#L36-L54)):
  In [`existing_reset_reason`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/session_reset.rs#L11), if `freshness_seconds > 0.0` and `resume_pending` is true, it compares the elapsed time against `last_resume_marked_at` (or `updated_at`). If `elapsed > freshness_seconds`, it returns `Some("resume_pending_expired")`, triggering automatic session rotation.
- **Dispatcher Storage & Forwarding** ([`dispatch.rs:45, 103-110, 286-312`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/dispatch.rs#L45)):
  - [`Dispatcher`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/dispatch.rs#L45) holds `session_store: Option<(Arc<SessionStore>, f64)>`.
  - [`Dispatcher::with_session_store`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/dispatch.rs#L103) stores `(store, freshness_seconds)`.
  - On each turn in [`handle_turn`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/dispatch.rs#L286-L313), `store.get_or_create_session(&source, false, true, freshness, ...)` runs inside `tokio::task::spawn_blocking`.

---

## 4. Concise Call-Site Map

To wire [`SessionStore`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/session_store.rs#L16) into the Rust gateway, the call sites across [`main.rs`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/main.rs) and [`dispatch.rs`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/dispatch.rs) map out as follows:

```
[config_loader::load_gateway_config()]  ──> GatewayConfig
[config_file::hermes_root()]            ──> PathBuf (root)
[config_file::hermes_home()]            ──> PathBuf (home)
[resolve_active_profile()]              ──> String (active_profile)
[process_probe_closure]                 ──> FnMut(&str) -> anyhow::Result<bool>
                     │
                     ▼
       SessionStore::open(...)           ──> Arc<SessionStore>
                     │
        ┌────────────┴────────────┐
        ▼                         ▼
   AppState                 start_push_path()
        │                         │
  (for /message & search)         ▼
                            Dispatcher::new()
                                  │
                                  ▼
                            .with_session_store(store, freshness_seconds)
                                  │
                                  ▼
                            Dispatcher::handle_turn()
                            - resolves session_key & session_id
                            - matches profile_routes
                            - selects per-key SessionDb
```

### Detailed Call-Site Tracing

1. **Gateway Configuration Loading** ([`main.rs:417`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/main.rs#L417)):
   ```rust
   // Target: main.rs replacing or augmenting config_file::load_config()
   let gateway_config = crate::config_loader::load_gateway_config();
   // or: GatewayConfig::from_dict(&user_config);
   ```
2. **Path & Profile Resolution** ([`main.rs:444`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/main.rs#L444)):
   ```rust
   let home = crate::config_file::hermes_home();
   let root = crate::config_file::hermes_root();
   let active_profile = std::env::var("HERMES_PROFILE")
       .ok()
       .filter(|s| !s.trim().is_empty())
       .unwrap_or_else(|| "default".to_string());
   ```
3. **Freshness Seconds Extraction**:
   ```rust
   let freshness_seconds = std::env::var("HERMES_AUTO_CONTINUE_FRESHNESS")
       .ok()
       .and_then(|v| v.parse::<f64>().ok())
       .or_else(|| {
           user_config.get("agent")
               .and_then(|a| a.get("gateway_auto_continue_freshness"))
               .and_then(|v| v.as_f64())
       })
       .unwrap_or(3600.0);
   ```
4. **Store Construction** ([`main.rs:444-453`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/main.rs#L444-L453)):
   ```rust
   let session_store = match crate::session_store::SessionStore::open(
       gateway_config.clone(),
       root,
       home.clone(),
       active_profile,
       |_key| Ok(false), // Process probe boundary
   ) {
       Ok(store) => Some(Arc::new(store)),
       Err(err) => {
           tracing::error!(%err, "failed to open session store; running stateless");
           None
       }
   };
   ```
5. **Wiring Store into Push Path Dispatchers** ([`main.rs:312-330`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/main.rs#L312-L330)):
   ```rust
   fn start_push_path(
       platform: hermes_core::Platform,
       adapter: Arc<dyn PlatformAdapter>,
       state: &AppState,
       store: Option<Arc<crate::session_store::SessionStore>>,
       freshness_seconds: f64,
       shutdown: CancellationToken,
   ) {
       let mut dispatcher = Dispatcher::new(
           state.agent.clone(),
           state.user_config.clone(),
           state.session_db.clone(),
       );
       if let Some(store) = store {
           dispatcher = dispatcher.with_session_store(store, freshness_seconds);
       }
       ...
   }
   ```
6. **Inbound Profile Matching in `handle_turn`** ([`dispatch.rs:289-305`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/dispatch.rs#L289-L305)):
   Before creating [`SessionSource`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/session.rs#L18), match [`ProfileRoute`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/profile_routing.rs#L64) from `gateway_config.profile_routes`:
   ```rust
   if gateway_config.multiplex_profiles {
       if let Some(route) = crate::profile_routing::match_profile_route(
           &gateway_config.profile_routes,
           &source.platform,
           msg.workspace_id.as_deref(),
           Some(&msg.channel_id),
           msg.thread_id.as_deref(),
           None,
       ) {
           source.profile = Some(route.profile.clone());
       }
   }
   ```

---

## 5. Classification: Missing Infrastructure vs. Reusable Code

| Component | Status | Location | Notes & Gap Description |
| :--- | :--- | :--- | :--- |
| **`SessionStore` Coordinator** | **Reusable** | [`session_store.rs`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/session_store.rs) | `open`, `get_or_create_session`, `update_session`, persistence snapshots, and single-flight joins are implemented and tested. |
| **`SessionDatabases` Isolation** | **Reusable** | [`session_db_recovery.rs`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/session_db_recovery.rs) | Encapsulates `root` vs `routing_home`, path-based handle caching, backoff retry, and fail-closed checks. |
| **Reset & Probe Predicates** | **Reusable** | [`session_reset.rs`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/session_reset.rs) | Pure functions `existing_reset_reason`, `active_processes_safe`, `process_pins_session`, and `process_age_limit` are verified against Python goldens. |
| **`GatewayConfig` & Loader** | **Reusable** | [`config_gateway.rs`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/config_gateway.rs), [`config_loader.rs`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/config_loader.rs) | `GatewayConfig::from_dict` and `load_gateway_config` parse `config.yaml`, `gateway.json`, and env overrides. |
| **Profile Route Matcher** | **Reusable** | [`profile_routing.rs`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/profile_routing.rs) | Specificity sorting and matching (`platform`, `guild_id`, `chat_id`, `thread_id`, WhatsApp JID aliases) fully ported. |
| **`Dispatcher` Session Hooks** | **Reusable** | [`dispatch.rs:103, 286`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/dispatch.rs#L103) | `with_session_store` and turn resolver in `spawn_blocking` already exist and are unit-tested. |
| **Store Health Sink** | **Reusable** | [`session_db_recovery.rs:207`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/session_db_recovery.rs#L207), [`main.rs:400`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/main.rs#L400) | `set_health_sink` callback writing aggregate health into runtime status block is already present in `main.rs`. |
| **Runtime Process Registry** | **Missing Infrastructure** | *None* in Rust | Rust has no equivalent of `tools.process_registry.process_registry` tracking active background commands. Must use `|_| Ok(false)` or parse `<home>/processes.json`. |
| **Active Profile Detection** | **Missing Infrastructure** | *Partial* (`config_file.rs` lacks helper) | No function like Python's `get_active_profile_name()`. Must read `HERMES_PROFILE` or inspect `<root>/active_profile` file. |
| **Inbound Profile Attribution** | **Missing Infrastructure** | [`dispatch.rs:289`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/dispatch.rs#L289) | `Dispatcher::handle_turn` constructs `SessionSource` with `profile = None` without calling `match_profile_route`. |
| **`main.rs` Wiring** | **Missing Infrastructure** | [`main.rs:444, 516`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/main.rs#L444) | `main()` does not call `load_gateway_config`, does not call `SessionStore::open`, does not pass store to `AppState`, and omits `with_session_store` in `start_push_path`. |
