# Native local Ollama GLM stop correction

Date: 2026-09-11

## Outcome

Native chat-completions turns now recognize Python's narrow local Ollama and
GLM failure mode: after tool history, a sufficiently long visible response can
end mid-sentence while Ollama reports `finish_reason="stop"`. The serving route
rewrites only that response to `length`, then reuses the existing bounded
continuation path, output-cap schedule, exact prompt, suffix delivery, and
durable transcript rules.

The correction runs in both native response modes. Buffered tool-enabled turns
cover the immediate post-tool failure that motivated the Python workaround.
No-tools streaming turns also apply it when a valid restored transcript already
contains tool history. Both paths use the actual serving route's provider,
model, and endpoint, including fallback and credential-pool endpoints.

Hosted Ollama, `:cloud` models, unrelated local servers, non-GLM models,
responses without tool history, active tool calls, non-string or short content,
single-token text, and naturally terminated answers remain ordinary `stop`
responses. The classifier never changes the system prompt, tool schema,
credential state, route cursor, or prior transcript bytes.

## Python contract

AGY owned the executable reference lane. It exercised the live Python
`_is_ollama_glm_backend`, `_should_treat_stop_as_truncated`, natural-ending,
visible-text, and continuation-prompt helpers. The deterministic corpus has 113
cases, with 43 positive and 70 negative outcomes across every ordered gate.

Primary review found that the first generator recorded declared matrix
expectations without asserting them. That hid an invalid Chinese positive which
failed both the minimum length and whitespace requirements. The generator now
asserts every declared expectation against the real Python helper, retains raw
content and history types, and checks its output byte for byte. The Rust
classifier consumes all 113 corrected cases. See
[ollama-glm-truncation-contract-agy.md](ollama-glm-truncation-contract-agy.md)
and
[ollama-glm-truncation-goldens.json](../tools/ollama-glm-truncation-goldens.json).

Claude worked on a separate future lane while the current implementation and
oracle proceeded. Its map isolates clean SSE EOF and post-delta transport
failure recovery from this provider-specific correction. See
[dropped-stream-recovery-claude.md](dropped-stream-recovery-claude.md).

After implementation, Claude separately reviewed classifier parity, Unicode and
truthiness behavior, call ordering, fallback and credential-pool route identity,
streaming and buffered integration, cache safety, usage, persistence, and test
strength. No correctness finding survived verification. Its residual notes are
future-shape risks rather than reachable defects in the current text-only stream.
See
[ollama-glm-truncation-review-claude.md](ollama-glm-truncation-review-claude.md).

## Classifier and ordering

`ollama_glm_truncation.rs` owns one pure decision over the normalized response
and serving route:

1. The finish reason is exactly `stop` and the API mode is chat completions.
2. The model contains `glm`, or the provider is exactly `zai`, after lowercase
   normalization.
3. The endpoint does not contain `ollama.com`, and the model does not contain
   `:cloud`.
4. The endpoint contains `ollama` or `:11434`, or the provider is exactly
   `ollama`.
5. At least one provider-visible history object has role `tool`.
6. The assistant exists, has no truthy tool calls, and has string content.
7. Visible content remains after Python-compatible reasoning and tool markup
   removal.
8. That text contains at least 20 Unicode characters and at least one
   Python-compatible whitespace character.
9. The final visible character is not a natural terminal boundary.

Natural boundaries include a closing triple-backtick fence, caret, the exact
ASCII and CJK punctuation set used by Python, and any final code point at or
above U+1F300. The checks use Unicode character counts rather than UTF-8 byte
lengths and preserve Python's four extra C0 whitespace separators.

## Runtime and durability

The buffered call site corrects the normalized choice before successful-body,
tool-truncation, length-guard, usage, and empty-response classification. The
streaming call site records the actual endpoint selected for that request and
corrects the assembled outcome before the shared length guard. A warning names
only the provider and model, without exposing endpoint credentials or response
content.

Once corrected, no special retry machinery is introduced. The established
length path commits an assistant fragment with `finish_reason="length"` and the
exact continuation user nudge before the follow-up request. Provider request
projection strips transcript-only finish metadata. Final delivery stitches the
visible prefix and suffix once, while SQLite stores the fragment, nudge, and
final provider suffix as the same alternating history the provider observed.

## Verification

Focused native tests prove:

- a real current-time tool round followed by a misreported local GLM stop
  triggers exactly one continuation
- the third buffered request retains the tool-call pair and uses the raised
  output cap plus exact continuation prompt
- the delivered answer is stitched once and SQLite stores the corrected
  assistant fragment, nudge, and final suffix in order
- restored durable tool history enables the same correction on the no-tools
  SSE path
- a `:cloud` model, a naturally terminated answer, and a non-GLM model do not
  issue a continuation
- all 113 source-executed Python classifier cases agree with Rust

The focused Python suite passes three correction tests. The full workspace
passes 1,928 Rust tests with two ignored. The 113-case corpus regenerates byte
for byte, and Rust formatting, Python formatting, Ruff, workspace Clippy with
warnings denied, and diff hygiene pass.

## Progress and remaining seam

The weighted full-port estimate is **58.75%**, reported as about **59%**, with a
judgment range of **55% to 61%**. Native agent core moves from 81% to 82%; other
area estimates remain unchanged.

The next isolated main-provider checkpoint is dropped-stream recovery. A clean
SSE EOF with visible text but no finish reason and no usage frame currently
looks complete, while a transport error after visible deltas currently
propagates instead of preserving the prefix. Network-specific continuation
prompts, run-budget scaling, operator notices, non-chat transports, remaining
OAuth routes, dynamic providers, native plugin and external-memory managers,
prompt invalidation, and broader client eviction remain.
