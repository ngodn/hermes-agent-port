# Native main-provider pre-body retry resolution

Date: 2026-09-10

## Result

The ordinary native chat-completions path now retries replay-safe connection
failures and non-success HTTP responses before advancing its frozen provider
fallback chain. The configured `agent.api_max_retries` value reaches each
conversation client with Python-compatible coercion and a minimum of one
attempt.

This checkpoint ends at the successful response-status boundary. Malformed
HTTP 200 bodies, safety refusals, response-body stalls, primary-client rebuild,
user-facing retry notices, response-driven compression, OAuth routes, and
non-chat transports remain separate work.

## Retry and fallback policy

The existing dispatcher still owns cross-provider selection, while its
per-route request method owns same-route retries. This keeps one route cursor
for streaming and tool rounds and avoids a second request engine.

- Connection failures, transient TLS failures, HTTP 408, and overload
  429/503/529 use two failed requests before an eligible fallback. Without a
  fallback they use the full configured budget.
- Generic server errors use the full configured budget before fallback.
- Deterministic 500/502 request-validation failures advance immediately.
- 503/529 empty-provider advisories remain server errors and therefore use the
  full budget, rather than the shorter overload threshold.
- Z.AI Coding GLM-5.2 overloads retain the native adaptive backoff and expand a
  no-fallback ceiling to eight attempts. Detection uses the active credential
  route endpoint, not a stale configured fallback URL.
- Certificate-verification failures fail immediately. They are deterministic,
  unlike transient TLS alerts, and must not burn retries or switch providers.

Each same-route retry reuses the same request value. Every fallback route gets
its normal frozen model and request policy, and its retry counter starts fresh.
Credential-pool auth, billing, and rate recovery remains a separate inner axis:
transport, overload, format, and server failures never quarantine a healthy
API key.

## Replay and lifecycle safety

Retry and fallback happen only before a successful response is returned to the
stream or tool decoder. A real truncated chunked SSE test delivers one visible
delta and then breaks the body. The turn fails with that delta emitted exactly
once, and the fallback receives no request. This pins the no-replay boundary
for partial output and future tool side effects.

Backoff sleeps hold no fallback or credential lock. Cancellation drops the
sleep and request future normally. Fallback stickiness, cooldown restoration,
prompt bytes, tool schemas, and usage attribution keep the lifecycle established
by the preceding fallback checkpoint.

## Helper split and review fixes

AGY independently extracted and executed the Python behavior contract, writing
a 157-case corpus across retry budgets, classification, validation, fallback,
credential, stream, cursor, notice, and retry-state sections. Claude separately
mapped the Rust ownership seam and reviewed the integrated implementation.

Primary verification corrected helper wording about the Python loop and the
two-failure transport threshold. Claude's review then found a reachable pooled
HTTP 408 panic and the 503/529 empty-response threshold mismatch. Both were
reproduced before repair and now have regression coverage. Primary review also
separated deterministic certificate failures and corrected active-route Z.AI
backoff detection.

## Verification

- Full Rust workspace: 1,867 passed, two expected ignores.
- Source-executed Python retry corpus: 157 cases, byte-for-byte regeneration.
- Focused Python retry, fallback, classifier, refusal, stream, and restore
  suite: 236 passed.
- Rust formatting, Ruff, workspace Clippy with warnings denied, Python
  formatting, and diff hygiene are checked before commit.

## Progress

The capability inventory moves native agent core from 75% to 76%. Gateway,
tool/RPC, and state/search estimates are unchanged. With the stable full-port
weights, the calculation is:

`0.35 * 67 + 0.30 * 25 + 0.15 * 76 + 0.20 * 76 = 57.55`

The refreshed estimate is **57.55%, reported as about 58%**, with a judgment
range of 55% to 61%. This is an engineering inventory of production behavior,
not a file, line, commit, or test-count ratio. Only one core point is added
because successful-body validation and recovery, non-chat transports, OAuth,
plugins, external-memory management, and most tool/runtime breadth remain.
