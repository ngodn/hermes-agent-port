# Native visible-text length continuation

Date: 2026-09-10

## Result

The native no-tools chat-completions stream now continues a successful response
that ends with `finish_reason="length"`. The already delivered fragment is never
replayed. The next request stays on the frozen route and carries the original
turn, the assistant fragment, and Python's exact output-limit continuation
prompt.

Continuation output is joined with Python's whitespace rule, so adjacent words
do not become glued together. Each follow-up progressively raises the output
cap from the configured base, preserves a larger outgoing cap, and stops at
32,768 otherwise. Usage from every completed fragment remains in the main usage
accumulator.

The fourth truncated response closes delivery with the accumulated partial text
and returns a failed turn outcome. This preserves the useful partial answer
without claiming completion, and the next admitted turn starts with a fresh
continuation budget.

## Safety boundary

This is semantic continuation, not transport replay. A completed `length`
response proves the request ended cleanly and asks the model only for the
missing suffix. A stream error after visible output still fails immediately and
never retries or changes providers. Pre-body transport retry and fallback remain
owned by the existing shared dispatcher.

The system prompt and route plan remain immutable. Continuation messages are
turn-local wire context, and the gateway persists the assembled assistant reply
as one settled turn. No synthetic continuation marker enters SQLite, prompt
snapshots, plugins, or external memory.

## Helper split

AGY owned the Python length-continuation contract. Its source-executed generator
produced 104 cases across ten sections and its two focused Python suites passed
53 tests.

Claude owned the separate response-stall and client-rebuild question. It found
that Python's explicit client retirement is an httpx file-descriptor safety
mechanism that should not be copied to reqwest. The actual missing Rust behavior
is an inactivity deadline. That work remains separate because its configuration
ownership has not been ported yet.

## Verification

- Source-executed length-continuation corpus: 104 cases across ten sections.
- Focused Python continuation suites: 53 passed.
- Rust public-seam tests cover successful continuation, exact request history,
  output-cap growth, whitespace-safe delivery, and the four-response ceiling.
- Full Rust workspace: 1,888 passed, two expected ignores.
- Rust and Python formatting, Ruff, workspace Clippy with warnings denied, and
  diff hygiene pass.

## Deliberate limits

This slice covers visible text with an explicit `length` finish reason on the
ordinary streaming chat-completions path. Still remaining are buffered text
continuation inside tool-enabled turns, truncated tool-call retries,
thinking-only length handling and one-shot reasoning disable, repetition-loop
rejection, dropped-stream conversion to a network continuation stub, local
Ollama GLM stop-misreport detection, and the separate inactivity deadline.

The weighted full-port estimate remains 57.95%, reported as about 58%. This
narrow slice retires a real production gap but does not justify moving the
native-core inventory another whole percentage point while the related paths
above remain open.
