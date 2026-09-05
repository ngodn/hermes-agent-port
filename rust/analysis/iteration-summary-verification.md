# Native iteration-exhaustion summaries

The native tool loop now finishes a normal budget exit with a summary request,
following the active Python finalizer path. Provider errors in the ordinary loop
still return immediately and do not trigger a summary.

The summary appends the exact `MAX_ITERATIONS_SUMMARY_REQUEST` after the existing
messages. It supplies no tools, and the native HTTP sender removes tools,
tool_choice, and parallel_tool_calls after applying overrides. Responses cannot
execute additional tools. Empty raw text gets one retry with identical messages;
a nonempty thinking-only response is cleaned and uses the fallback without a
retry. A failed summary request becomes the reference-style user-facing fallback.

Evidence:

- 17 fixtures evaluate the actual Python helper's regex call and extract its
  shared request constant. They include multiline, incomplete, nested and
  case-sensitive think tags plus Python whitespace differences.
- Inline tests cover empty retries, thinking-only replies, summary errors,
  unsolicited tool calls, immutable prefixes and the two-attempt bound.
- The cap regression verifies exactly three executed tools even when the
  provider continues returning tool calls during both summary attempts.
- The real HTTP regression completes nine tool rounds under the default limit.
  With max_turns=3, it makes three ordinary requests and one tool-free summary
  request, then returns the summary successfully.
- Workspace: 1,170 tests passed, one existing bridge test ignored.

This does not complete Python finalizer parity. Native requests still use the
native shared provider builder, not all of the summary-specific
reasoning and provider branches in `agent/chat_completion_helpers.py`. Pending
verification-answer preservation, full resumed-message repair, prefill/system
assembly, relay lifecycle accounting and non-chat transports remain to integrate.
The eligible exit implemented here is the native loop's normal cap exhaustion;
the Python finalizer's richer exit classifications remain with the wider loop port.

Temperature follow-up: summary calls now omit the main request's temperature,
including a caller override, and use 0.5 only for the exact normalized Arcee
Trinity Large Thinking model name. Kimi and all other models omit the field.
76 cases execute the actual auxiliary policy across model spellings and URLs;
URL does not affect this reference function. The real HTTP test now covers
ordinary, Kimi and Arcee summary requests with a main-turn temperature of 0.9,
checking both omission and replacement across 22 requests. Clippy with warnings
denied and formatting passed.
