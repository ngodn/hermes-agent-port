# Native title implementation review, Claude

Claude independently reviewed the native title checkpoint against the Python
gateway, state store, schema, and tests.

## Findings

1. It found that `SessionDb` had no explicit SQLite busy timeout. The new
   immediate title and index transactions could therefore fail quickly during
   Python/Rust shared-database contention. `SessionDb::open` now installs the
   same five-second wait as Python's default connection, and the schema test
   verifies the live pragma.
2. It found the same reset-warning newline mismatch as Gemini. This is fixed.
   Its punctuation difference is intentional under the repository's no-em-dash
   rule.
3. It noted Rust tells the user when a reset title cannot be persisted, while
   Python silently returns a titled success response for unexpected database
   failures. The native behavior is retained because a false persistence claim
   would hide user-data loss. Duplicate and validation failures still match the
   Python contract.
4. It requested stronger regression coverage for cache preservation, held-turn
   serialization, and reset validation branches. The HTTP test now asserts the
   cached client build count and durable conversation generation remain
   unchanged across `/title`; a lease test proves title waits for the current
   transcript; and HTTP coverage includes duplicate, too-long, and cleaned-empty
   `/new` titles.

The review confirmed exact sanitizer ranges, manual provenance, NULL-safe CAS,
compression-ancestor transfer, hidden Bot Chat guard, unique-index repair,
lazy materialization, ingress symmetry, and prompt/transcript isolation.
