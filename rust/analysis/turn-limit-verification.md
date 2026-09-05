# Native turn-limit configuration

The native client previously hardcoded eight API iterations. It now receives
`agent.max_turns` at startup and defaults to Python's `sys.maxsize` sentinel.
The constructor also defaults to that sentinel for direct callers.

`turn_limit.rs` follows `hermes_cli/config.py::resolve_turn_limit`: positive
numeric values are truncated to integers, nonpositive values and the documented
unlimited spellings remove the cap, and invalid types or text use the default.
Unicode decimal digits and valid numeric underscores follow the shared Python
coercion helpers. Explicit config values override `HERMES_MAX_ITERATIONS`;
explicit null clears that fallback, matching the gateway bridge's authority.
The Rust resolver does not mutate the process environment.

Verification:

- 88 fixtures execute the actual Python function extracted with AST, with both
  unlimited and finite defaults. Regeneration uses Python 3.12.13.
- Inline authority tests cover absent config, positive config, null, and boolean.
- A real local HTTP server requires nine tool results before returning an answer.
  Native `run_turn` succeeds after ten requests with the default, and a configured
  limit of three stops after three requests. This catches the old eight-call cap.
- Workspace: 1,167 tests passed, one existing Python bridge test ignored.
- Clippy with warnings denied passed.

Remaining scope: live config reload, per-agent iteration budgets and independent child budgets, review
input budgets, and Python's exhaustion response construction are not integrated.
The later [summary port](iteration-summary-verification.md) replaces the original
native error on normal cap exhaustion with a tool-free summary request.
Python has arbitrary-sized positive integers; values beyond Rust's usize counter
are rejected rather than wrapped or rounded. This is a documented native bound,
not exact parity for oversized positive limits. Nonfinite overflow text also
raises an error in the reference. No claim of complete agent-loop parity is made.

The source audit in [turn-budget-audit.md](turn-budget-audit.md) corrects the
earlier shared-budget assumption and distinguishes the dormant grace flag from
the active finalizer summary fallback.
