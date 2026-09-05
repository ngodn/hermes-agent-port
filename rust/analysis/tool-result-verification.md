# Tool-result construction integration

Tool-result construction is integrated into the native loop. Claude supplied
the constructor and Gemini supplied the scanner draft. Main integration replaced
the draft's partial normalization table with full Unicode 15 NFKC, corrected
Python word/space/ignorecase semantics, bounded input traversal and verified
real constructor-to-scanner and HTTP request behavior.

## Source contracts checked

- agent/tool_dispatch_helpers.py constructs results with role, name, tool_name,
  content and normalized tool_call_id, then timestamp and optional risk/effect
  metadata. Elision detection runs on raw content before untrusted framing.
- Wrapping is only for web_extract, web_search, browser_* and mcp_*.
  It applies to text of at least 32 Python characters, including individual text
  parts of multimodal results. Embedded delimiter tokens are neutralized before
  wrapping; already-wrapped input is not trusted as an escape hatch.
- Advisory findings are ordered and deduplicated by the result constructor.
  The scanner's invisible-character prefix comes from a Python set, whose order
  varies by process hash seed. Tests must distinguish set membership from the
  stable order of regex findings rather than freezing an arbitrary Python seed.
- Scan input is capped at 65,536 characters before NFKC. No threat match blocks
  or redacts the result. Advisory metadata must not reach the provider wire.
- Canonical tool IDs must match in assistant replay and tool results.

## Unicode dependency

unicode-normalization =0.1.22 supplies Unicode 15.0.0, matching the Python 3.12
reference. The fetched crate's src/tables.rs declares (15, 0, 0).
Upstream release history records that version's Unicode 15 update:
[Unicode 15 release history](https://github.com/unicode-rs/unicode-normalization/commits?after=576ae0b1407dd14854876c93f1a348df0c19dffe+34).
[Crate API reference](https://docs.rs/unicode-normalization/latest/unicode_normalization/).

## Runtime verification

A real HTTP tool loop returns external text containing a forged closing tag and
an elision marker, runs a second tool round and verifies that the first result
stays byte-stable in the outgoing messages. It also checks that name survives,
internal fields are stripped and composite call IDs match their results.

## Validation

- 62 source-executed constructor/helper cases, including actual threat findings,
  multimodal deduplication, short-output scanning and elision before wrapping.
- 129 source-executed scanner cases covering all 36 source patterns, scopes,
  truncation, Unicode whitespace, NFKC, Turkish I and Python word boundaries.
- Generated Python 3.12 alphanumeric/underscore ranges drive word classification.
  Input projection is valid for the current ASCII-literal pattern set, which is
  compared with the Python definitions by an inline test. A non-ASCII pattern
  addition requires updating that projection rather than silently approximating.
- Full workspace: 1,164 passed, one existing bridge test ignored. Formatting and
  warnings-denied Clippy pass. Logs: tool-result-tests.log and
  tool-result-clippy.log. Both Python generators pass --check.

## Remaining scope

Durable tool history and browser/MCP runtime registration remain separate work.
The constructor accepts an already-resolved timestamp; the native caller supplies
wall-clock seconds. Python's unordered invisible-character finding prefix is
made deterministic in Rust by first appearance; regex finding order is retained.
The scanner takes a fixed valid context scope here; its standalone invalid-scope
path panics rather than returning a Python ValueError. Neither scanner is a
cross-script confusable detector. Internal risk metadata remains advisory.
