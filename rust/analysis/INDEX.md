# Rust port evidence index

Read this before resuming, then [PORT.md](../PORT.md) for current progress.

## Settled findings

- The rewrite is gateway-first and incremental. Keep the Python reference tree
  intact and new Rust work under `rust/`.
- Compiling a module is not runtime integration. The newer full config loader
  and session registry still await their runner consumers.
- The existing AGY wrapper serializes invocations after the observed auth race.
  Preserve that gate; helper results still need independent source verification.
- Inbound media classification is pure; the enclosing Tier 2 pipeline also
  performs model/network calls and per-session mutations.
- Resume IDs and titles are routing handles, never authority. Normal callers
  must match route, chat, thread and DM user; see
  [native-resume-resolution.md](native-resume-resolution.md).
- Conversation clients are keyed by profile home and immutable session ID.
  Resume changes the route key target and must not hard-retire the outgoing
  resumable client.
- Manual session titles are metadata only. They use `title_source=user`, never
  enter prompt or transcript bytes, and are serialized by the route then
  transcript lease plus an immediate SQLite transaction.
- Manual native compression defaults to same-session in-place publication and
  keeps rotation behind `compression.in_place: false`. Both paths use a
  redacted, tool-aware checkpoint under the shared `session_turn_leases`
  protocol.
- Automatic native compression runs before inbound persistence on HTTP and
  push ingress. It sizes the frozen provider request, preserves complete head
  and tail regions, supports in-place and rotation publication, and persists
  cooldown and anti-thrash guards.
- Native provider usage is normalized into main and auxiliary ledgers. Native
  tool-call groups are committed incrementally before side effects and later
  provider calls, then replayed from the durable wide transcript.
- Count-based proactive pruning publishes atomically at the next admitted
  pre-turn boundary and after durable tool batches in the active loop, with
  exact-snapshot CAS and durable hysteresis. Full compression now also uses the
  Python token estimator, protected-tail pressure passes, and token-aware
  summary-tail selection. Auxiliary fallback chains and hooks remain open.
- Opt-in native micro-compaction runs after reply persistence and before
  external-memory completion or the next provider request. It preserves all
  user bytes, supersedes only contained rolling markers, bounds failures and
  defrag, and publishes through an exact-snapshot plus lineage-lease SQLite
  transaction.
- Full compression now runs after durable tool-result batches and before the
  next same-turn provider request in both in-place and rotation modes. It uses
  real prompt usage, sentinel/rearm attempt semantics, guarded publication,
  durable transcript adoption, byte-stable system prompts, and a shared
  physical-session identity that follows the committed child.
- Full compression resolves a frozen, isolated `auxiliary.compression` client
  at native startup. Exact non-reasoning routes may carry a configured cap;
  unusable auxiliary output gets one clean main-route retry. Configured
  fallback chains and non-chat-completions transports remain open.
- Structural full-compression no-ops arm a conversation-local, in-memory 300
  second guard shared across pre-turn and same-turn paths. They never strike or
  persist the durable ineffective breaker. Successful boundaries and forced
  manual compression clear the guard.
- Full compression now honors the configured minimum count of real actionable
  user turns and publishes the complete Python-compatible handoff plan in
  manual, pre-turn, and same-turn paths. Dynamic template-visible roles,
  collision merges, old-carrier normalization, zero-user anchors, exact wide
  row cloning, and reference-only call suppression are live.
- Versioned external-memory checkpointing now runs before summary I/O in every
  native full-compression path. Required failures preserve the exact transcript;
  optional failures proceed without context. The durable projection retains the
  derivative-summary marker, and sanitized provider context is fenced as data.
- Every committed native full-compression boundary now rebinds the real Python
  memory manager after publication. Rotation atomically moves the same frozen
  conversation client to the child cache key; in-place notification keeps the
  key. Observer failure never rolls back SQLite.
- The generic `session:compress` event now reaches the selected profile's user
  hooks after the memory callback attempt. The exact five-key Python payload,
  in-place empty parent ID, clone-shared count, async failure isolation, Python
  function ABI, profile-secret boundary, and descendant timeout cleanup are
  live. Context-engine and relay-boundary adoption remain open.
- A session-bound native Unix-local foreground terminal is live only for explicit
  no-approval local configurations with no user deny rules. Its frozen schema,
  profile-isolated persistent environment, routed cwd, bounded redacted output,
  private spills, process-group cleanup, and unconditional security floor are
  covered through the real provider loop. Approval modes, background process
  management, and remote backends remain on the Python path.

## Rejected paths

- Do not use the AGY API-server map as an authoritative enumeration. Prior
  source checks found fabricated names and line numbers. Use the extracted
  route table instead.
- Do not substitute another model for either requested helper silently.
- Do not merge helper code on the strength of its own tests or completion
  report. Python behavioral comparison caught earlier helper test mistakes.
- Do not title-gate every native session picker before a title writer exists.
  `/sessions full` is the discoverable ID path until `/title` and auto-title
  are native.
- Do not notify a standalone Python context engine or relay coordinator merely
  because a native transcript compressed. Neither currently owns the native
  compression, turn, tool, usage, or relay-scope lifecycle needed to make that
  notification truthful.

## Artifacts

| Artifact | Takeaway |
| --- | --- |
| [native-local-terminal-resolution.md](native-local-terminal-resolution.md) | Live local foreground terminal, safe eligibility gate, persistent profile-scoped runtime, routed cwd, bounded output, hardline security, helper disposition, and explicit deferrals |
| [native-terminal-python-contract-agy.md](native-terminal-python-contract-agy.md) | AGY's source map of the Python schema, execution, cwd, timeout, cleanup, output, approval, and backend contracts |
| [native-terminal-rust-seam-claude.md](native-terminal-rust-seam-claude.md) | Claude's independent Rust seam audit covering tool construction, prefix freezing, process primitives, state ownership, and safe first-checkpoint scope |
| [terminal-contract-goldens.json](../tools/terminal-contract-goldens.json) | Source-executed 233-case Python contract for arguments, validation, workdirs, hardline commands, sudo stdin guessing, and approval classification |
| [native-compression-hook-resolution.md](native-compression-hook-resolution.md) | Production `session:compress` dispatch, profile-safe subprocess runtime, Python function runner, exact payload and ordering, review disposition, and explicit context-engine and relay deferrals |
| [compression-event-relay-claude.md](compression-event-relay-claude.md) | Source audit of the generic event payload, fire-and-forget bridge, rollback oddity, absent relay transport, dormant Rust registry, and safe wiring seam |
| [compression-context-engine-boundary-agy.md](compression-context-engine-boundary-agy.md) | Source audit of context-engine boundary arguments, commit and memory ordering, deferred finalization, and full lifecycle requirements |
| [native-compression-hook-python-review-agy.md](native-compression-hook-python-review-agy.md) | Separate Python ABI review of the runner, imports, sync and async handling, error containment, and exact event shape |
| [native-compression-hook-rust-review-claude.md](native-compression-hook-rust-review-claude.md) | Separate Rust review of profile isolation, child lifecycle, post-commit ordering, count ownership, cache stability, and remaining test notes |
| [native-compression-boundary-rebind-resolution.md](native-compression-boundary-rebind-resolution.md) | Post-commit memory-provider rebinding, frozen-client cache transfer, retirement races, live Python-child proof, helper disposition, and explicit notification deferrals |
| [native-pre-compress-checkpoint-resolution.md](native-pre-compress-checkpoint-resolution.md) | Versioned Python host protocol, complete-snapshot ordering, required and optional failure semantics, sanitized summary context, helper disposition, and proof |
| [native-compression-handoff-tail-resolution.md](native-compression-handoff-tail-resolution.md) | Live N-user tails, complete handoff planner, summary rehydration, exact replacement publication, call suppression, helper disposition, and proof |
| [compression-handoff-oracle-claude.md](compression-handoff-oracle-claude.md) | Source-executed 60-case handoff contract, runtime coverage, and parity traps |
| [compression-handoff-goldens.json](../tools/compression-handoff-goldens.json) | Exact constants plus classification, role, carrier, anchor, and call-suppression outputs |
| [native-compression-structural-backoff-resolution.md](native-compression-structural-backoff-resolution.md) | Live conversation-local structural retry guard, manual and successful clearing, durable-breaker separation, validation, and explicit deferrals |
| [compression-structural-backoff-oracle-claude.md](compression-structural-backoff-oracle-claude.md) | Claude's separate 24-case source-executed timer, caller, guard, and clearing oracle |
| [compression-handoff-anchors-agy.md](compression-handoff-anchors-agy.md) | AGY's separate source map for synthetic user rows, multi-user tail anchors, reference handoffs, todo coupling, role alternation, and restart suppression |
| [native-compression-auxiliary-routing-resolution.md](native-compression-auxiliary-routing-resolution.md) | Production auxiliary compression route construction, isolated request policy, bounded main fallback, validation, and explicit failover deferrals |
| [compression-auxiliary-config-oracle-claude.md](compression-auxiliary-config-oracle-claude.md) | Claude's separate 129-case source-executed config, cap, temperature, and fallback oracle |
| [compression-auxiliary-routing-agy.md](compression-auxiliary-routing-agy.md) | AGY's separate runtime map of Python auxiliary client selection, timeout, usage, startup, and layered fallback behavior |
| [native-same-turn-full-compression-resolution.md](native-same-turn-full-compression-resolution.md) | Live post-tool full summary ordering, usage and attempt lifecycle, guarded in-place publication, cache invariants, helper disposition, and explicit deferrals |
| [same-turn-full-compression-oracle-claude.md](same-turn-full-compression-oracle-claude.md) | Claude's independent 25-case source-executed Python decision and adoption oracle |
| [same-turn-full-compression-runtime-agy.md](same-turn-full-compression-runtime-agy.md) | AGY's separate Python runtime map for trigger precedence, failure guards, publication, adoption, and same-turn ordering |
| [native-micro-compaction-resolution.md](native-micro-compaction-resolution.md) | Live post-turn rolling compaction, guarded SQLite publication, helper dispositions, validation, and remaining compression work |
| [micro-compaction-oracle-claude.md](micro-compaction-oracle-claude.md) | Claude's independent 21-case source-executed Python state-machine oracle |
| [micro-compaction-runtime-agy.md](micro-compaction-runtime-agy.md) | Corrected AGY source map of configuration, post-turn ordering, auxiliary calls, state transitions, and transaction invariants |
| [native-token-budget-compression-resolution.md](native-token-budget-compression-resolution.md) | Live token-aware Phase 1 pruning and summary-tail selection, replay-sidecar estimator parity, guarded publication, differential proof, and remaining compression work |
| [token-budget-prune-oracle-claude.md](token-budget-prune-oracle-claude.md) | Claude's independent source-executed oracle covering 17 estimator, 14 prune, and 3 tail-cut cases |
| [native-same-turn-pruning-resolution.md](native-same-turn-pruning-resolution.md) | Live post-tool prune ordering, partial-turn transaction safety, fail-open adoption, cache behavior, validation, and explicit remaining work |
| [same-turn-prune-claude.md](same-turn-prune-claude.md) | Claude's independent runtime trace for post-result persistence, prune adoption, hysteresis, and the next-provider-request boundary |
| [token-budget-prune-agy.md](token-budget-prune-agy.md) | AGY's separate source map of token-tail selection and three-stage pressure demotion for the next pure-function slice |
| [native-provider-usage-pruning-resolution.md](native-provider-usage-pruning-resolution.md) | Source-verified native provider usage, incremental tool history, atomic proactive-prune publication, helper dispositions, and explicit remaining parity |
| [provider-usage-agy.md](provider-usage-agy.md) | AGY's bounded provider-usage lane, including the accepted source map and rejected draft behavior |
| [tool-result-prune-claude.md](tool-result-prune-claude.md) | Claude's independent pure count-based tool-result pruning lane |
| [tool-prune-persistence-claude.md](tool-prune-persistence-claude.md) | Claude's independent wide-row prune publication and durable rearm contract map |
| [native-tool-history-claude.md](native-tool-history-claude.md) | Claude's independent source trace of assistant-call-before-side-effect and result-before-next-request persistence ordering |
| [native-automatic-compression-resolution.md](native-automatic-compression-resolution.md) | Live pre-turn request-pressure compression on HTTP and push, complete prefix/tail publication, durable guards, helper corrections, validation, and explicit remaining parity |
| [automatic-compression-policy-agy.md](automatic-compression-policy-agy.md) | AGY's bounded pure-policy lane plus the source-verified corrections applied before integration |
| [automatic-compression-guards-claude.md](automatic-compression-guards-claude.md) | Claude's separate SQLite guard lane plus the nullable-deadline correction and runtime use |
| [native-in-place-compression-resolution.md](native-in-place-compression-resolution.md) | Implemented Python-default same-session publication, soft-archive recall semantics, byte-exact tail cloning, live counters, rollback, cache retention, and explicit remaining work |
| [automatic-compression-map-agy.md](automatic-compression-map-agy.md) | AGY's bounded source map of automatic thresholds, retry/rearm state, pruning, micro-compaction, and safe Rust insertion points |
| [compression-hooks-model-map-claude.md](compression-hooks-model-map-claude.md) | Claude's separate bounded source map of auxiliary routing, cooldowns, checkpoint hooks, notifications, and in-place policy |
| [native-compression-resolution.md](native-compression-resolution.md) | Implemented manual rotation compression, accepted and rejected review findings, cross-process lease/route fixes, validation, and explicit automatic/in-place/hook deferrals |
| [native-compression-fix-review-agy.md](native-compression-fix-review-agy.md) | Gemini fix re-review that found retained tool-group rejection, ingress timeout, structured shrink accounting, release grace, and one incorrect lock-table premise |
| [native-compression-fix-review-claude.md](native-compression-fix-review-claude.md) | Claude fix re-review that independently found DB-only route healing, refresh teardown logging, and structured shrink accounting gaps |
| [native-compression-review-agy.md](native-compression-review-agy.md) | Gemini first implementation review covering tool fidelity, redaction, title transfer, cache scope, durable exclusion, guard math, failure text, and partial boundaries |
| [native-compression-review-claude.md](native-compression-review-claude.md) | Claude first implementation review covering cross-process exclusion, shrink accounting, error handling, and partial-boundary parity |
| [native-compression-map-agy.md](native-compression-map-agy.md) | Gemini source map of Python manual compression grammar, durability, cache, safety, and concurrency contracts |
| [native-compression-map-claude.md](native-compression-map-claude.md) | Claude source map supporting a real rotation-mode summary checkpoint through the current native model seam |
| [conversation-cache-review-resolution.md](conversation-cache-review-resolution.md) | Verified disposition of both bounded-cache audits, including accepted race and descendant-RSS fixes plus rejected exactly-once and PID-reuse premises |
| [conversation-cache-review-agy.md](conversation-cache-review-agy.md) | Gemini post-implementation audit of cache bounds, finalizer safety, teardown sequencing, reset and shutdown races |
| [conversation-cache-review-claude.md](conversation-cache-review-claude.md) | Claude post-implementation parity audit of soft and hard retirement, expiry, shutdown and remaining command gaps |
| [conversation-cache-implementation-agy.md](conversation-cache-implementation-agy.md) | Gemini source-grounded bounded-cache design, with active-turn, LRU, pressure and awaitable teardown requirements |
| [conversation-lifecycle-wiring-claude.md](conversation-lifecycle-wiring-claude.md) | Claude source map of Python cache triggers, post-persist ownership and the production Rust wiring sites |
| [external-memory-lifecycle-review-resolution.md](external-memory-lifecycle-review-resolution.md) | Verified audit disposition, including tool transcript capture, post-persist sync, clean sidecars and explicit teardown deferrals |
| [external-memory-lifecycle-review-agy.md](external-memory-lifecycle-review-agy.md) | Gemini implementation audit of lifecycle parity, persistence, transcript shape, timeouts and test gaps |
| [external-memory-lifecycle-review-claude.md](external-memory-lifecycle-review-claude.md) | Claude implementation audit identifying tool transcript and durable ordering gaps fixed before commit |
| [external-memory-lifecycle-map-agy.md](external-memory-lifecycle-map-agy.md) | Python external-memory prefetch, sidecar, sync, queue-drain and session-boundary contract map |
| [conversation-eviction-map-claude.md](conversation-eviction-map-claude.md) | Conversation cache ownership, active-turn safety, TTL/LRU/pressure policy and child teardown design for the next checkpoint |
| [explicit-session-rotation-map-claude.md](explicit-session-rotation-map-claude.md) | Independent audit of `/new` `/reset` `/resume` `/compress`: Python contracts/ordering, live Rust primitives, the pre-lease slash-gate race, the smallest `reset_session` seam, and now-vs-deferred split |
| [explicit-session-rotation-map-agy.md](explicit-session-rotation-map-agy.md) | Gemini source audit of command aliases, reset lineage, stale-route races, and the safe now-vs-deferred boundary |
| [explicit-session-rotation-resolution.md](explicit-session-rotation-resolution.md) | Implemented reset/admission design and verified disposition of both helper audits |
| [destructive-slash-confirm-map-claude.md](destructive-slash-confirm-map-claude.md) | Independent audit of Python's destructive slash-confirm primitive (gate default, exactly-once resolve, once/always/cancel replies, config write) and a narrow Rust design for a text-fallback confirm wrapping native `/new` `/reset`, with the config-snapshot and comment-preserving-write risks, security/concurrency review, tests, and the buttons-deferred boundary |
| [destructive-slash-confirm-map-agy.md](destructive-slash-confirm-map-agy.md) | Gemini source audit of route-scoped state, pop-before-action resolution, live config reads, lease timing, access control and deterministic confirmation tests |
| [destructive-slash-confirm-resolution.md](destructive-slash-confirm-resolution.md) | Implemented confirmation design and verified disposition of both helper audits, including parser corrections, durable config writes and explicit adapter-button deferral |
| [native-resume-map-claude.md](native-resume-map-claude.md) | Independent audit of Python `/resume` `/sessions` (soft `switch_session` + reopen, titled-vs-preview list, IDOR fail-closed scope, compression-tip landing, no confirm) and the narrowest native checkpoint: own-route preview list + `/resume <N\|id>` soft switch fenced by route+transcript lease and CAS, needing new `switch_session`/`reopen_session`/per-route-listing seams; no cache retirement thanks to id-keyed cache; titles, admin cross-scope, search/full, buttons deferred |
| [native-resume-map-agy.md](native-resume-map-agy.md) | Retained Gemini source map of the resume authorization, route transition, lease, cache and test invariants used for implementation |
| [native-resume-review-agy.md](native-resume-review-agy.md) | Gemini implementation review that found the initial untitled-session usability blocker, same-channel DM IDOR, reverse preview, and numeric-label defects; all blocking findings were fixed |
| [native-resume-review-claude.md](native-resume-review-claude.md) | Claude implementation review of transaction, lease, CAS and cache identity, plus the initial title-gated usability blocker and compatibility/test gaps; see the resolution for final disposition |
| [native-resume-resolution.md](native-resume-resolution.md) | Implemented design and verified disposition of both helper maps and reviews, including DM identity filtering, full unnamed listing, atomic rollback, warm-client reuse and explicit deferrals |
| [progress-audit-2026-09-08.md](progress-audit-2026-09-08.md) | Current 50.30-point weighted full-port estimate, area scores, live evidence, uncertainty range and largest remaining systems |
| [native-title-map-agy.md](native-title-map-agy.md) | Distilled Gemini source map for gateway title behavior, sanitizer rules, durable metadata and checkpoint boundaries |
| [native-title-map-claude.md](native-title-map-claude.md) | Independent Python/Rust contract map for `/title`, `/new <title>`, title uniqueness, provenance, lazy creation and cache isolation |
| [native-title-review-agy.md](native-title-review-agy.md) | Gemini implementation review and disposition of metadata lookup, transaction-race, index, formatting and sanitizer findings |
| [native-title-review-claude.md](native-title-review-claude.md) | Claude implementation review that found SQLite busy-wait risk and requested stronger cache, lease and invalid-reset coverage |
| [native-title-resolution.md](native-title-resolution.md) | Final title design, applied review fixes, rejected race premise, deliberate persistence-error behavior and deferred work |
| [extension-host-review-resolution.md](extension-host-review-resolution.md) | Verified disposition of both implementation reviews, with fixed findings, tests, and explicit lifecycle deferrals |
| [extension-host-implementation-review-agy.md](extension-host-implementation-review-agy.md) | Gemini post-implementation audit of protocol health, collisions, secrets, multimodal results and process cleanup |
| [extension-host-implementation-review-claude.md](extension-host-implementation-review-claude.md) | Claude post-implementation compatibility review, including auto-loaded backends, route identity and cache-lifetime gaps |
| [extension-host-protocol-agy.md](extension-host-protocol-agy.md) | Gemini protocol and lifecycle design for a persistent JSONL plugin and external-memory host |
| [extension-host-python-audit-claude.md](extension-host-python-audit-claude.md) | Claude source audit of the real Python plugin, toolset, memory-provider and shutdown contracts |
| [native-plugin-memory-manager-map-agy.md](native-plugin-memory-manager-map-agy.md) | Gemini map of dormant native plugin/memory seams; its fixture-only manager recommendation is rejected because it has no production consumer |
| [native-plugin-memory-manager-map-claude.md](native-plugin-memory-manager-map-claude.md) | Claude map confirming prompt order, toolset gates and Python-only providers; its empty-manager checkpoint is likewise not sufficient |
| [frozen-conversation-state-map-agy.md](frozen-conversation-state-map-agy.md) | Gemini source map of Python tool-prefix restoration, plugin snapshots and persistence order |
| [frozen-conversation-state-map-claude.md](frozen-conversation-state-map-claude.md) | Claude source map of the native ownership seam and deferred capability managers |
| [frozen-conversation-state-review-agy.md](frozen-conversation-state-review-agy.md) | Gemini post-implementation audit that identified duplicate and empty-prefix edge cases |
| [frozen-conversation-state-review-claude.md](frozen-conversation-state-review-claude.md) | Claude parity review of the full two-stage Python restore behavior and malformed-state handling |
| [async-conversation-prompt-map-agy.md](async-conversation-prompt-map-agy.md) | Gemini map of the async construction seam, prompt ordering and lock boundary |
| [async-conversation-prompt-map-claude.md](async-conversation-prompt-map-claude.md) | Claude review of single-flight initialization, persistence ordering and retry behavior |
| [progress-audit-2026-09-07.md](progress-audit-2026-09-07.md) | Current 33% weighted full-port estimate, verified runtime boundaries and historical comparison |
| [slack-audio-verification.md](slack-audio-verification.md) | Private audio downloads, video-labeled voice clips, token/URL boundaries and remaining files.info resolution |
| [discord-audio-verification.md](discord-audio-verification.md) | Gateway audio-only messages, ordered CDN downloads, shared bounded cache and remaining SDK fallback gaps |
| [telegram-audio-verification.md](telegram-audio-verification.md) | Telegram download/cache to real HTTP STT and Dispatcher agent-turn proof; remaining adapter scope |
| [progress-audit-2026-09-06.md](progress-audit-2026-09-06.md) | Full-port estimate with phase weights, current runtime evidence and remaining scope |
| [stt-credential-resolution-plan.md](stt-credential-resolution-plan.md) | Strict selection gates, scoped credential dependencies and 100 source resolver cases |
| [auth-store-verification.md](auth-store-verification.md) | Auth-store reads, legacy normalization and per-provider profile/root shadowing; pool selection remains |
| [transcription-http-verification.md](transcription-http-verification.md) | Native multipart STT transport and gateway enrichment integration |
| [tool-text-verification.md](tool-text-verification.md) | Tool-associated assistant text cleanup and remaining fallback scope |
| [tool-name-repair-verification.md](tool-name-repair-verification.md) | Tool-name normalization and fuzzy recovery in native requests |
| [tool-argument-verification.md](tool-argument-verification.md) | Argument rejection before native tool invocation |
| [tool-pairing-verification.md](tool-pairing-verification.md) | Positional tool-result repair, alias-aware deduplication and fresh-batch ID uniqueness |
| [message-repair-verification.md](message-repair-verification.md) | Thinking-only removal and adjacent-user merging before native requests |
| [api-content-verification.md](api-content-verification.md) | Previously sent content restored before native wire projection |
| [iteration-summary-verification.md](iteration-summary-verification.md) | Normal native budget exits, summary retry and tool-free HTTP behavior |
| [turn-limit-verification.md](turn-limit-verification.md) | Turn-limit config authority, Python coercions and native HTTP iteration proof |
| [tool-result-verification.md](tool-result-verification.md) | Native result construction, threat scanner corrections and HTTP replay proof |
| [tool-result-goldens.json](../tools/tool-result-goldens.json) | 62 source-executed result construction and wrapping cases |
| [threat-pattern-goldens.json](../tools/threat-pattern-goldens.json) | 129 scanner comparisons against Python |
| [threat-word-ranges.json](../tools/threat-word-ranges.json) | Generated Python 3.12 word-character ranges for regex compatibility |
| [tool-events-verification.md](tool-events-verification.md) | Native call-event correlation and the remaining tool-result constructor contract |
| [refusal-verification.md](refusal-verification.md) | Refusal-only payload selection, user event delivery and remaining response-normalization scope |
| [refusal-goldens.json](../tools/refusal-goldens.json) | 48 cases executed through the actual Python chat response normalizer |
| [tool-call-replay-verification.md](tool-call-replay-verification.md) | Native tool metadata preservation, model-sensitive projection and reasoning echo startup integration |
| [chat-message-goldens.json](../tools/chat-message-goldens.json) | 30 source-executed Chat Completions projection cases |
| [reasoning-replay-goldens.json](../tools/reasoning-replay-goldens.json) | 62 source-executed host, provider family and reasoning policy cases |
| [prompt-cache-verification.md](prompt-cache-verification.md) | Native cache integration, persisted conversation scopes, override order and dispatcher lease regression |
| [prompt-cache-goldens.json](../tools/prompt-cache-goldens.json) | 65 source-executed cache scope, bounding, static prefix and key projection cases |
| [gateway-map.md](gateway-map.md) | Original gateway dependency map; check against current source |
| [agent-invocation.md](agent-invocation.md) | Python agent boundary used for the strangler bridge |
| [session-db.md](session-db.md) | Session storage mapping |
| [run-py-map.md](run-py-map.md) | Runner tier plan, with verified structure and noted inaccuracies |
| [api-server-map.md](api-server-map.md) | Rejected enumeration, retained with a warning |
| [api-server-routes.md](api-server-routes.md) | Mechanically extracted route table with handler checks |
| [tier2-source-audit.md](tier2-source-audit.md) | Gemini audit of inbound classifiers, side effects, and next seams |
| [inbound-media-review.md](inbound-media-review.md) | Claude follow-up review of the classification port and oracle |
| [inbound-state-verification.md](inbound-state-verification.md) | Context notes, pending merge/STT, native images, parity fixes and proof boundaries |
| [pending-stt-review.md](pending-stt-review.md) | Gemini review; composite and queue-state gaps subsequently addressed |
| [tools/README.md](../tools/README.md) | Helper invocation, requested permissions, and dgnrt reference findings |
| [inbound-media-goldens.json](../tools/inbound-media-goldens.json) | Executed Python outputs for 217 cases |
| [media-context-goldens.json](../tools/media-context-goldens.json) | Placeholder and document/audio/video text outputs |
| [pending-stt-goldens.json](../tools/pending-stt-goldens.json) | 27 source-executed cache/echo/composite transitions |
| [pending-message-goldens.json](../tools/pending-message-goldens.json) | 166 source-executed pending-event merges |
| [cache-path-goldens.json](../tools/cache-path-goldens.json) | 224 cache mappings from real Python imports |
| [inbound-text-goldens.json](../tools/inbound-text-goldens.json) | 144 sender/reply cases and 30 normalization cases |
| [transcription-goldens.json](../tools/transcription-goldens.json) | 38 transcription scenarios with provider call ordering |
| [vision-goldens.json](../tools/vision-goldens.json) | 51 vision scenarios with sanitizer and provider call ordering |
| [image-routing-plan.md](image-routing-plan.md) | Verified next steps and corrections to Gemini's draft APIs |
| [image-routing-verification.md](image-routing-verification.md) | Configuration/session routing and real-filesystem reference extraction evidence |
| [image-routing-goldens.json](../tools/image-routing-goldens.json) | 392 configuration and routing cases |
| [session-image-routing-goldens.json](../tools/session-image-routing-goldens.json) | 28 session-aware wrapper cases |
| [image-reference-goldens.json](../tools/image-reference-goldens.json) | 46 reference extraction cases on real files |
| [native-image-verification.md](native-image-verification.md) | Real image I/O, read guard, MIME inference, corrections and remaining decoder gaps |
| [native-image-goldens.json](../tools/native-image-goldens.json) | Byte signatures and real-file/Pillow output comparisons |
| [file-read-safety-goldens.json](../tools/file-read-safety-goldens.json) | 69 POSIX read-guard cases |
| [mime-defaults.json](../tools/mime-defaults.json) | CPython MIME defaults consumed by the runtime resolver |
| [mime-goldens.json](../tools/mime-goldens.json) | 78 path and overlay inference cases |
| [structured-content-verification.md](structured-content-verification.md) | Prepared image parts through real HTTP, native streaming/tool requests, SQLite replay, and unsupported-backend rejection |
| [content-storage-goldens.json](../tools/content-storage-goldens.json) | 14 Python storage codec cases, compared as decoded values |
| [live-capability-plan.md](live-capability-plan.md) | Verified endpoint resolution, remaining HTTP/cache dependencies, and corrections to the helper audit |
| [inference-endpoint-goldens.json](../tools/inference-endpoint-goldens.json) | 1,164 Python URL/key resolution cases using explicit runtime values |
| [local-probe-verification.md](local-probe-verification.md) | Real HTTP server detection, Ollama show, cache tests, integration fixes and remaining limits |
| [local-probe-goldens.json](../tools/local-probe-goldens.json) | 42 Python request/response and normalization cases |
| [endpoint-locality-verification.md](endpoint-locality-verification.md) | Locality comparisons, zero-request remote gate, and URL/key-to-Ollama HTTP integration |
| [endpoint-locality-goldens.json](../tools/endpoint-locality-goldens.json) | 253 CPython 3.12.13 locality cases including Unicode and address boundaries |
| [provider-prefix-plan.md](provider-prefix-plan.md) | Registry/discovery contract and rejected manifest-name substitutions |
| [managed-capability-verification.md](managed-capability-verification.md) | Staged models, PID ownership, live props, catalog/projector fallback and remaining lifecycle work |
| [managed-capability-goldens.json](../tools/managed-capability-goldens.json) | 50 source-derived staging and capability cases |

| [managed-catalog-verification.md](managed-catalog-verification.md) | Packaged curated catalog loading, refresh, HTTP tests, and remaining compatibility limits |
| [managed-catalog-goldens.json](../tools/managed-catalog-goldens.json) | 43 source-executed catalog constructor cases |

| [cloud-catalog-verification.md](cloud-catalog-verification.md) | Cloud registry cache, real HTTP and concurrency evidence, remaining metadata integration |
| [cloud-catalog-goldens.json](../tools/cloud-catalog-goldens.json) | 60 Python cache state transitions with disk, ETag, and network trace comparisons |

| [cloud-metadata-verification.md](cloud-metadata-verification.md) | Capability/context lookup, overrides, HTTP integration, and canonical JSON ordering checks |
| [cloud-metadata-goldens.json](../tools/cloud-metadata-goldens.json) | 683 Python capability and context comparisons, including insertion-order ties |

| [provider-registry-verification.md](provider-registry-verification.md) | Registration identity, prefix recognition, and full live vision lookup stage ordering |
| [provider-registry-goldens.json](../tools/provider-registry-goldens.json) | Six registration transitions and 265 Python prefix comparisons |

| [provider-fetch-verification.md](provider-fetch-verification.md) | Native model-list hook, credential-safe redirect evidence, and remaining TLS/discovery work |
| [provider-fetch-goldens.json](../tools/provider-fetch-goldens.json) | 49 Python model-list selection/body cases and 12 hostnames |

| [provider-tls-verification.md](provider-tls-verification.md) | CA bundle precedence, trust replacement, public fetch and real HTTPS checks |

| [bundled-base-profiles-verification.md](bundled-base-profiles-verification.md) | Native bundled definitions, startup request headers, credential rotation, transport fallback and remaining discovery work |
| [bundled-base-profiles.json](../tools/bundled-base-profiles.json) | 17 complete profiles generated from 13 base-only Python modules |

| [upstage-verification.md](upstage-verification.md) | Native Upstage request hook, reasoning config and real startup HTTP evidence |
| [upstage-goldens.json](../tools/upstage-goldens.json) | 138 hook, 208 clamping and 215 config-resolution source cases |

| [nebius-verification.md](nebius-verification.md) | Native Nebius profile and request hook, source comparisons and startup HTTP evidence |
| [nebius-goldens.json](../tools/nebius-goldens.json) | 812 Python model-gate and reasoning projection cases |

| [output-cap-verification.md](output-cap-verification.md) | Gateway cap resolution, token parameter selection and profile defaults on native requests |
| [output-cap-goldens.json](../tools/output-cap-goldens.json) | 132 parameter and 364 gateway/init resolution comparisons |

| [request-merge-verification.md](request-merge-verification.md) | Vercel, custom-provider settings and final HTTP JSON merge evidence |
| [request-merge-goldens.json](../tools/request-merge-goldens.json) | 20 actual Hermes/SDK projection cases |
| [vercel-goldens.json](../tools/vercel-goldens.json) | 32 real Vercel hook cases |
| [custom-request-goldens.json](../tools/custom-request-goldens.json) | 24 custom-provider selector cases from Python |

| [gemini-thinking-verification.md](gemini-thinking-verification.md) | Pre-hook effort normalization and native Gemini output-headroom integration |
| [gemini-thinking-goldens.json](../tools/gemini-thinking-goldens.json) | 64 config/cap cases executed from Python |
| [wire-reasoning-goldens.json](../tools/wire-reasoning-goldens.json) | 51 pre-hook normalization comparisons |

| [session-entry-verification.md](session-entry-verification.md) | Persisted entry codec, 124 Python comparisons and integration limits |
| [extension-host-recovery-resolution.md](extension-host-recovery-resolution.md) | No-replay child recovery, frozen capability checks, session rebinding, teardown policy and proof |
| [extension-host-recovery-review-claude.md](extension-host-recovery-review-claude.md) | Independent recovery correctness and security review; identified tests resolved before publication |
| [native-same-turn-rotation-contract-agy.md](native-same-turn-rotation-contract-agy.md) | Authoritative Python mid-turn rotation ordering, transaction, lease, callback and rollback contract |
| [native-same-turn-rotation-seam-claude.md](native-same-turn-rotation-seam-claude.md) | Independent Rust stale-reference map, reusable primitives, seam alternatives and test plan |
| [native-same-turn-rotation-resolution.md](native-same-turn-rotation-resolution.md) | Shared turn identity, atomic child adoption, lease and cache continuity, failure handling and live proof |
| [native-terminal-python-contract-agy.md](native-terminal-python-contract-agy.md) | Python terminal schema, approval, foreground execution, persistence, timeout, output and process contract |
| [native-terminal-rust-seam-claude.md](native-terminal-rust-seam-claude.md) | Rust terminal construction seam, reusable primitives, routing constraints and registration blockers |

Raw local helper outputs: `port-inbound-media.claude.json` and `.stderr`,
`inbound-media-review.claude.json` and `.stderr`, `tier2-source-audit.agy.log`.
Validation outputs: `takeover-tests.log` and `takeover-clippy.log`.
