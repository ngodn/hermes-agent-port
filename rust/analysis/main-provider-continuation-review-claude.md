# Main-Provider Continuation Review (buffered text length continuation, durable continuation history, truncated tool-call same-request retry)

**Reviewer lane**: correctness of the uncommitted Rust implementation against the two agy contracts
**Scope reviewed**: the live diff on `rust-rewrite` for
`crates/hermes-gateway/src/{agent,dispatch,message,native_agent,native_tools,session_db}.rs`
**Contracts used as the oracle**:
- `rust/analysis/main-provider-length-continuation-contract-agy.md`
- `rust/analysis/main-provider-tool-truncation-contract-agy.md`
**Out of scope by instruction**: timeout/stall configuration (covered by `main-provider-stall-config-claude.md`).

## How I read the three features

Three distinct lanes share one `NativeAgentClient` step loop and one gateway persistence seam:

1. **Buffered text length continuation** (tool-enabled path): `native_agent.rs:5406-5494`. A
   `finish_reason == "length"` text `Step::Final` with visible content is accumulated into
   `continuation_parts`, the fragment plus a `MAIN_LENGTH_CONTINUATION_PROMPT` nudge is appended to
   `request_messages` and `continuation_messages`, and the loop re-requests with a boosted cap. On the
   first non-length terminal step it returns `Step::WithContinuation{preceding_messages, visible_prefix,
   next}`, which `run_tool_loop_with_messages` (`native_tools.rs:647-658`) flattens by persisting the
   pairs, extending the live message list, and prepending the joined prefix to the delivered answer.
   The no-tools streaming variant is the sibling loop at `native_agent.rs:4320-4420`.

2. **Exact durable continuation history**: `session_db.rs:3951` (`append_native_continuation_messages`)
   writes the assistant-fragment/user-nudge pairs under an Immediate transaction gated on turn lease,
   session liveness, and tail phase. The final settled answer is written separately by the gateway
   `end_turn` through the new `assistant_reply_for_history` projection (`agent.rs:161`,
   `native_agent.rs:4761`, consumed at `dispatch.rs:779` and `message.rs:494`). The replacement map
   (`durable_reply_replacements`) carries the final suffix so `end_turn` stores only the suffix while the
   fragments live as their own rows.

3. **Truncated tool-call same-request retry**: `native_agent.rs:5409-5426`. A `finish_reason == "length"`
   `Step::ToolCalls` never appends anything and never executes; it re-runs the identical request up to 4
   times with an exponentially boosted one-shot cap, then returns
   `Step::PartialFinal{repair_tool_tail:true}` and marks the reply delivery-only. The ceiling repair
   appends a synthetic terminal assistant row only when the durable tail is a `tool` row
   (`native_tools.rs:685-707`, `session_db.rs:4100-4183`).

## Verdict summary

I found **no high-severity correctness defect** in the three implemented lanes on the chat-completions
success and ceiling paths. The retry ceilings, cap schedule, usage attribution on the happy and ceiling
paths, durable role ordering for the tested shapes, tool-execution safety, and concurrent-turn isolation
all match the contracts as far as the implemented surface goes. The substantive gaps are **deliberate
deferrals** of contract guardrails that this change did not attempt, plus a small number of **low to
medium edge divergences**. Details below, with the safe conclusions stated explicitly.

---

## Findings

### F1. Thinking-only length truncation bypasses the continuation lane entirely (deferral, medium if unintended)

**Where**: tool path `native_agent.rs:5427-5428` (`if let Some(visible) = crate::visible_response::answer(content)`),
no-tools path `native_agent.rs:4359` (`if outcome.visible`).

Both continuation lanes only engage when the length response carries *visible* text. A
`finish_reason == "length"` response whose whole budget went to reasoning scratchpad (empty visible
content) falls through: the tool path drops into the generic empty-response machinery
(`native_agent.rs:5506-5597`) and the no-tools path skips the length block and returns without
continuation state.

The length-continuation contract (sections 4.1, 4.2, 7.2, 8.3) requires three guardrails at this point:
thinking-budget-exhaustion abort with actionable guidance, repetition-dominated abort (#86581), and the
empty-assistant suppression plus `_ephemeral_reasoning_off` retry. None of these are present in the diff.

**Reproduction**: a reasoning model returns `finish_reason:"length"` with `content:""` and a `<think>`
block. Python aborts with the "used all output tokens on reasoning" guidance. Rust instead routes into
empty-response retries and can emit the "(empty)" or the reasoning-preview fallback at
`native_agent.rs:5592-5597`. User-visible delivery differs.

**Assessment**: this is the guardrail family the contract flags, and it is almost certainly a scoped
deferral rather than a regression (no prior Rust code handled it either). Confirm it is deferred; if the
port intends parity it is a real gap. Marked medium only because the user-facing message changes.

### F2. Network stream-stall variant and dropped-tool-names prompt are not implemented (deferral)

**Where**: only `MAIN_LENGTH_CONTINUATION_PROMPT` exists (`native_agent.rs:1003`); the only tool-truncation
terminal string is `"Response truncated due to output length limit"` (`native_agent.rs:5419`,
`native_tools.rs:783`).

The tool-truncation contract section 7 and the length contract sections 3.1/6.1/11.3 require distinguishing
genuine output-cap truncation from `PARTIAL_STREAM_STUB_ID` network stalls: a separate terminal message
(`"Stream repeatedly dropped mid tool-call (network); the tool was not executed"`), a network-stub
continuation prompt, and the zero-byte dropped-tool-args path that diverts into text continuation with a
dropped-tools chunking prompt capped at 3 names. None of this is in the diff.

**Assessment**: deferral. No user-visible defect within the implemented genuine-length lane; the stall
detection that would feed these branches is part of the timeout/stall lane that is explicitly out of scope
here. Flagging so the deferral is on record, not as a defect.

### F3. Ollama GLM `stop`-misreport rewrite is not implemented (deferral)

**Where**: length contract section 3.2. No `_should_treat_stop_as_truncated` equivalent appears; a GLM
`stop` that is actually truncated is treated as a clean terminal answer.

**Assessment**: deferral, provider-specific, low blast radius. Recorded for completeness.

### F4. Memory-extension turn projection omits the final continuation answer (low)

**Where**: no-tools path return `native_agent.rs` end of `run_model_turn_inner`
(`current_turn_messages(request_messages, history.len(), content)`), tool path
`native_agent.rs:4310` plus `TranscriptModel::step` recording at `native_agent.rs:188-196`.

On a continued turn, `request_messages` (no-tools) and `last_messages` (tool path) contain
`[user, assistant(frag1, length), user(nudge), ...]` but never the final non-length fragment, because the
final fragment is never pushed back into the request message list. `current_turn_messages` therefore hands
the external-memory host (`pending_memory_turn`, `native_agent.rs:4710-4724`) a turn that ends on a
continuation nudge with the final answer missing.

**Reproduction**: configure an `_extension_host`, drive a 2-pass length continuation, inspect the
`PendingMemoryTurn.messages`; the final fragment ("part two" in the test shape) is absent.

**Assessment**: low. Only affects memory extraction, not delivery or provider replay, and only when an
extension host is configured. It is still strictly more context than the pre-change path (which always fell
back to `[user]`), so not a regression, just incomplete.

### F5. A transient lease/phase race during continuation becomes a hard turn failure (low)

**Where**: tool path `native_tools.rs:647-651` (`model.persist_continuation_messages(&preceding_messages)?`),
no-tools path `native_agent.rs:4410-4415`, backed by the `Ok(false)` rejections in
`append_native_continuation_messages` (`session_db.rs:3966`, `3992`, `4011`, `4019`).

If the turn lease is lost or the durable tail is not in an acceptable phase at persist time,
`append_native_continuation_messages` returns `Ok(false)`, which both call sites convert into
`Error::Other("... rejected an invalid tail")`. In the tool path this aborts a turn whose provider work
already succeeded, and the user gets an error instead of the assembled answer. Python persists the whole
turn once at finalize, so it does not fail a completed turn on a mid-turn persistence check.

**Assessment**: low. The lease is held for the turn's duration, so the race window is narrow, and failing
closed (error, no half-written pair) is the safe direction. Noting the divergence in failure mode, not a
data-integrity defect.

### F6. Router-rewrite truncation refusal persists the error as a durable assistant turn, unlike the new length path (low, pre-existing asymmetry)

**Where**: pre-existing `native_tools.rs:773-789`. When a proxy masks a cap hit as
`finish_reason:"tool_calls"` with unterminated args, this branch refuses execution and returns
`Err("Response truncated due to output length limit")` **without** calling
`mark_turn_reply_delivery_only`.

Because it is not marked delivery-only and sets no replacement, the gateway seam persists the delivered
error string as a normal assistant row: `dispatch.rs:760-781` runs `end_turn` through
`assistant_reply_for_history` **regardless of `succeeded`**, and `assistant_reply_for_history`
(`native_agent.rs:4761`) returns `Some(reply)` here. So after a first-call router-rewrite truncation with
no prior tool, durable history becomes `[user, assistant("Response truncated due to output length limit")]`,
whereas the new `finish_reason == "length"` path correctly marks delivery-only and leaves durable history
as `[user]` (or appends only the `_interrupted_tool_terminal` repair when a tool tail exists).

**Assessment**: low, and **pre-existing** (this branch is not in the diff, and the prior
`assistant_reply_is_durable` default already persisted it). The new delivery-only plumbing makes the
asymmetry visible; aligning this branch with `mark_turn_reply_delivery_only` would match the tool-truncation
contract section 8.3 ("broken assistant response is never added"). Out of this change's primary scope, but
worth a follow-up.

### F7. Duplication risk if a continuation is followed by a terminal step that is never a `Step::Final` (low, edge)

**Where**: `native_agent.rs:5482-5505`. The durable replacement is set only when the continuation's `next`
is a `Step::Final` (5484-5488) or when a later step is a `Step::Final` while `turn_has_continuation` is set
(5496-5504).

If a text continuation completes into a terminal shape that never resolves to a `Step::Final` and no
subsequent `Step::Final` runs (for example the iteration-limit summary path,
`native_tools.rs:960-1015`/`summary_step_text`, returning a tool-call-shaped summary), the fragments are
already persisted by `persist_continuation_messages` but no replacement is recorded. The gateway then falls
back to persisting the full delivered `reply` (prefix + terminal text), duplicating the already-persisted
prefix in durable history.

**Reproduction**: force a length continuation, then starve the tool loop to the iteration limit so the
summary step returns a `ToolCalls`-shaped response with content. Inspect `load_lifecycle_messages`: the
joined prefix appears both as the separately-persisted fragment rows and inside the final gateway-persisted
answer.

**Assessment**: low, and requires a contrived provider shape (a continuation immediately followed by an
iteration-limit summary that is not a clean final). The common paths (continuation to Final, and
continuation to ToolCalls to tool to Final) all set the replacement and do not duplicate.

---

## Risks I examined and found SAFE

- **Retry ceilings**. Text continuation: `length_continue_retries` gives 4 length responses before ceiling
  (`native_agent.rs:5432`, `4369`), matching length contract 8.1. Tool truncation:
  `truncated_tool_call_retries < 4` yields 5 total attempts (`native_agent.rs:5414`), matching
  tool-truncation contract 5.1 and the `[4096,8192,16384,32768,32768]` body assertion in
  `truncated_tool_call_retries_same_request_then_refuses_execution`. Safe.

- **Exponential cap schedule**. `apply_main_length_continuation_cap` (`native_agent.rs:407-430`) computes
  `base * 2^attempt`, floors at the requested cap, and ceils at `max(32768, requested)`, matching contract
  5.2 exactly; the golden-backed unit test `tool_truncation_retry_policy_matches_python_goldens` pins
  attempts 1..4. `output_retry = length_continue_retries.max(truncated_tool_call_retries)` keeps the two
  lanes from compounding. Safe.

- **Same-request retry transcript non-pollution**. The truncated-tool lane `continue`s without pushing any
  assistant or nudge message (`native_agent.rs:5414-5417`); the test asserts all 5 request bodies carry
  identical `messages`. Matches tool-truncation contract 4.1. Safe.

- **Tool execution prohibition**. The broken tool call is never turned into a `Step::ToolCalls` that runs:
  the length lane either retries or returns `PartialFinal`, and the independent invalid-JSON guard
  (`native_tools.rs:773-789`) refuses unterminated args even under a rewritten `finish_reason`. The
  recovery test `recovered_tool_call_executes_once_after_same_request_retry` shows exactly one tool event
  after a same-request retry. Safe for tool-execution safety.

- **Usage attribution**. Rejected truncated-tool attempts `continue`/`return` without `capture_usage`
  (`native_agent.rs:5414-5425`), so they are unbilled per contract 9.1; successful recoveries and every
  text continuation attempt do capture (`native_agent.rs:5431`, `5483`, `5511`; `4367`, `4395`), matching
  length contract 11.1. I checked each iteration consumes `usage` at most once (the length block is
  `finish_reason=="length"`-gated, so it cannot double-move with the `continuation_ready` block). Safe.

- **Durable role alternation for the tested shapes**. `append_native_continuation_messages` validates even
  length, strict assistant/user alternation, non-empty string content, and absence of tool_calls
  (`session_db.rs:3960-3976`), and gates on `partial_turn_phase` being `Assistant` or empty `Tools`
  (`session_db.rs:4013-4019`). The ceiling repair is admitted only against a `tool` tail via the
  `_interrupted_tool_terminal` carve-outs (`session_db.rs:4100-4183`). The four integration tests assert
  the exact durable role vectors (`["user","assistant","user","assistant"]` and
  `["user","assistant","tool","assistant"]`). Safe for the shapes exercised.

- **No double-persist of the settled answer**. On a successful continuation the fragments are persisted by
  `append_native_continuation_messages` and the gateway persists only the replacement suffix
  (`assistant_reply_for_history` returns the `durable_reply_replacements` entry, `native_agent.rs:4761-4773`),
  not the full delivered `reply`. This is the core of the design and it holds on all common paths. Safe.

- **Ceiling partial is still persisted (no wedge) on both lanes**. At a text ceiling neither lane persists
  the intermediate pairs (persistence happens only on the success/`WithContinuation` path), the partial is
  delivered, and the gateway persists the delivered `reply` as a single settled assistant row even though
  the turn returns `Err` (`dispatch.rs:760-781` runs `end_turn` irrespective of `succeeded`). Result is
  `[user, assistant(joined partial)]`, i.e. the collapsed single settled turn the length contract 10
  requires, with no dangling nudge. The only divergence is a missing `finish_reason:"length"` marker on
  that row, which is cosmetic. Safe against the wedge the contract warns about.

- **Concurrent-turn isolation**. `turn_reply_durable`, `turn_has_continuation`, and `turn_reply_replacement`
  are replaced with fresh Arcs per admitted turn (`native_agent.rs:4582-4585`) and shared only down into the
  turn's own sub-route clones (`native_agent.rs:2340` region). `delivery_only_replies` and
  `durable_reply_replacements` are keyed by session id and cleared at turn start and in finalize
  (`native_agent.rs:4589-4598`, `4980` region), and same-session concurrency is excluded by the turn lease
  that `append_native_continuation_messages` itself re-checks (`session_db.rs:3980-3992`). Safe.

- **Whitespace-safe joining parity**. `main_join_length_parts`/`main_length_needs_separator`
  (`native_agent.rs:1006-1013`, `1013+`) and `append_continuation_text` (`native_tools.rs:581-593`)
  implement the same rule as Python `_join_truncated_parts` (insert `\n` only between two non-whitespace
  boundaries). Joining parts then appending the final is associative with a single-pass join, so the tool
  path and the no-tools `continuation_join_after` path both land on the contract's accumulation. Safe.

- **Prompt/cache bytes**. The continuation request grows `request_messages` by exactly the assistant
  fragment (with `finish_reason:"length"` and any preserved reasoning fields) plus the nudge, matching the
  contract's wire shape; the buffered test asserts `bodies[1]["messages"]` is exactly
  `[user, assistant("part one"), user(prompt)]`. No scaffolding marker leaks to the wire because the Rust
  port carries no `_length_continuation_*` wire tags in the first place. Safe.

- **Multimodal/array assistant content rejection**. `append_native_continuation_messages` rejects non-string
  content (`session_db.rs:3967-3972`). For chat-completions assistant output, content is a string or null,
  and the length lane only builds a fragment when visible text exists, so the array case is unreachable on
  this transport. Safe for chat-completions; would need revisiting only if a future transport emits
  structured assistant content into this lane.

---

## Deferrals catalog (for the record, not defects)

Implemented faithfully by this change: genuine output-cap length continuation (both streaming and buffered),
the exact durable fragment/nudge history with lease-and-phase-guarded atomic pairs, and the bounded
same-request truncated-tool retry with ceiling repair.

Deferred from the two contracts (none present in the diff): thinking-budget-exhaustion abort,
repetition-dominated abort (#86581), content-filter mid-stream fallback during continuation, Ollama GLM
`stop`-misreport rewrite, `_is_stub_stall` network-vs-length distinction and its separate terminal message,
the network-stub and dropped-tool-names continuation prompts (and the 3-name cap), ephemeral reasoning-off
for thinking-only retries, and Codex/Anthropic/Bedrock truncation lanes. F1 through F3 are the user-facing
slices of this set; confirm they are intended deferrals.

## Primary-lane disposition

- F1, F2, and F3 are confirmed deferrals and remain listed in `PORT.md` and the
  checkpoint resolution. They are the next continuation/stall cluster, not
  behavior claimed by this checkpoint.
- F4 was fixed before commit. Finalization now removes the per-session durable
  replacement and appends that final provider suffix to the external-memory
  transcript, while the separate response argument remains the stitched answer.
- F5 is accepted as the required fail-closed behavior. Losing the admitted turn
  lease cannot be converted into an unguarded or partial transcript write.
- F6 was fixed before commit. The native buffered parser now detects the
  router-rewritten unterminated JSON shape, returns the same delivery-only
  partial terminal, performs zero retry and zero tool execution, and can repair
  a prior completed tool tail.
- F7 remains a low, contrived iteration-summary boundary. Normal continuation to
  final and continuation to tool calls to final are covered by live tests and
  use suffix-only durable projection.
