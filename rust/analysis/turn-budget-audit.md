# Python turn-budget behavior

Gemini traced the source; the main agent checked the paths below. Its raw report
is local in `audit-turn-budget.agy.log`. These findings guide the next loop port.

- `agent/conversation_loop.py:2289` checks both the per-turn API call counter and
  `iteration_budget.remaining`. The counter increments before the request at
  line 2326; budget consumption follows at line 2335. They are distinct counters.
- `agent/turn_context.py:756` creates a new `IterationBudget(max_iterations)`
  every turn. `tools/delegate_tool.py:2161` passes `iteration_budget=None` to
  children, giving subagents independent budgets. Do not implement a shared
  parent/child budget based on old architectural descriptions.
- `agent/conversation_loop.py:8205` refunds the budget when all calls are
  `execute_code`. It does not refund the raw API counter in this branch.
- `_budget_grace_call` is initialized false and consumed by the loop. A search
  of the current Python tree found no assignment setting it true. The old init
  comment describes an extra grace iteration, but does not prove it happens.
- The active exhaustion behavior is in `agent/turn_finalizer.py:181`: preserve
  a pending verification answer when eligible, otherwise call
  `_handle_max_iterations` for the eligible budget exit. That helper lives in
  `agent/chat_completion_helpers.py:3197` and requests a summary without tools.
  This is separate from the dormant grace flag. Port the full eligibility and
  request construction before changing the native exhaustion response.
- `gateway/run.py:2375` gives config authority over the environment. Cached
  agents receive the refreshed cap at line 6300. Rust now resolves this at
  startup only; runtime refresh remains.

Next work: exhaustion summary and eligibility, mutable per-agent budgets and
refunds, independent child budgets when delegation is integrated, and runtime
config refresh. Relevant Python tests include
`tests/agent/test_turn_finalizer_iteration_limit_exit.py` and
`tests/run_agent/test_iteration_budget_race.py`.
