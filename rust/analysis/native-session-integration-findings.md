# Native conversation initialization findings

Verified from the current worktree on 2026-09-07. This is an integration finding,
not a claim that production prompt initialization is complete.

Both live paths resolve a durable session and selected database before calling
an agent, but retain a shared startup agent:

- dispatch.rs: run_admitted_turn loads history from turn_db, then clones self.agent.
- message.rs: the admitted HTTP task loads history from turn_db, then clones state.agent.
- agent.rs: AgentClient::run_turn receives Message, history and an event sender.
  Message carries resolved_session_id but no selected profile or prompt context.
- main.rs: build_agent_client_for_home captures provider/config for one home.
  Outside its startup wrapper, current calls are tests, not routed production turns.
- native_agent.rs: run_turn clones its client, changes cache_scope to the resolved
  session ID, and prepends its optional immutable system_prompt. It does not load
  or initialize a session prompt. Session identity alone does not select a profile.
- session_db.rs: get_session resolves the stored prompt body, and
  update_system_prompt already persists it. begin_turn only loads history and
  appends the inbound message; it does not restore a stored prompt.
- session_state.rs: AgentSlot is only an enum. SessionRegistry has no live native
  client handle to reuse for prompt initialization.

Consequences for the next implementation:

1. Add a concrete conversation initialization path shared by push and HTTP,
   after session/profile resolution and under the existing conversation lease.
   Carry the selected database/profile and internal resolved ID, never a
   transport-supplied prompt or filesystem path.
2. Reuse build_agent_client_for_home for selected native provider/config, rather
   than setting a conversation prompt on the shared startup client. Keep CLI
   and Python history ownership semantics intact.
3. Restore a usable persisted prompt when appropriate; otherwise assemble all
   required captured sections once and persist through the existing DB API.
   Use the ACID skill before modifying transaction behavior or coordinating writes.
4. Keep the resulting native client/prompt scoped to the conversation, with
   explicit reset/resume/profile-switch handling. Do not assume channel ID alone
   identifies a conversation or that every resume should rebuild its prompt.
5. Cover both ingress paths using real temporary profile databases and a local
   provider server: separate profile configuration, distinct prompts for distinct
   sessions, unchanged bytes across turns/tool rounds, persisted resume, and a
   fresh context after reset. Existing native_agent tests prove fixed-prompt
   request behavior, not this production initialization lifecycle.

Python reference anchors: gateway/run.py's agent-cache reuse path near 6138,
agent/system_prompt.py's once-per-session construction and invalidation near
1039, and gateway/run.py's stored-prompt hygiene seed near 754. The hygiene seed
alone is not evidence for the full normal-turn resume contract. Read the complete
normal reuse/resume path before implementing eviction or restoration policy.
