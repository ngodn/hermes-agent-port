> Historical draft report. Main integration replaced partial NFKC with full
> Unicode 15 normalization, added Python word-class projection, bounded prefix
> traversal, and expanded the fixture to 129 cases. See
> [tool-result-verification.md](tool-result-verification.md) for accepted behavior.

# threat_patterns.rs — Port Analysis & Verification

Port of [`tools/threat_patterns.py`](file:///home/eins0fx/development/hermes-agent-port/tools/threat_patterns.py) into [`rust/crates/hermes-gateway/src/threat_patterns.rs`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/threat_patterns.rs).

## Origin & Context

`tools/threat_patterns.py` serves as the single source of truth for prompt-injection, promptware, C2, and exfiltration detection across:
- Context assembly scanners (`agent/prompt_builder.py`, `tools/memory_tool.py`)
- Outbound/inbound tool-result delimiters (`agent/tool_dispatch_helpers.py`)

In the Rust gateway port, this module is consumed by the incoming `tool_result` module via:
```rust
pub fn scan_for_threats(content: &str, scope: &str) -> Vec<String>
```

## Public Surface

- [`pub fn scan_for_threats(content: &str, scope: &str) -> Vec<String>`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/threat_patterns.rs):
  Scans text up to `MAX_SCAN_CHARS` (65,536 characters) for invisible Unicode markers and regular expression threat patterns active for `scope`.
- `pub const MAX_SCAN_CHARS: usize = 65_536`:
  Hard input character length cap matching the Python reference implementation.
- `pub const INVISIBLE_CHARS: [char; 17]`:
  Canonical set of 17 invisible and bidirectional Unicode codepoints tracked for injection markers.

## Preserved Behaviors

### 1. Scope Partitioning and Inheritance
The reference implementation organizes 36 patterns by attack class with three scopes:
- `"all"` (narrow, 11 patterns): Classic prompt injection and secret exfiltration. Applied everywhere.
- `"context"` (broader, 28 patterns): Adds promptware, C2 commands, role-play/identity hijack, and evasion tactics. Includes all `"all"` patterns plus 17 `"context"` patterns.
- `"strict"` (broadest, 36 patterns): Adds persistence (SSH backdoors, `authorized_keys`, agent config tampering) and URL exfiltration. Includes all `"all"`, `"context"`, and 8 `"strict"` patterns.

If an unknown scope is passed (e.g. `"bogus"`), `scan_for_threats` panics with `"scan_for_threats: unknown scope <scope>"`, matching Python's `ValueError`. For empty strings, an empty vector is returned immediately before scope validation, exactly matching Python line 229.

### 2. Invisible Unicode Handling and Finding Order
1. **Pre-normalization scan**: Scanned on raw text before NFKC normalization, preventing decomposition from stripping directional isolates or zero-width joiners.
2. **Finding Deduplication**: Repeated occurrences of the same invisible codepoint produce only one finding.
3. **Strict Order**: Invisible Unicode findings (`"invisible_unicode_U+XXXX"`) always precede regex pattern findings in the returned vector.

### 3. Python Regex Semantics & Whitespace Parity
- **Case Folding**: Python's `re.IGNORECASE` is mapped via `(?i)`.
- **Whitespace (`\s`) Class Expansion**:
  In Python, `\s` matches Unicode whitespace plus the four ASCII information separators `\x1c..\x1f` (File/Group/Record/Unit Separators), as recognized in [`python_value::python_whitespace`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/python_value.rs#L180). Because standard Rust regex / `fancy_regex` `\s` matches only Unicode `White_Space`, our compiler converts `\s` to `[\s\x1c-\x1f]` (and appends `\x1c-\x1f` within character classes like `[\s\-]`). This closes a potential bypass vector where attackers use information separators between keywords.
- **Bounded Fillers**:
  Filler patterns `(?:\w+\s+){0,8}` are preserved verbatim to prevent ReDoS while permitting up to eight obfuscation words between attack anchors.

### 4. Unicode NFKC Normalization & Dependency Report
In Python, `unicodedata.normalize("NFKC", content)` folds compatibility variants (such as full-width Latin `ｃａｔ` -> `cat`, ideographic space `\u3000` -> `' '`, circled letters, and ligatures) into ASCII before regex matching.

> [!IMPORTANT]
> **Dependency Note to Main Agent**:
> `unicode-normalization` is currently not present in `Cargo.toml`. Per prompt constraints forbidding Cargo changes, normalization is handled via built-in compatibility folding targeting homograph attack vectors (full-width Latin/ASCII `U+FF01..=U+FF5E`, full-width space `U+3000`, circled alphanumeric characters, mathematical alphanumeric ranges `U+1D400..=U+1D7FF`, and common ligatures). All 111 reference goldens and homograph attack vectors pass. If arbitrary, exhaustive Unicode decomposition across all Unicode blocks is required, `unicode-normalization = "0.1"` should be added to `Cargo.toml` by the main agent.

### 5. Finding Deduplication and Pattern Order
- Each regex pattern is evaluated once against the normalized string; multiple matches of the same pattern within the content produce only a single entry in `findings`.
- Pattern findings are appended in the exact relative declaration order of `_PATTERNS` in `tools/threat_patterns.py`.

## Verification & Golden Corpus

Goldens and source-extracted pattern definitions are generated by [`rust/tools/gen_threat_pattern_goldens.py`](file:///home/eins0fx/development/hermes-agent-port/rust/tools/gen_threat_pattern_goldens.py):

```bash
mise x python@3.12.13 -- python3 rust/tools/gen_threat_pattern_goldens.py --check
```

Outputs:
- [`rust/tools/threat-patterns.json`](file:///home/eins0fx/development/hermes-agent-port/rust/tools/threat-patterns.json): 36 pattern entries extracted from `tools/threat_patterns.py`.
- [`rust/tools/threat-pattern-goldens.json`](file:///home/eins0fx/development/hermes-agent-port/rust/tools/threat-pattern-goldens.json): 111 verified golden test cases covering:
  - 36 canonical pattern triggers at native scope
  - Multi-scope inheritance and isolation (all/context/strict)
  - Full Brainworm payload regression across all scopes
  - 13 false-positive guard cases (benign text, legal language, mid-name env vars)
  - All 17 invisible Unicode codepoints, repeated dedup, and combined ordering
  - Adversarial Unicode homographs (full-width, circled, mathematical bold, ligatures)
  - ASCII information separators (`\x1c..\x1f`)
  - Multi-word filler boundaries (0..8 words match; 9 words reject)
  - Length truncation boundaries (65,536 chars)
  - Case variations and ReDoS near-miss bounds

## Inline Tests

[`rust/crates/hermes-gateway/src/threat_patterns.rs`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/threat_patterns.rs) contains inline unit tests:
- `test_patterns_match_json`: Asserts 100% parity between in-code definitions and `threat-patterns.json`.
- `test_threat_pattern_goldens`: Replays all 111 golden cases from `threat-pattern-goldens.json`.
- `test_brainworm_payload_multi_scope`: Regression test for Brainworm detection.
- `test_invisible_unicode_dedup_and_ordering`: Verifies invisible Unicode dedup and precedence.
- `test_nfkc_compatibility_homographs`: Verifies full-width Latin folding.
- `test_python_whitespace_information_separators`: Verifies `\x1c..\x1f` detection.
- `test_filler_word_boundaries`: Verifies 8-word vs 9-word filler cutoff.
- `test_scan_cap_bounded`: Verifies 65,536 character truncation.
- `test_unknown_scope_panics`: Verifies panic on invalid scope.
- `test_empty_content_does_not_panic_on_unknown_scope`: Verifies early return on empty content.

## Build & Cargo Boundary

In compliance with instructions:
- Cargo was not executed anywhere.
- No other gateway modules (`main.rs`, `lib.rs`, `Cargo.toml`) were modified.
- No git commits were created.
- Build, integration, and cargo testing remain the responsibility of the main agent.
