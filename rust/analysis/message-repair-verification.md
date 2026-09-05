# Thinking-only removal and user-message repair

`message_repair.rs` ports `AIAgent._is_thinking_only_assistant` and
`agent.agent_runtime_helpers.drop_thinking_only_and_merge_users`. It runs on the
native request copy after sidecar substitution and reasoning projection, before
transport schema stripping. This keeps prefill markers available for detection
and prevents stale api_content from replacing a merged user message.

The detector follows Python precedence: tool calls survive; prefill stubs drop;
visible payloads survive; compaction checkpoints protect ordinary reasoning
carriers; reasoning fields and the Codex flag then determine thinking-only turns.
Merging preserves the first user's metadata and joins strings with two newlines
only when both are nonempty. Mixed strings and multimodal lists become ordered
blocks. Unknown content shapes stay separate, as in Python.

Evidence:

- Claude Opus 4.8 generated 67 oracle cases by extracting all three actual Python
  definitions, including has_compaction_checkpoint. The main agent checked the
  extraction and independently regenerated with Python 3.12.13.
- Inline Rust comparisons cover the whole corpus and additional precedence
  regressions. Fixtures assert the Python function leaves its input unchanged.
- The real main/summary HTTP test now includes a healed prefill stub between a
  sidecar-bearing user message and an image-bearing user message. It verifies
  removal, correct text/image merging, identical outgoing prefixes, and unchanged
  source history.
- Workspace: 1,175 tests passed, one existing bridge test ignored.

The later [tool-pairing port](tool-pairing-verification.md) adds orphan
tool-call/result repair. Native transport checkpoint replay remains. This is the Chat Completions send boundary; it does
not make the current HistoryMessage persistence model carry all reasoning fields.
Non-object top-level messages are not a Python success case: the source merge
pass calls .get on kept entries. The Rust helper tolerates them but the oracle
corpus covers the valid dictionary-message interface.

Empty-message healing now runs before thinking-only removal, following
`repair_empty_non_final_messages` and `_msg_has_payload` from the Python runtime
helpers. It fills empty non-final user and assistant turns with the exact
`[response interrupted]` placeholder. The last message, other roles, structured
payloads and reasoning/Codex carriers are left alone. 140 source-executed cases
cover content shapes and structural fields; the main/summary HTTP test verifies
an empty historical assistant is healed on both sends without changing its
stored copy. Clippy with warnings denied and formatting passed. Python's local
healing log/notice aggregation is not yet integrated into Rust event delivery.
