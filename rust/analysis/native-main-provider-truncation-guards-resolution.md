# Native main-provider truncation content guards

Date: 2026-09-11

## Outcome

Ordinary native chat-completions turns now apply Python's content-level guards
before continuing a response whose `finish_reason` is `length`. The shared
classifier is used by the no-tools streaming path and the buffered tool path,
so the decision order cannot drift between transports:

1. Structured tool calls bypass these guards and remain in the existing
   truncated-tool-call lane.
2. A recognized inline reasoning tag with no visible answer aborts immediately.
3. Visible text dominated by a repeated exact fragment aborts immediately.
4. Empty content arms a reasoning-off override for exactly the next request.
5. Other visible text follows the existing semantic continuation path.

Thinking-exhausted and repetition diagnostics are delivery-only. The rejected
fragment does not enter SQLite, native usage accounting, micro-compaction, or
external-memory completion. The guards do not mutate the system prompt, frozen
tool schema, or prior assistant messages.

## Python contract

AGY owned the reference lane. It inspected the live Python loop and executed
the real think-block, repetition, reasoning-wire, and output-cap helpers. The
result is a deterministic 41-case corpus across three sections:

- 16 thinking-exhaustion cases
- 15 repetition cases
- 10 empty-reasoning and one-shot lifecycle cases

The Rust detector consumes this corpus in unit tests. The generator's focused
Python suite passes 20 tests, and `--check` confirms that the checked-in JSON is
byte-for-byte current. See
[main-provider-truncation-guards-agy.md](main-provider-truncation-guards-agy.md)
and
[main-provider-truncation-guard-goldens.json](../tools/main-provider-truncation-guard-goldens.json).

Claude used a separate lane to map only the future local Ollama/GLM
stop-to-length correction while implementation proceeded. That work did not
duplicate the AGY contract and did not change this checkpoint. See
[ollama-glm-truncation-claude.md](ollama-glm-truncation-claude.md).

After implementation, Claude reviewed the completed diff. It found that a
mixed buffered sequence could discard earlier visible fragments when its
fourth response was empty, and that a locally synthesized post-visible stall
could enter the repetition guard even though dropped streams are a separate
lane. Both findings were reproduced with focused tests and fixed. The mixed
ceiling now returns the joined model-authored partial, and content guards run
only for a provider-reported length. Its reasoning-effort-only hardening note
was also applied. See
[main-provider-truncation-review-claude.md](main-provider-truncation-review-claude.md).

## Implementation

`main_provider_truncation.rs` owns the provider-neutral guard ordering, exact
user guidance, Python-compatible Unicode character counts and line splitting,
the conservative 400-character repetition threshold, and the one-shot
reasoning override. Both provider call paths consult this module only after a
successful response has explicitly reported `length`.

The empty reasoning path does not append an empty assistant row. Its
continuation nudge is merged into the current model-facing user message for the
request, leaving clean display content unchanged. Each empty response raises
the output cap through the existing bounded schedule. Four empty attempts emit
an actionable delivery-only result and purge turn-local scaffolding.

If a route returns HTTP 400 with `reasoning is mandatory` after the one-shot
disable, the native client retries once using the user's original reasoning
configuration. The route remembers that rejection across conversation-client
clones. Future configured or ephemeral disables are omitted for that route,
while an enabled user configuration is replayed verbatim. This keeps the retry
on the conversation's established provider cache key.

## Durable transcript rules

Normal visible continuation still commits assistant-fragment/user-nudge pairs
atomically. An empty reasoning response has no valid assistant row, so the
SQLite transaction handles a leading marked nudge in one of two ways:

- After the current user, it merges the nudge into `api_content` while
  preserving clean display text.
- After a completed tool-call group, it inserts one user continuation row.

The transaction validates the live session, lineage lease, current tail, and
all remaining assistant/user pairs before writing. A stale lease changes
nothing. Structured multimodal `api_content` remains sentinel-encoded in
SQLite, gains a text part for the nudge, and is decoded back to the original
array shape at the provider projection boundary.

## Verification

Focused native tests use local HTTP servers and real SQLite files to prove:

- immediate tagged-thinking and repetition termination with one provider call
- tool-call preemption of the content guards
- one-request reasoning disable in streaming and buffered paths
- restoration of the original reasoning config on the following request
- mandatory-reasoning rejection retry on the original cache key
- a four-request ceiling with no next-turn flag leakage
- mixed visible-then-empty ceilings preserving the visible partial
- post-visible stalls bypassing the provider length-content guards
- no empty assistant messages on provider requests
- atomic string and structured sidecar persistence
- stale-lease rollback and completed-tool-tail continuation
- clean display history with exact model-facing nudge reuse

The full workspace and static checks are recorded in `PORT.md` after final
validation.

## Remaining seam

The next independent main-provider seam is the conservative local Ollama/GLM
rewrite from a misreported `finish_reason="stop"` to `length`. Dropped-stream
stub recovery, run-budget scaling, operator notices, non-chat transports, and
other OAuth paths also remain outside this checkpoint.
