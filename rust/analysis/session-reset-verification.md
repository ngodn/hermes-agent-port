# Session reset decisions

Ported GatewayConfig.get_reset_policy from gateway/config.py and the decision
portion of SessionStore._should_reset from gateway/session.py. Existing Rust
configuration types are reused. Platform overrides win over chat-type overrides,
then the default policy applies.

session_reset.rs accepts local naive wall-clock timestamps and the store's
background-process liveness result. Active processes short-circuit every policy
check, including invalid fields. Idle expiration uses a strict greater-than
comparison. Daily expiration uses the latest configured local-hour boundary,
with a strict less-than comparison for updated_at. Idle wins when both apply.
Invalid numeric/hour values and Python datetime range overflow return errors;
the future active-session caller must treat those as unavailable evidence, as
Python's caller catches exceptions and returns false.

The generator executes the actual Python predicate with a fixed clock and
supplied policy/process state. 283 cases cover no-reset/idle/daily/both/unknown
modes, exact deadlines, microsecond differences, pre-boundary hours, fractional
and negative idle durations, booleans, invalid strings/hours and year-boundary
overflow. Inline Rust tests compare reasons or error outcomes. A separate inline
config test verifies policy precedence.

Validation: 1,285 workspace tests passed, two ignored. Formatting, fixture
regeneration and diff checks pass. Logs: /tmp/hermes-session-reset-test.log,
/tmp/hermes-session-reset-workspace.log and /tmp/hermes-session-reset-clippy.log.

This is not a complete SessionStore or active-session lookup. Loading entries,
generating source-compatible session keys, background-process age/liveness checks,
reset execution and runtime integration remain pending. The native registry and
SQLite history alone cannot establish Python-equivalent session activity.

## Process-liveness guard, pause checkpoint

Added the safe probe boundary from SessionStore: no callback returns false;
registry errors return true and preserve context. The process-age predicate
matches the refreshed registry selection: same session, not exited, and age
strictly below the optional threshold. GatewayRunner derives that threshold
from the default reset policy, with nonpositive/falsy hours disabling the cap.
This predicate does not kill or refresh processes. Live registry integration
remains unfinished.

100 cases execute ProcessRegistry.has_active_for_session against supplied entries
and verify its refresh hook runs. Another 13 evaluate GatewayRunner's actual age
configuration expression. Inline tests exercise failed probes and reset outcomes.
The first settings test exposed a numeric JSON comparison mismatch (3600 versus
3600.0); the assertion now compares numeric seconds, matching the Rust API's f64
return type. No behavioral workaround was added.

Pause checkpoint: 1,287 workspace tests passed, two ignored; fixture regeneration,
formatting and whitespace checks pass. User requested a rest break. Changes are
uncommitted. Logs: /tmp/hermes-pause-checkpoint-tests.log and
/tmp/hermes-pause-checkpoint-clippy.log. Resume only on the user's request.
