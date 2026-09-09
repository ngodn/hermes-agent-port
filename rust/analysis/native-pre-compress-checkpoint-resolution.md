# Native pre-compression memory checkpoint resolution

Date: 2026-09-09

## Scope

This checkpoint connects Python's versioned external-memory boundary to every
native full-compression path:

- manual `/compress`
- automatic compression before an inbound turn is persisted
- same-turn compression after a complete tool-result batch is durable

The goal is narrow. Before a full compression can discard direct evidence, an
active memory provider gets the complete durable snapshot. Its sanitized return
text can inform the summary request. This does not add a native memory manager,
session-switch notifications, or a new model tool.

## Host protocol

The persistent Python extension host now accepts `pre_compress` with three
strict parameters:

- `messages`: a JSON list containing the raw durable transcript projection
- `require_checkpoint`: an actual JSON boolean
- `checkpoint_api_version`: a positive integer, currently version 2

It delegates capability probing and callback execution to the existing
`MemoryManager`. Legacy providers retain the raw-message API v1 contract.
Providers advertising version 2 receive the shared direct-evidence projection
from `agent.conversation_compression`, which excludes system rows, tool rows,
derivative compressed summaries, and prose-free assistant tool wrappers.

The host returns both `checkpoint_supported` and nullable `memory_context`.
Provider text is passed through the existing credential and size sanitizer
before it crosses the process boundary. Missing managers, unsupported versions,
probe failures, and callback failures become protocol errors in required mode.
Optional mode remains best effort.

The Rust decoder denies unknown fields and requires both result fields, including
an explicit string or null for `memory_context`. The 310 second request timeout
allows provider-backed persistence work while remaining bounded.

## Native ordering and durability

All three callers choose a valid compression boundary first, then invoke the
checkpoint with the complete exact SQLite snapshot before any summary provider
request. Only after a successful required checkpoint, or an optional best-effort
attempt, can summarization begin.

The summary receives its already selected history region plus the sanitized
memory context. The context is serialized as one JSON string inside a dedicated
source-material fence. Angle brackets and ampersands are escaped so provider
text cannot close the fence or become prompt instructions.

Required mode has fail-closed publication semantics:

| Condition | Required mode | Optional mode |
| --- | --- | --- |
| No manager or compatible provider | No summary and no transcript mutation | Continue without context |
| Capability probe or callback fails | No summary and no transcript mutation | Continue without context |
| Callback succeeds with empty context | Continue, checkpoint was completed | Continue |
| Callback returns sanitized context | Feed context to summary, then use normal guarded publication | Same |

The durable route, transcript, and lineage leases remain unchanged. Checkpoint
and summary I/O stay outside SQLite transactions. Exact-snapshot comparison at
publication still rejects concurrent transcript changes.

## Transcript projection

`CompressionHistoryMessage::lifecycle_value` reconstructs the lifecycle-hook
shape from the wide durable row. It preserves role, model content, API content,
tool identity, tool calls, effect disposition, assistant reasoning and replay
sidecars, plus `_compressed_summary` for derivative rows. Display-only database
fields remain private.

Preserving `_compressed_summary` is load-bearing. Without it, a resumed native
conversation could send an old generated summary to a version 2 memory provider
as if it were new direct evidence.

## Conversation identity

Preflight, checkpoint, summary, and normal turns all use the same bounded
per-home, per-session `ConversationAgent` client. Compression does not rebuild
the frozen prompt or tool snapshot, and in-place compression does not evict the
client. Rotation keeps the existing physical-session behavior.

## Helper division and review disposition

The helper work was split by file ownership and did not duplicate effort:

- AGY ran once behind the repository's auth lock and implemented only the
  Python host endpoint and its focused Python tests.
- Claude implemented only the Rust JSONL client method and protocol tests.
- The primary lane integrated the three production compression paths, built the
  durable projection and prompt boundary, reviewed both helper changes, and ran
  the complete proof.

Primary review tightened the Rust result decoder so a missing nullable field is
rejected, extended the timeout for real provider persistence, retained the
compressed-summary marker in the durable projection, and removed provider
exception contents from newly added required-mode host errors.

## Validation

- Full Rust workspace: 1,704 passed, 2 ignored (1 core, 1,703 gateway).
- Targeted Python checkpoint, memory-context, and extension-hook suites: 78
  passed.
- Same-turn and handoff source oracles: both match their generated corpora.
- Rust formatting, Clippy with warnings denied, Ruff lint for changed Python,
  and `git diff --check`: passed.
- Focused tests prove strict protocol validation, raw versus direct-evidence
  routing, optional and required failure behavior, sanitization, subprocess
  dispatch, full-snapshot ordering, provider-context forwarding, immutable
  transcript failure paths, and reuse of the cached conversation client.

## Deferred work

- Session-switch and compression-boundary extension notifications
- Safe mid-turn rotation with immutable client and lease rebinding
- Overflow recovery and compression stall handling
- Configured multi-provider auxiliary fallback chains and non-chat transports
- Native plugin and memory managers, including extension-host restart recovery
- The broader native tool, provider, approval, delegation, and platform surface
