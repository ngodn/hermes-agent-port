# Native tool replay integration

The native step now separates decoded execution arguments from the assistant
message replayed on the next iteration. Valid JSON argument strings retain
spacing, escapes and number spelling. Tool-call extra_content, assistant text
and reasoning sidecars survive parsing. Outgoing message projection operates on
a copy and removes provider-specific fields according to the target model.

The chat transport projection comes from ChatCompletionsTransport.convert_messages.
Reasoning echo policy comes from agent/message_sanitization.py; internal
reasoning and finish_reason are removed after copying reasoning to its wire
field, matching the preparation order in agent/conversation_loop.py.
The active client's model.reasoning_echo config flag enables custom endpoints.
Automatic echo families use the Python provider/model/hostname rules.

## Validation

- 1,141 workspace tests passed, one existing Python bridge test ignored.
- 30 Python-executed chat projection cases and 62 reasoning replay cases.
- Eight HTTP requests cover Gemini/Gemma signature retention, strict-provider
  filtering and DeepSeek reasoning replay through actual tool iterations.
- Twelve startup-to-HTTP requests cover config truthiness and configured
  provider identity in both streaming and tool modes, including a provider
  without a registered native profile.
- Inline tests check decoded execution arguments versus exact replay text,
  immutable source messages, internal sidecar removal and explicit echo opt-in.
- The HTTP regression first failed because a non-Gemini model received stale
  extra_content. Final logs: tool-call-replay-tests.log and
  tool-call-replay-clippy.log. Formatting and warnings-denied Clippy pass.

The helper also produced unused fallback/compaction predicates. These were
removed from the module and fixtures until their actual consumers are ported;
no blanket dead-code allowance remains in the new modules.

## Remaining contracts

- The full Python malformed-argument repair pipeline and argument coercions
  remain unported. Invalid execution arguments retain the existing empty-object
  fallback; valid argument text is no longer unnecessarily serialized again.
- Response content flattening, think-block removal, redaction, alias reversal,
  missing tool-call ID recovery and full response normalization remain separate.
- This proves in-flight replay. Durable tool-call/reasoning history and full
  session reconstruction are not yet wired through the native turn lifecycle.
- The final-answer Step remains text-only. Usage, refusal classification and
  complete reasoning delivery still require the native agent-loop port.
