# Hermes Rust rewrite

## Current handoff: 2026-09-05, Codex takeover

Read [the analysis index](analysis/INDEX.md) first when resuming. Codex owns
integration and validation, with Claude Opus 4.8 (medium) and Gemini 3.8 Flash
(high) as CLI helpers. Both wrappers use the user's requested permission bypass.
Usage and reference-app findings are in [tools/README.md](tools/README.md).

Commit history confirms two separate levels of completion:

| Capability | State | Evidence |
| --- | --- | --- |
| Runnable gateway, Telegram/Discord/Slack, CLI/native/Python agent backends | Existing runtime paths | `379f8b74e8`, `ea428d0723`, `5db3460dc8`, `8a761a89f4` |
| History and delivery ledger | Wired into Dispatcher | `3afb9d2145`, `232626198d` |
| Full gateway config pipeline | Ported and differential-tested; runner integration remains | `811fcaca55`, config_loader golden corpus |
| Session registry and run generations | Ported and tested; future runner owns consumption | `1a6cfb3546` |
| QQBot onboarding, WhatsApp helpers, shared text helpers | Support modules, not complete platform adapters | `6fc55e3f38`, `b5271716b2` |
| Inbound attachment classification | New Tier 2 slice, compiled and tested; not yet called by Dispatcher | `inbound_media.rs`, 217 Python differential cases plus focused unit tests |
| Media placeholders and document/audio/video notes | Ported; display names and sandbox paths are caller-resolved | `media_context.rs`, 28 placeholder cases, 40 document cases, 4 audio/video input pairs |
| Pending STT and combined transcribe/echo flow | Ported with injectable async operations; real providers remain unwired | `pending_stt.rs`, 27 Python transition steps |
| Pending event merge and caption deduplication | Ported, with STT state bundled alongside the event | `pending_messages.rs`, 166 Python merge cases and executable cache/echo integration tests |
| Native-image buffer consumption | Atomic take, scoped to one session | `session_registry.rs`, simultaneous-consumer and cross-session tests |
| Sandbox cache-path mapping | Ported, including staging creation and legacy layout selection | `cache_paths.rs`, 224 mappings generated with real Python imports |
| Sender and reply context | Ported, preserving prompt placement and shared-session policy | `inbound_text_context.rs`, 144 context cases and 30 metadata-normalization cases |
| Transcription enrichment orchestration | Ported with an explicit provider boundary; live provider implementation remains | `transcription_enrichment.rs`, 38 Python scenarios plus recording-backend tests |
| Vision enrichment and memory-context sanitizer | Ported with an explicit provider boundary; live vision provider remains | `vision_enrichment.rs`, 51 Python scenarios checking output and call order |
| Attachment display names | Ported inside the existing media context module | `media_context.rs`, 14 source-executed cases |
| Image mode and capability overrides | Ported with live capability lookup; runner construction remains | `image_routing.rs`, 392 Python cases including recorded lookup effects |
| Session-aware image routing wrapper | Ported with runtime resolution supplied by the runner | `session_image_routing.rs`, 28 Python cases checking fallback and call order |
| Image references in text | Ported with real filesystem checks | `image_references.rs`, 46 Python cases on temporary files |
| Native image content | Real file reads, guarded loading, base64, and PNG conversion; dispatcher integration and HEIC/AVIF decoding remain | `native_image_content.rs`, 23 byte signatures and 20 real-file/Pillow comparisons |
| File read policy | Ported for the POSIX reference, used by native image loading | `file_read_safety.rs`, 69 Python cases including missing paths and symlinks |
| MIME inference and document fallback | CPython default mappings plus system overlays, wired to native image and document helpers | `mime_types.rs` and `media_context.rs`, 78 MIME and 56 document cases |
| Structured message transport and history | Prepared text/image parts reach the native streaming and tool paths through `/message`, survive SQLite replay, and are rejected by unsupported backends | Inline core, storage, native-agent and HTTP tests; [verification](analysis/structured-content-verification.md) |
| Inference endpoint resolution | Ported inside image_routing.rs with explicit turn context; consumed by live capability lookup | 1,164 Python cases, inline tests, [lookup plan](analysis/live-capability-plan.md) |
| Local server detection and Ollama vision probes | Real HTTP and memory/disk cache implemented; prefix recognition implemented; discovery and runner integration remain | 42 source-derived cases plus inline HTTP/cache tests; [verification](analysis/local-probe-verification.md) |
| Endpoint locality and Ollama fallback | Locality gate connected to URL/key resolution and real probes; managed/catalog stages connected through live lookup | 253 Python cases and inline HTTP integration; [verification](analysis/endpoint-locality-verification.md) |
| Managed local vision capability | Staged files, live state/props, and projector fallback implemented; packaged catalog available, caller supplies shared root | 50 source-derived cases and real HTTP tests; [verification](analysis/managed-capability-verification.md) |
| Managed curated catalog | Embedded shared catalog, constructor coercions, explicit background/forced refresh and failure retention | 43 Python loader cases and real HTTP/filesystem tests; [verification](analysis/managed-catalog-verification.md) |
| Cloud registry cache | Memory/disk/network cache, stale background refresh, ETag and failure backoff implemented; capability/context lookup available | 60 Python cases and inline HTTP/concurrency tests; [verification](analysis/cloud-catalog-verification.md) |
| Cloud capability and context metadata | Provider map, override/default selection, model matching and vision catalog stage implemented | 683 Python cases, HTTP tests and insertion-order collision regression; [verification](analysis/cloud-metadata-verification.md) |
| Provider registration and live vision lookup | Registration/aliases, prefix recognition and combined lookup implemented; discovery and runner construction remain | Six registration transitions, 265 Python prefix cases, full HTTP-stage tests; [verification](analysis/provider-registry-verification.md) |
| Base provider model-list hook | Native endpoint selection, model-list HTTP and credential-safe redirects implemented; per-provider TLS contexts and overrides remain | 49 Python fetch and 12 hostname cases plus real redirect/header tests; [verification](analysis/provider-fetch-verification.md) |
| Provider model-list CA bundles | Environment precedence, full PEM bundles, default fallback and custom trust store implemented | Real local HTTPS and public environment-to-fetch tests; [verification](analysis/provider-tls-verification.md) |
| Bundled base profiles and native startup | 17 profiles across 13 modules loaded natively; selected endpoints, headers and declared credentials wired into startup | Source regeneration, real streaming/tool HTTP requests and saved-key rotation; [verification](analysis/bundled-base-profiles-verification.md) |
| Upstage provider hook and reasoning config | Native Solar profile, reasoning hook, shared clamping and per-model config resolution wired into both request paths | 561 Python comparisons and 12 real HTTP requests; [verification](analysis/upstage-verification.md) |
| Nebius Token Factory | Native profile, model allowlist and request reasoning hook wired into startup | 812 Python cases and 12 real HTTP requests; [verification](analysis/nebius-verification.md) |
| Output caps and profile defaults | Gateway/init cap resolution, wire parameter selection and profile temperature/token defaults reach both native request paths | 496 Python comparisons, eight startup HTTP requests; [verification](analysis/output-cap-verification.md) |
| Request-body projection and Vercel | Split profile maps, caller overrides, shallow SDK projection and legacy custom-provider body selection wired into native requests | 76 source comparisons and eight HTTP requests; [verification](analysis/request-merge-verification.md) |
| Gemini thinking caps and wire reasoning | Shared pre-hook normalization and thinking output headroom wired into both native request paths | 115 Python comparisons and 14 HTTP requests; [verification](analysis/gemini-thinking-verification.md) |
| Native prompt-cache routing | Per-turn persisted scope, static-prefix/tool hashing and explicit key bounding wired before SDK body flattening; dispatcher locks share history identity | 65 Python comparisons, ten HTTP requests and endpoint/profile gate checks; [verification](analysis/prompt-cache-verification.md) |
| Native tool replay and message projection | Valid argument text, thought signatures and reasoning sidecars preserved in flight; outgoing copies enforce model filtering and provider echo policy | 92 Python comparisons, eight tool HTTP requests and twelve startup HTTP requests; [verification](analysis/tool-call-replay-verification.md) |
| Native refusal payloads | Non-streaming refusal-only responses reach the user; usable text and tool calls retain precedence | 48 Python comparisons and a real HTTP-to-event test; [verification](analysis/refusal-verification.md) |
| Native tool events | Decoded arguments, per-turn correlation indexes and measured execution duration emitted through the loop | Inline multi-iteration, failed-call and counter-reset regression; [verification](analysis/tool-events-verification.md) |
| Native tool-result construction | Names, canonical IDs, timestamps, elision notices, untrusted framing and advisory findings wired into result creation; internal fields stripped at wire projection | 191 Python comparisons and three real HTTP tool rounds; [verification](analysis/tool-result-verification.md) |

The classification oracle executes AST-extracted Python predicates and the
actual inbound bucketing loop. This proves the pure transformation contract,
not end-to-end media delivery, model resolution, STT, or vision integration.
The broader runner tier includes network calls and session mutation, despite
the earlier map calling it pure. See [the source audit](analysis/tier2-source-audit.md).

Current validation: 1164 workspace tests passed, one existing Python-bridge
test ignored. Clippy with warnings denied and formatting pass. Rust tests live
inside their implementation files, following the user's layout preference.
See [inbound verification](analysis/inbound-state-verification.md) and
[routing verification](analysis/image-routing-verification.md) and
[native image verification](analysis/native-image-verification.md) and
[structured transport verification](analysis/structured-content-verification.md) for source
comparison results and limitations. The full-port goal remains active;
tested orchestration is not yet the complete live runner.

Next steps, in order:

1. Port the live image capability lookup, runtime model resolution, remaining
   native decoder support (HEIC/AVIF), and @ context expansion.
   Local-server probes, locality, and the Ollama fallback now exist. Next are
   dynamic provider discovery and custom hooks (the 13 base-only bundled modules
   now load natively, along with Upstage and Nebius request hooks), then runner construction around the live
   capability lookup. Managed local capability and cloud registry caching are implemented;
   follow the corrected [dependency plan](analysis/live-capability-plan.md).
2. Connect transcription and vision orchestration to real provider adapters
   behind their explicit I/O boundaries.
3. Integrate the pipeline and pending-message state into the richer runner event path with real
   adapter and model-runtime resolution. The Dispatcher accepts a
   core Message type carrying prepared content parts, but platform adapters
   still need attachment download, enrichment, and session routing integration.

The sections below retain the earlier port inventory; this handoff qualifies
what "ported" means where that inventory does not distinguish runtime wiring.

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
land with the runner. `hermes_cli/install_identity.py` is now ported
(`install_identity.rs`: CSPRNG-minted 32-hex install id, flock publication
fence, atomic write, process cache) and wired into `hosted_rooms_log`, so
`local_authority_gateway_id` resolves a real `install:<id>` and the
promote/demote fencing takes over under it. It still fails closed if the id can
neither be read nor minted.

## Adapter-support + relay leaves (ported in parallel)

Self-contained slices that unblock the adapter and relay subsystems, each
golden-tested against real Python:

- [x] `code_skew.py` -> code_skew.rs (git-revision fingerprint + hot-pull skew
      detection; also ported the `.git` ref parser it borrows from hermes_cli.main)
- [x] platforms/base.py value tier -> platform_base_types.rs (MessageType,
      ProcessingOutcome, MessageEvent + is_command/get_command/get_command_args,
      SendResult, EphemeralReply, and the pure send-error classifiers
      classify_send_error / is_chat_level_not_found). BasePlatformAdapter, the
      media-cache primitives, TextDebounceState, MessageHandler and the
      caption/pending-event merges stay with the adapter/runner tier.
- [x] `relay/auth.py` -> relay_auth.rs (HMAC-SHA256 sign/verify, base64url
      tokens, delivery signatures; RFC 4231 verified)
- [x] `relay/command_manifest.py` -> relay_command_manifest.rs (Discord slash
      palette on the relay hello frame; byte-exact wire shape)
- [x] `relay/descriptor.py` -> relay_descriptor.rs (CapabilityDescriptor
      handshake; byte-exact json.dumps(sort_keys=True, ensure_ascii=False))
- [x] platforms/qqbot/crypto.py -> qqbot_crypto.rs (AES-256-GCM credential
      decrypt via RustCrypto aes-gcm, never hand-rolled; CSPRNG bind key; golden
      vectors vs Python cryptography AESGCM)
- [x] platforms/qqbot/{constants,utils}.py -> qqbot_common.rs
- [x] platforms/qqbot/keyboards.py -> qqbot_keyboards.rs (inline-keyboard wire
      structs, approval/update-prompt parsers + builders, InteractionEvent;
      ApprovalSender's adapter send is deferred to the QQ adapter)
- [x] platforms/signal_format.py -> signal_format.rs (markdown -> Signal
      bodyRanges, UTF-16 offset math + overlap suppression; 18 golden vectors)
- [x] platforms/signal_rate_limit.py -> signal_rate_limit.rs (token-bucket
      attachment scheduler; asyncio.Lock -> std Mutex held only across the brief
      refill/deduct sections, never across an await; process-global singleton)
- [x] `agent/retry_utils.py` -> retry_utils.rs (Retry-After parse incl. RFC 7231
      HTTP-date via chrono rfc2822; jittered/adaptive backoff; zai overload).
      Obsolete RFC 850 / asctime date forms are not accepted (documented gap,
      only affects a future date in those rare forms)
- [x] platforms/_http_client_limits.py -> http_client_limits.rs (adapter
      connection-pool tuning; httpx Limits -> reqwest pool knobs)
- [x] `relay/transport.py` -> relay_transport.rs (RelayTransport async trait;
      reuses MessageEvent + CapabilityDescriptor; pure interface)
- [x] platforms/webhook_filters.py -> webhook_filters.rs (declarative route
      filters + subprocess script transforms; timeout kills+reaps the direct
      child without joining reader threads, matching CPython subprocess.run's
      leak-the-grandchild-fd timeout behavior). Deferred: build_subprocess_env
      secret-scrub and agent.redact output redaction (base behavior reproduced).
- [x] platforms/yuanbao_proto.py -> yuanbao_proto.rs (Yuanbao WebSocket protobuf
      codec, hand-rolled varint + length-delimited ConnMsg framing; 14 golden
      test fns). Deviation: decode_conn_msg/decode_biz_msg degrade to a default
      struct on a hard wire error instead of raising.
- [x] platforms/yuanbao_sticker.py -> yuanbao_sticker.rs (sticker catalogue +
      fuzzy search + FaceMsg wire body). Gap: _normalize_text NFKC is strip+
      lowercase only (no NFKC crate); a no-op for ASCII/CJK, diverges only for
      compatibility-form search queries. Closing it needs unicode-normalization.

## Config + API-server foundation

- [x] gateway/config.py schema tier -> config_schema.rs (Platform enum with all
      24 built-in members, the coercion/normalization helpers, watchdog clamp,
      platform_binds_port). config.rs remains the runnable skeleton.
- [x] gateway/config.py dataclasses -> config_types.rs (HomeChannel,
      SessionResetPolicy, ChannelOverride, PlatformConfig, StreamingConfig; each
      Default + from_dict/to_dict, reusing config_schema).
- [x] GatewayConfig -> config_gateway.rs (fields, defaults, __post_init__,
      to_dict/from_dict, _has_usable_api_server_key).
- [x] _apply_env_overrides -> config_env_overrides.rs. All 158 env vars present,
      verified mechanically by rust/tools/check_env_override_parity.sh. Faithful
      quirks kept (the dead BLUEBUBBLES_REQUIRE_MENTION guard writing false; the
      warn helper bypassing the secret scope). The registry-driven plugin-enable
      pass is a documented no-op (Python wraps it in try/except -> debug).
- [x] load_gateway_config + _validate_gateway_config -> config_loader.rs, which
      composes the pipeline: file layers -> env overrides -> validate. The
      non-uniform precedence is reproduced exactly, including multiplex_profiles
      having no elif (so gateway.json beats gateway.multiplex_profiles).
      Deferred, matching Python's fail-open: managed_scope.apply_managed_overlay
      (identity) and plugin discovery (the _pr = None branch).

THE CONFIG LAYER IS COMPLETE AND DIFFERENTIALLY VERIFIED. rust/tools/
gen_config_goldens.py captures real Python `load_gateway_config().to_dict()` for
18 fixture homes; the `golden_corpus` test in config_loader.rs replays them
through the full Rust pipeline with a cleared process environment, and all 18
match exactly. That covers config_loader + config_env_overrides + config_gateway
+ config_types + config_schema together.
- [x] agent/secret_scope.py -> secret_scope.rs (get_secret fail-closed
      resolution, is_global_env tables, multiplex flag, parse_env_value /
      strip_inline_comment / load_env_file). The Python ContextVar is modeled as
      a tokio task-local with a scope-based with_secret_scope(...) API. Remaining:
      the external get_secret_source_values merge (its loader is unported) and
      wiring the scope into the multiplex turn path when multiplexing is built.
- [x] platforms/api_server_run_idempotency.py -> api_server_run_idempotency.rs
      (the full RunIdempotencyStore: SQLite dedup for POST /v1/runs, Created/
      Reused/Conflict via constant-time fingerprint check, terminal+expired
      prune). The rest of the api_server layer (api_server.py, room_grants,
      room_dispatch, runs) is aiohttp-mixin / runner coupled and lands with the
      API-server subsystem.

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
- `agent/secret_scope.py` — DONE (secret_scope.rs), see the config foundation
  section. The ContextVar became a tokio task-local scope API. session_context
  can follow the same pattern when its turn-path consumers are ported.
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
