# Native refusal payload selection

The non-streaming native tool path now promotes message.refusal when the
response has no usable text or parsed tool calls. Whitespace follows Python
str.strip, including U+001C and excluding U+200B. Visible content and valid tool
calls take precedence over refusal metadata. The explanation is preserved
verbatim and flows through the existing final-reply events.

## Evidence

- gen_refusal_goldens.py executes the actual normalize_response method extracted
  from agent/transports/chat_completions.py, with SimpleNamespace SDK-shaped
  inputs and normalized output containers. Its 48 cases vary absent, empty,
  whitespace and usable content/refusal values, with and without tool calls.
- The comparison failed before the change because refusal-only content became
  an empty Final step. See refusal-before.log.
- A real local HTTP response carrying a refusal reaches the tool-loop event
  channel as one text chunk followed by one final stop, after one request.
- Workspace: 1,143 passed, one existing bridge test ignored. Formatting and
  warnings-denied Clippy pass. Logs: refusal-tests.log, refusal-clippy.log.

## Remaining scope

This is payload selection, not full normalized-response support. Finish-reason
classification, provider metadata, refusal fallback policy and streamed refusal
assembly remain unported. Durable response records and the complete agent loop
are still required. No refusal was reinterpreted as a tool execution request.
