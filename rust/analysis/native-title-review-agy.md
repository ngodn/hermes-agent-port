# Native title implementation review, Gemini

Gemini reviewed the uncommitted native `/title` and `/new <title>` checkpoint
against the current Python source.

## Findings

1. It reported a possible panic from indexing `entry.fields["display_name"]`
   while materializing a cold session. Every current `SessionEntry` constructor
   supplies that default, but the code was hardened to use `get` so a future or
   legacy constructor cannot turn metadata repair into a panic.
2. It claimed a unique-index collision could occur between the conflict query
   and title update and surface as a database error. This premise does not hold
   for this implementation because `BEGIN IMMEDIATE` obtains SQLite's writer
   reservation before the conflict query. A deterministic transaction boundary
   and a concurrent two-connection claim test leave one winner and one ordinary
   duplicate rejection.
3. It found the obsolete Rust-only non-unique title index would remain in old
   databases. The migration now drops it after establishing the Python-compatible
   partial unique index.
4. It noted the reset warning used two newlines where the English Python locale
   uses one. The native replies now use one newline. ASCII hyphens remain
   deliberate because this repository forbids em dashes in code and prose.
5. It noted sanitization occurs both at the command boundary and inside the
   database API. That is intentional. The command needs distinct empty-title
   feedback, while the database API must not let future callers bypass the
   invariant.

The review also confirmed model exclusion, cache stability, route then
transcript lease ordering, confirmation replay, and cold-row ownership
materialization.
