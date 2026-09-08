# Native title source map, Gemini audit

This is the retained source map distilled from the Gemini report produced by
`rust/tools/agy.sh`. Every point below was checked against the current Python
and Rust sources before implementation.

## Python contract

- `gateway/slash_commands.py:5139-5210` owns gateway `/title`. It resolves the
  current route, creates the SQLite row if the command precedes the first model
  turn, and either displays or writes the title.
- `gateway/slash_commands.py:301-322` applies `/new <title>` only after the
  destructive reset has been approved and completed. A rejected title leaves
  the new session alive and untitled.
- `hermes_state.py:11013-11058` strips unsafe ASCII and Unicode controls,
  collapses whitespace, treats an empty result as no title, and rejects more
  than 100 Unicode characters.
- `hermes_state.py:11097-11220` writes manual titles with `title_source=user`.
  The read, hidden canonical Bot Chat guard, uniqueness check, compression
  ancestor transfer, and value-matched update form one transaction.
- `hermes_state_schema.py:1608-1630` enforces non-null title uniqueness with a
  partial unique index and repairs legacy duplicates by retaining the newest
  row's title.

## Required native invariants

- `/title` and `/new <title>` are gateway control traffic. They must never enter
  the transcript or consume a model turn.
- A title is session metadata. It does not alter prompt bytes, change the
  immutable session ID, or evict the conversation client.
- Route and transcript leases must prevent a title write from landing on the
  wrong session while reset or resume is changing the stable route.
- Duplicate checks need a database constraint as the final race guard. An
  application query alone is not sufficient across processes.
- HTTP and push ingress must call one shared implementation.

## Narrow implementation boundary

This checkpoint should add the sanitizer, user-title transaction, unique-index
repair, native command classification, shared title command, and reset-title
write. Automatic LLM and derived titles, adapter topic renames, and UI title
state remain later work.

The helper suggested deferring compression-ancestor title transfer because
native `/compress` is not yet active. The implementation keeps that small
compatibility rule because Rust can open Python-created databases whose live
session is already a compression continuation. Deferring it would make manual
rename behavior depend on which runtime last touched the profile.
