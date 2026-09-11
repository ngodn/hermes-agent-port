# Native main-provider liveness resolution

Date: 2026-09-11

## Outcome

Ordinary native chat-completions routes now enforce the Python main-provider
liveness contract instead of relying on an unbounded reqwest call. Each frozen
primary and fallback route owns an immutable timeout policy resolved from the
conversation's `config.yaml` snapshot. Existing environment variables remain
compatibility fallbacks and are never reread in the request loop.

The implementation covers three distinct boundaries:

1. A streaming call that never returns response headers is bounded by the
   resolved model/provider request timeout.
2. A buffered tool call is bounded while connecting, waiting for headers, and
   reading its JSON body by the smaller of the request timeout and buffered
   stale deadline.
3. Once streaming headers exist, one rearmed inactivity deadline owns the SSE
   body. It resets only on complete provider `data:` events. Network bytes,
   partial lines, blank separators, and SSE comments do not make a stalled
   generation look healthy.

## Frozen policy

`main_provider_timeouts::Policy` resolves these cascades once per route:

- Request: per-model `timeout_seconds`, provider
  `request_timeout_seconds`, `HERMES_API_TIMEOUT`, then 1800 seconds.
- Streaming stale: per-model or provider `stale_timeout_seconds`,
  `HERMES_STREAM_STALE_TIMEOUT`, then 180 seconds.
- Streaming socket read: a configured model/provider request timeout wins;
  otherwise `HERMES_STREAM_READ_TIMEOUT` or 120 seconds applies. The default
  read bound rises to the local request timeout or a longer cloud stale floor
  so it cannot preempt the structured watchdog.
- Buffered stale: configured stale timeout,
  `HERMES_API_CALL_STALE_TIMEOUT`, reasoning-model floor, then 90 seconds.
- Local streaming: the implicit 180-second base becomes the configured
  `agent.local_stream_stale_timeout`, or 900 seconds by default. This branch
  intentionally bypasses context and reasoning scaling.
- Local buffered: an implicit plain-model default disables the stale watchdog.
  Explicit values and reasoning floors remain finite.
- Large contexts raise streaming deadlines to 240 or 300 seconds and buffered
  deadlines to 150 or 240 seconds.
- Python's full reasoning-family floor table is matched after stripping an
  aggregator prefix, with delimiter-safe longest-prefix selection.

Positive scalar coercion, boolean behavior, the 365-day safe clamp, retry
counts, and context estimates are frozen by the source-executed Python corpus.
`HERMES_STREAM_RETRIES` and `HERMES_STREAM_STALE_GIVEUP` remain legacy
compatibility inputs because Python has no corresponding public config keys.

## Recovery and replay safety

A pre-visible stale stream may replay because it has emitted no user-visible
text. The inner default is three attempts. With a configured fallback, the
ordinary outer transport policy permits two such batches before advancing, so
the default primary ceiling is six network stalls. Setting
`agent.api_max_retries: 1` advances after one three-attempt batch.

A post-visible stale stream never replays the original request. It becomes the
same `finish_reason="length"` semantic continuation already used for explicit
provider truncation. The visible fragment and exact continuation nudge are
added to the provider history, only the missing suffix is delivered, and the
existing transaction persists the alternating fragment/nudge sequence before
later provider I/O.

Streaming and buffered stale expiries increment one route-local atomic streak.
The streak survives turns and is checked before new network work. The default
ceiling is five, zero disables it, and a successful or observably partial
response resets it. Provider fallback and primary restoration also reset the
failed route's streak so one provider cannot wedge another.

Timeouts remain transport health, not credential health. They do not quarantine
API keys, mutate billing/rate cooldowns, rebuild prompts, alter tool schemas, or
record usage for rejected attempts.

## Evidence

AGY independently source-executed the Python contract into 168 deterministic
cases across 13 sections and ran the focused 68-test Python suite. Rust tests
consume its reasoning-floor and context-estimation outputs directly.

Public local-HTTP tests prove:

- model timeout precedence reaches a no-tools streaming request before headers
  and activates the frozen fallback;
- buffered stale timeout covers a response body that starts and then hangs;
- a pre-visible SSE stall performs the exact configured replay batch and then
  falls back;
- post-visible inactivity continues without replay or duplicated text;
- streaming and buffered stale breakers survive later turns/calls and issue no
  extra request at the ceiling;
- SSE keepalive comments cannot postpone the meaningful-activity deadline.

Claude separately mapped the next recovery seam while this implementation was
in progress, then performed a bounded adversarial review of this checkpoint.
Its final report is linked from the evidence index.

## Deliberate limits

- The 30-second operator heartbeat and reconnect notices are not rendered by
  the current Rust gateway event consumers, which also ignore existing generic
  gateway notices. That presentation lane remains open.
- Python's remaining-run-budget cap for implicit buffered deadlines awaits the
  native evaluation/run-budget owner.
- User interruption after 30 seconds of pre-response silence does not yet bump
  the native stale streak because native interrupt ownership is still open.
- Reqwest uses a fixed 15-second connect ceiling plus the resolved pre-header
  deadline, rather than recreating Python's per-request httpx pool timer. The
  liveness and replay contracts are preserved, while exact pool-phase tuning
  remains part of the broader client-policy port.
- Bedrock, Anthropic Messages, Codex Responses, and OAuth-only routes remain on
  their separate transport checkpoints.

The weighted full-port estimate is **58.35%**, reported as about 58%. Native
agent core moves from 79% to 80%; gateway, tool/RPC, and state/search inputs are
unchanged.
