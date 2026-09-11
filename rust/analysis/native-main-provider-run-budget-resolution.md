# Native main-provider run-budget stale scaling

## Outcome

Native tool-enabled chat-completions turns now apply Python's wall-clock run
budget to buffered stale deadlines. The budget is normalized from
`agent.run_budget_seconds` when the frozen conversation client is built, and a
fresh start time is stamped at each admitted turn before restore, memory, or
provider work.

The provider request resolves the remaining budget at dispatch. Default,
reasoning-floor, and context-scaled buffered timeouts are shortened to half the
remaining budget with a 60-second floor, but only when that cap is lower.
Explicit model, provider, and legacy environment stale settings remain
authoritative. Plain local implicit timeouts remain unbounded. Streaming
timeouts, request timeouts, retry counts, backoff, prompt bytes, tool schemas,
and credential routing are unchanged.

## Runtime integration

`main_provider_timeouts.rs` now records whether the buffered stale authority
was explicit independently from whether the ordinary default was implicit.
That distinction is load-bearing: reasoning floors stay finite on local routes,
but still yield to a run-budget cap.

`NativeAgentClient` owns the normalized budget and turn start. The exact start
is copied into every frozen fallback route, so primary and fallback attempts
consume one shared turn budget. The effective buffered deadline is resolved
before request I/O and carried with the response into JSON decoding. This
prevents later elapsed time from changing stale attribution after the body has
already timed out.

The production startup builder reads only the existing `config.yaml` key. No
new environment setting, schema surface, prompt content, transcript row, or
provider request field was added.

## Behavioral proof

The dedicated 32-case source-executed Python corpus covers normalization,
fresh and elapsed clocks, reasoning floors, context tiers, explicit settings,
local infinity, the 60-second floor, negative remaining time, downward-only
capping, and unchanged streaming derivation.

Public paused-time tests exercise `AgentClient::run_turn` against local HTTP:

- an implicit 600-second DeepSeek reasoning floor with a 120-second run budget
  times out at 60 seconds
- an explicit 600-second stale setting is still pending after 100 seconds
- the production builder propagates the configured budget, then falls back
  successfully after the capped primary body stalls

The implementation was developed test-first. The public test initially stayed
pending past 100 seconds because the 600-second floor was not yet capped. It
then passed after budget propagation and request-time deadline resolution were
wired.

## Scope boundary

This checkpoint ports only run-budget-aware buffered stale scaling. Python's
80-percent wrap-up injection and operator-visible wait, retry, fallback, and
failure notices remain separate. Claude independently mapped that presentation
surface in `main-provider-operator-notices-claude.md`; its next safe seam is a
turn-local drop-on-success and flush-on-terminal-failure notice buffer whose
events never enter transcript or prompt bytes.
