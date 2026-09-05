> Historical helper report. Main integration added full-constructor cases with
> real scanner findings; the current generator has 62 cases and does not require
> all build cases to be benign. See [tool-result-verification.md](tool-result-verification.md).

# Port: `make_tool_result_message` and untrusted-content framing

Source: `agent/tool_dispatch_helpers.py` (the tool-result slice).
Target: `rust/crates/hermes-gateway/src/tool_result.rs`.
Generator: `rust/tools/gen_tool_result_goldens.py`.
Fixture: `rust/tools/tool-result-goldens.json` (56 Python-executed cases).

## Scope

Ported the tool-result construction path only, not the parallelism / mutation /
trajectory helpers that share the Python module. The functions carried over:

- `make_tool_result_message` -> `pub fn build`
- `_normalize_tool_call_id`
- `_is_untrusted_tool`
- `_detect_upstream_elision`
- `_maybe_append_elision_notice`
- `_tool_output_risk_metadata`
- `_neutralize_delimiters`
- `_maybe_wrap_untrusted`

Public API:

```rust
pub fn build(
    name: &str,
    content: &Value,
    tool_call_id: &Value,
    timestamp: &Value,
    effect_disposition: Option<&str>,
) -> Value
```

Everything else is a private helper reachable from `build`, so the module needs
no `allow(dead_code)`. It compiles into the crate once `mod tool_result;` is
added to `main.rs` (left out here per the no-`main.rs`/no-build constraint;
dead-code warnings until `build` is wired into a call site are expected and
vanish on first use).

## Behaviour preserved (and pinned by tests)

- **Constructor order.** Raw content is checked for elision markers and the
  notice appended first, then the result is wrapped, so the notice lands inside
  the untrusted block next to the data. Risk metadata is classified from the RAW
  content, before wrapping. Message keys are inserted in Python's exact order:
  `role, name, tool_name, content, tool_call_id, timestamp, [_tool_output_risk],
  [effect_disposition]`. Because `serde_json` uses `IndexMap` (`preserve_order`
  is on) and `IndexMap` equality ignores order, the build test pins the key
  sequence separately from the value comparison.
- **Composite id normalization.** `"call|extra"` -> `"call"`, stripped with
  CPython `str.strip` whitespace (via `python_value::python_whitespace`).
  Non-string ids and strings without a `|` pass through untouched.
- **Elision detection.** Only string content, only when length (code points)
  reaches 1,000, scan bounded to the first 65,536 code points. The four
  provider-side markers (`...N more item(s)`, `"has_more":true`,
  `saved to sandbox`, `data_preview`) are hand-matched to keep CPython regex
  semantics: `\s` is `python_whitespace` (CPython `\s` includes U+001C..U+001F
  beyond Unicode White_Space), `\d` is any Unicode decimal digit
  (`python_value::decimal_digit`), and letters match case-insensitively.
- **Case-insensitive delimiter neutralization.** `untrusted_tool_result` is
  matched case-insensitively including CPython's Unicode `re.IGNORECASE` quirks.
  For the characters in these tokens the only extras are `s` also matching the
  long s (U+017F), `i` also matching U+0130/U+0131, and `k` matching U+212A
  (enumerated against CPython 3.12.13 directly). The delimiter token only
  contains `s` among these, so `untruſted_tool_reſult` and
  `</UNTRUSTED_TOOL_RESULT>` both neutralize to the fixed ASCII
  `untrusted-tool-result`. Replacement is non-overlapping, left to right.
- **Multimodal parts.** A content list has each `{"type":"text","text": str}`
  part wrapped individually with the string rules; non-text parts and text
  parts whose `text` is not a string pass through unchanged; other keys on the
  part are preserved in position (`{**item, "text": ...}` semantics). The outer
  list is rebuilt, so callers compare by value.
- **Risk metadata findings order.** Findings are collected across text parts in
  first-seen order and deduplicated. A list content with no text parts yields
  `None` (no `_tool_output_risk` key); non-string / non-list content yields
  `None`; string or non-empty text-part list yields
  `{"risk", "findings", "redacted": false}` in that key order.
- **Compatibility literals are byte-for-byte.** The wrap block and the elision
  notice reproduce the Python strings exactly, including their existing em
  dashes. Verified by reconstructing the Rust `format!` output in Python and
  diffing against the real function output. New prose comments avoid em dashes.

## Parity boundaries (stated honestly)

- **Timestamp.** Python's `make_tool_result_message` calls
  `stamp_message_timestamp`, whose contract is "set `message["timestamp"]` only
  when absent, else keep it; when absent use the caller value or the wall
  clock". The dict handed to that helper here never carries a `timestamp` key,
  so the guard always fires. This port takes the already-resolved `timestamp`
  as an argument and inserts it verbatim. The wall-clock fallback for a missing
  stamp (Python's `wall_time()`) is the caller's responsibility, not this
  function's. The generator's stamp stub injects a fixed timestamp per case,
  which is behaviourally identical to the real helper on this path.
- **Risk scanner.** `build` wires the real
  `crate::threat_patterns::scan_for_threats(text, "context")` (ported
  separately by another workstream) into the findings assembly. The dedup and
  ordering logic owned here is exercised directly by an inline test with a
  deterministic stub scanner. The golden `build` cases deliberately use benign
  content so the recorded metadata (`risk: "low"`, `findings: []`) is
  independent of the pattern set; the generator asserts none of them trip the
  real scanner. End-to-end parity of high-risk finding IDs is validated by the
  `threat_patterns` port's own goldens, not here.

## Generator

Follows the sibling `ast`-extract pattern: pulls the eight functions plus the
module-level constants/regexes they close over out of the source and execs them
into one namespace, so the real CPython code runs without importing the agent
runtime. `stamp_message_timestamp` and `logger` are stubbed; `scan_for_threats`
is the real one from `tools/threat_patterns.py`. `--check` round-trips cleanly
under `mise x python@3.12.13`.

## Verification done

- `gen_tool_result_goldens.py` and `--check` both pass (56 cases) under Python
  3.12.13.
- Wrap block and elision notice reconstructed in Python and diffed against the
  real function output: exact match.
- The Rust elision matchers and delimiter neutralizer were mirrored 1:1 in
  Python and run against every `detect_elision` and `neutralize` golden input:
  zero mismatches. This substitutes for a `cargo test` run, which was out of
  scope (no `mod` line in `main.rs`, and cargo must not be run here).

No cargo run, no `main.rs` / native / Cargo edits, no commit. Only the four
owned files were created.
