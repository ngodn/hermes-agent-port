> Maintainer review: this is Gemini's advisory output, not a verified parity claim.
> The secondary-stat recommendation in section 2.2.2 and the final checklist is
> incorrect: Python calls f.stat() again outside its size-read exception handler.
> Rust deliberately preserves that call and its error propagation. Resolving the
> root per symlink also matches Python. Traversal ordering and enumeration errors
> need version-specific evidence; the general claims below do not establish a
> discrepancy. Windows and non-Unicode path support remain explicit gaps.
> scan_skill now includes scan_provenance and passes six complete Python cases.

# Skill Structure Review: `check_structure` and `bundle_paths` Parity

This document reviews the structural verification and path traversal logic in [`rust/crates/hermes-gateway/src/skills_guard.rs`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/skills_guard.rs) ([`check_structure`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/skills_guard.rs#L242-L370) and [`bundle_paths`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/skills_guard.rs#L217-L240)) compared against the Python reference in [`tools/skills_guard.py`](file:///home/eins0fx/development/hermes-agent-port/tools/skills_guard.py) ([`_check_structure`](file:///home/eins0fx/development/hermes-agent-port/tools/skills_guard.py#L1070-L1198)). It also audits the test generator [`rust/tools/gen_skill_structure_goldens.py`](file:///home/eins0fx/development/hermes-agent-port/rust/tools/gen_skill_structure_goldens.py) and fixtures [`rust/tools/skill-structure-goldens.json`](file:///home/eins0fx/development/hermes-agent-port/rust/tools/skill-structure-goldens.json) for missed behavior, and defines concrete requirements for subsequent [`scan_skill`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/skills_guard.rs#L183-L213) and content hash integration.

No source files were modified, no Cargo commands were executed, and authentication was unchanged during this review.

---

## 1. Architectural Overview and Function Signatures

### 1.1 Signature Comparison

| Component | Python Reference ([`tools/skills_guard.py`](file:///home/eins0fx/development/hermes-agent-port/tools/skills_guard.py)) | Rust Port ([`rust/crates/hermes-gateway/src/skills_guard.rs`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/skills_guard.rs)) |
| :--- | :--- | :--- |
| Traversal | Inline call: `skill_dir.rglob("*")` | Helper function: `bundle_paths(directory: &Path) -> Vec<PathBuf>` |
| Structural Check | `_check_structure(skill_dir: Path, ignore=None) -> List[Finding]` | `pub fn check_structure(directory: &Path, ignore: Option<&IgnoreRules>) -> anyhow::Result<Vec<Finding>>` |
| Return Error Mode | Returns empty/partial list or raises unhandled exceptions | Returns `anyhow::Result` with fail-fast propagation on unexpected I/O errors |
| Structural Limits | `MAX_FILE_COUNT = 50`, `MAX_SINGLE_FILE_KB = 256`, `MAX_TOTAL_SIZE_KB = 5120` | Hardcoded numeric literals (`50`, `256 * 1024`, `5120 * 1024`) |

---

## 2. Detailed Discrepancy Analysis

### 2.1 Directory Traversal (`bundle_paths` vs `Path.rglob("*")`)

1. **Traversal Order and Structure**:
   - Python's `Path.rglob("*")` produces a generator yielding entries as it traverses the filesystem.
   - Rust's `bundle_paths` collects directory entries eagerly into an output vector, collecting non-symlink child directories into a temporary list, and then recursively calling `visit` on each child directory.
   - Consequently, `bundle_paths` performs a shallow-first traversal per level: all immediate entries of a directory appear before the contents of any child directory. Python's traversal order depends on interpreter internals and underlying filesystem directory order.
   - Neither implementation sorts paths at enumeration time. Because findings are appended in traversal order, raw finding output order differs between Python and Rust unless externally sorted.

2. **Directory Symlinks**:
   - In Python, `Path.rglob("*")` emits directory symlinks as entries, but does not descend into them unless configured to follow symlinks.
   - In Rust, `path.is_dir() && !path.is_symlink()` prevents directory symlinks from being added to `children`. The directory symlink is added to `output`, but never recursed into. Both implementations correctly avoid infinite recursion on directory symlinks.

3. **Error Handling on Directory Enumeration**:
   - In Rust `bundle_paths`:
     ```rust
     let Ok(entries) = std::fs::read_dir(directory) else { return; };
     let Ok(entries) = entries.collect::<std::io::Result<Vec<_>>>() else { return; };
     ```
     If any single entry in `read_dir` fails with an I/O error (such as an unreadable directory entry or transient permission issue), `entries.collect()` fails, and `bundle_paths` silently discards the entire directory and all sibling entries.
   - In Python, `rglob` will either raise an exception or skip individual inaccessible files depending on the Python minor version and `on_error` configuration.

4. **Plain Directories in Output Vector**:
   - Rust's `bundle_paths` pushes plain directory paths into `output`.
   - `check_structure` filters them out via `if !file.is_file() && !file.is_symlink() { continue; }`.
   - Python does the same check inside the loop (`if not f.is_file() and not f.is_symlink(): continue`).
   - While functionally equivalent, passing directories through `output` in Rust creates unnecessary vector allocations and metadata calls in `check_structure`.

---

### 2.2 Structural Verification (`check_structure` vs `_check_structure`)

#### 2.2.1 Symlink Evaluation and Circular Loop Resolution

1. **Path Resolution Engine**:
   - Python uses `f.resolve()`, which defaults to non-strict evaluation (`strict=False`).
   - Rust uses `resolve(&file)` which invokes `crate::file_read_safety::realpath_abs`. This is a faithful port of CPython's `posixpath._joinrealpath(strict=False)`.

2. **Containment Verification**:
   - Python checks: `not resolved.is_relative_to(skill_dir.resolve())`.
   - Rust checks: `!target.starts_with(&root)` where `root` is `resolve(directory)`.
   - Because Rust's `Path::starts_with` operates on whole path components, prefix-confusion attacks (e.g. `axolotl` vs `axolotl-backdoor`) are correctly blocked in both.
   - Inefficiency in Rust: `resolve(directory)` is invoked repeatedly inside the loop for every symlink found in the bundle, instead of resolving the root directory once prior to the loop.

3. **Circular Symlinks and Exception Divergence**:
   - Python reference behavior:
     ```python
     try:
         resolved = f.resolve()
         if not resolved.is_relative_to(skill_dir.resolve()):
             findings.append(Finding(pattern_id="symlink_escape", ...))
     except OSError:
         findings.append(Finding(pattern_id="broken_symlink", ...))
     ```
     In Python (both 3.11 and 3.12), `pathlib.Path.resolve()` on a circular symlink (e.g. `link -> link`) raises `RuntimeError("Symlink loop from ...")`, not `OSError`. Because `RuntimeError` does not inherit from `OSError`, Python crashes with an unhandled exception.
   - Rust implementation behavior:
     ```rust
     Err(error) if error.to_string() == "symlink loop" => return Err(error.into()),
     Err(_) => add("broken_symlink", "medium", "traversal", ...),
     ```
     Rust's `realpath_abs` returns `io::Error::other("symlink loop")`. Rust's `check_structure` intercepts this specific error and propagates it upward as `anyhow::Result::Err`.
   - Discrepancy and Design Intent: The author of `_check_structure` wrote `description="broken or circular symlink"`, showing an intent to capture circular links as a finding. However, Python standard library mechanics caused `RuntimeError` instead. Rust preserved the Python crash behavior by returning `Err`. However, when integrated into skill loading, returning `Err` aborts admission entirely rather than generating a structured finding.

4. **Broken Symlinks with Missing Targets**:
   - Python non-strict `Path.resolve()` does not raise `OSError` when the target file does not exist.
   - If the missing target is inside the skill directory (e.g. `link -> missing.txt`), Python resolves the path within `skill_dir`, `is_relative_to` returns `True`, and zero findings are emitted.
   - If the missing target is outside the skill directory (e.g. `link -> ../missing.txt`), Python resolves the path outside `skill_dir`, and emits `symlink_escape` (critical).
   - Rust behaves identically: `realpath_abs` returns `Ok` for nonexistent targets, so internal broken links produce no findings and external broken links produce `symlink_escape`.
   - Consequence: The `broken_symlink` finding pattern is effectively dead code in both Python and Rust for broken symlinks. It can only be triggered in Rust if `realpath_abs` produces a non-loop I/O error.

---

#### 2.2.2 Secondary Stat Call and Error Propagation

1. **Redundant Secondary Metadata Call**:
   - In `check_structure` lines 269 and 306:
     ```rust
     // Line 269: First stat call
     let Ok(metadata) = std::fs::metadata(&file) else {
         continue;
     };
     let size = metadata.len();
     total += size;
     ...
     // Line 306: Second stat call for executable check
     if ![".sh", ".bash", ".py", ".rb", ".pl"].contains(&extension.as_str()) {
         let metadata = std::fs::metadata(&file)?;
         #[cfg(unix)]
         {
             use std::os::unix::fs::PermissionsExt;
             if metadata.permissions().mode() & 0o111 != 0 { ... }
         }
     }
     ```
   - In Python, `f.stat().st_size` is queried inside a try-except block. If stat fails, it continues. The permission check `f.stat().st_mode` reuses the stat result and never raises an unhandled error.
   - In Rust, `metadata` is already in scope from line 269. Calling `std::fs::metadata(&file)?` a second time at line 306 is redundant I/O. Furthermore, using `?` causes `check_structure` to abort with an error if the file was unlinked or modified between the two calls, diverging from Python's best-effort `continue`.

2. **Platform Specific Permissions**:
   - Rust uses `#[cfg(unix)]` to query `metadata.permissions().mode() & 0o111 != 0`. On non-Unix systems (e.g. Windows), the block compiles to `let _ = metadata;` and never flags `unexpected_executable`.
   - In Python, `st_mode & 0o111` is evaluated regardless of platform, though on Windows `st_mode` executable bits are synthesized by Python.

---

#### 2.2.3 Path Sanitization and Windows Portability

1. **Path Separator Handling in IgnoreRules**:
   - In Rust `check_structure`:
     ```rust
     let relative = file.strip_prefix(directory)?.to_str()
         .ok_or_else(|| anyhow::anyhow!("non-Unicode skill path"))?;
     if ignore.is_some_and(|ignore| ignore.ignores(relative)) {
         continue;
     }
     ```
   - On Windows, `strip_prefix` produces native path separators (`\`).
   - In `IgnoreRules::ignores`:
     ```rust
     let path = relative.split('/').filter(|part| !part.is_empty() && *part != ".").collect::<Vec<_>>().join("/");
     ```
     Rust splits only on forward slashes (`/`). On Windows, `relative` with backslashes is not split, leaving backslashes intact. Consequently, directory matching patterns like `docs/` or glob segment matching fail on Windows.
   - Python reference handles this explicitly in `_load_skill_ignore` by converting all paths: `rel_posix = Path(rel).as_posix()`.

2. **Non-Unicode Paths**:
   - Rust errors out immediately if a path is not valid UTF-8 (`non-Unicode skill path`).
   - Python uses PEP 383 surrogate escaping on POSIX filesystems, allowing non-UTF-8 paths to be inspected without throwing immediate decode errors during directory iteration.

---

#### 2.2.4 File Extension Matching Edge Cases

1. **Extension Extraction**:
   - Rust:
     ```rust
     let extension = file.extension()
         .and_then(|s| s.to_str())
         .map(|s| format!(".{}", s.to_lowercase()))
         .unwrap_or_default();
     ```
   - Python: `ext = f.suffix.lower()`.
   - Behavior for files starting with a dot (e.g. `.env`, `.exe`, `.bashrc`):
     * Rust `Path::extension()` returns `None` for a file named `.exe`. The formatted extension is `""`.
     * Python `Path(".exe").suffix` returns `""`.
     * Both implementations treat `.exe` as having no extension, so neither flags `.exe` under `binary_file`. However, if `.exe` has executable permissions, both flag it under `unexpected_executable`.
   - Behavior for trailing dots (e.g. `file.`):
     * Rust `Path::extension()` returns `Some("")`, producing `"."`.
     * Python `Path("file.").suffix` returns `""`.

---

## 3. Structural Generator and Fixtures Audit

The generator [`rust/tools/gen_skill_structure_goldens.py`](file:///home/eins0fx/development/hermes-agent-port/rust/tools/gen_skill_structure_goldens.py) and fixtures [`rust/tools/skill-structure-goldens.json`](file:///home/eins0fx/development/hermes-agent-port/rust/tools/skill-structure-goldens.json) contain 59 test cases across 9 categories. The following missed behaviors and testing gaps were identified:

### 3.1 Complete Omission of Circular Symlink Fixtures
- The generator does not include any test case for circular symlinks (e.g. `a -> a` or `a -> b -> a`).
- Cause: If `gen_skill_structure_goldens.py` created a circular symlink, Python's `sg._check_structure` crashed with `RuntimeError` during oracle execution. The fixture author omitted circular symlinks from the generator to allow golden generation to complete.
- Impact: In [`rust/crates/hermes-gateway/src/skills_guard.rs`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/skills_guard.rs#L641-L651), circular link verification had to be separated into an ad-hoc unit test (`structure_propagates_symlink_loops_like_python_312`).

### 3.2 Zero Fixtures for `broken_symlink` Finding
- Across all 59 generated test cases, `pattern_id: "broken_symlink"` appears 0 times in `expected_findings`.
- All 4 test cases in Group 4 (`symlink_broken`):
  * `symlink-broken-target-inside` (target: `missing_file.txt`): yields 0 findings because non-strict resolution succeeds lexically within the skill root.
  * `symlink-broken-target-outside` (target: `../missing_outside.txt`): yields `symlink_escape` (critical).
  * `symlink-broken-nested-outside` (target: `../../missing_root.txt`): yields `symlink_escape` (critical).
  * `symlink-broken-through-file` (target: `regular.txt/subfile`): yields 0 findings because lexical resolution resolves within the skill root.
- The `broken_symlink` pattern is unexercised in golden fixtures because Python non-strict resolution never reaches the `except OSError` branch for these cases.

### 3.3 Artificial Normalization of Finding Order
- The generator sorts all findings using `finding_sort_key` (by file path alphabetically, then by rule priority, then directory-level findings).
- In reality, neither Python `_check_structure` nor Rust `check_structure` sorts findings internally. Both emit findings in raw filesystem traversal order.
- In the Rust unit test `structure_findings_match_actual_python`, Rust must manually sort actual findings before comparing with `expected_findings`. This hides real traversal order discrepancies between Python and Rust.

### 3.4 Missing Escaping Directory Symlink Fixtures
- Group 3 includes `symlink-internal-valid-dir` (`bin -> scripts/sub`), but there is no fixture for an unignored directory symlink escaping to a parent directory (e.g. `escape_dir -> ../outside`).
- Neither implementation descends into directory symlinks to scan files, but tests should assert that an escaping directory symlink is flagged as `symlink_escape` without scanning external contents.

### 3.5 Exact Byte Boundary Tests
- The generator tests:
  * Single file: exact 256KB (allowed), 257KB (flagged). It does not test the exact threshold boundary of `256 * 1024 + 1` bytes (262,145 bytes).
  * Total size: exact 5120KB (allowed), 5250KB (flagged). It does not test `5120 * 1024 + 1` bytes.
  * At `256 * 1024 + 1` bytes, integer division `size / 1024` evaluates to `256KB`, generating the message: `"file is 256KB (limit: 256KB)"`. Verifying this boundary string is essential for strict oracle matching.

### 3.6 Windows Path Separators and Unreadable Files
- As noted in `EXCLUSIONS_DEFINITION` in the generator, non-UTF8 paths and ACLs are excluded.
- The generator runs only POSIX-style paths with forward slashes. It does not test how ignore rules handle native Windows backslash paths.
- The generator does not test unreadable files (mode `0o000`) or transient file deletion between the size stat and permissions stat.

---

## 4. Next `scan_skill` and Hashing Integration Requirements

To complete the port of `skills_guard` and connect it with project skills quarantine, the following concrete components must be integrated:

```
+-----------------------------------------------------------------------------------+
|                                  scan_skill                                       |
|                                                                                   |
|  +---------------------------+             +-----------------------------------+  |
|  |      check_structure      |             |         scan_file loop            |  |
|  |  (file count, sizes,      |             |  (threat patterns, docstrings,    |  |
|  |   symlink escapes,        |             |   invisible unicode characters)   |  |
|  |   binaries, permissions)  |             +-----------------------------------+  |
|  +---------------------------+                               |                    |
|                \                                            /                     |
|                 +---------------------+--------------------+                      |
|                                       |                                           |
|                                       v                                           |
|                             Aggregated Findings                                   |
|                                       |                                           |
|                                       v                                           |
|                              verdict & summary                                    |
+-----------------------------------------------------------------------------------+
                                        |
       +--------------------------------+-------------------------------+
       |                                                                |
       v                                                                v
+-----------------------------+                        +--------------------------------+
|      content_hash /         |                        |       scan_skill_cached        |
|     full_content_hash       |                        |   (persists ScanResult with    |
|  (sorted POSIX paths +      |                        |    provenance into disk JSON   |
|   null byte + file bytes)   |                        |    cache under .scan-cache)    |
+-----------------------------+                        +--------------------------------+
```

### 4.1 `scan_skill` Refactoring and Optimization

1. **Avoid Double Traversal**:
   - Currently, `scan_skill` in [`skills_guard.rs`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/skills_guard.rs#L183-L200) calls `bundle_paths(path)` twice: once inside `check_structure`, and a second time in the text scanning loop.
   - Requirement: `bundle_paths` should be run once, or `check_structure` and `scan_file` should consume the same pre-filtered file list to eliminate duplicate directory reads and stat operations.

2. **`ScanResult` Structure Parity**:
   - Python's `ScanResult` includes a `scan_provenance: dict` field.
   - Rust's current `ScanResult` in `skills_guard.rs` lacks `scan_provenance`.
   - Requirement: Add `pub scan_provenance: serde_json::Value` (or a dedicated struct) to `ScanResult` so cached scan results and disk attestations can round-trip cleanly without loss of provenance metadata.

3. **Handling Escaping Symlinks During Scan**:
   - When a skill contains an escaping symlink pointing to a regular file (e.g. `leak -> /etc/hosts`):
     * In `check_structure`, the symlink is flagged as `symlink_escape` (critical).
     * In `scan_skill`, `file.is_file()` follows symlinks and returns `true`. If not ignored, `scan_file` reads the target file and performs regex pattern matching.
   - This matches Python behavior, but must be guarded to ensure that reading through symlinks cannot trigger unbounded reads on special devices or hang on FIFOs.

---

### 4.2 Content Digest and Hashing Algorithms

Python defines two hash functions in [`tools/skills_guard.py`](file:///home/eins0fx/development/hermes-agent-port/tools/skills_guard.py#L884-L914):
- `_content_digest(skill_path: Path) -> str`: canonical SHA-256 computation over relative paths and file content.
- `content_hash(skill_path: Path) -> str`: returns `f"sha256:{_content_digest(skill_path)[:16]}"`.
- `full_content_hash(skill_path: Path) -> str`: returns `f"sha256:{_content_digest(skill_path)}"`.

These functions must be strictly byte-symmetric with `bundle_content_hash` in [`tools/skills_hub.py`](file:///home/eins0fx/development/hermes-agent-port/tools/skills_hub.py#L4395-L4422).

#### Concrete Specification for Rust Implementation:
1. **Directory Hashing**:
   - Walk the skill directory for all entries where `file_path.is_file()` is true (following file symlinks, excluding directories and broken symlinks).
   - Convert every relative path to a POSIX path string using `/` separators (`as_posix()`), even on Windows.
   - Sort entries strictly in ASCII ascending order by the POSIX relative path string.
   - For each entry in sorted order:
     1. Feed `rel_posix_path.as_bytes()` to the SHA-256 hasher.
     2. Feed byte `0x00` (`\0`) to the SHA-256 hasher.
     3. Feed the full raw bytes of the file to the SHA-256 hasher.
   - Crucial detail: Ignore rules (`.skillignore`) are **not** applied during content hashing. All files present in the bundle contribute to the digest.

2. **Single-File Hashing**:
   - If `skill_path` is a regular file, feed its raw bytes directly to the SHA-256 hasher without path prefix or null delimiter.

3. **Output Formats**:
   - `content_hash`: `"sha256:"` prefix followed by the first 16 lowercase hex characters.
   - `full_content_hash`: `"sha256:"` prefix followed by all 64 lowercase hex characters.

---

### 4.3 Cached Attestation Integration (`scan_skill_cached`)

In Python [`tools/skills_guard.py`](file:///home/eins0fx/development/hermes-agent-port/tools/skills_guard.py#L922-L969), `scan_skill_cached` binds scan results to exact disk content:

1. **Cache Location and Identity**:
   - Cache directory defaults to `skill_path.parent / ".scan-cache"` (or custom directory like `~/.hermes/cache/project_skill_scans`).
   - `bundle_hash`: full 64-hex SHA-256 from `full_content_hash`.
   - `source_identity`: first 16 hex characters of `sha256("{source}\0{source_url}")`.
   - Cache file name: `<bundle_hash_without_sha256_prefix>-<source_identity>.json`.

2. **Validation and Invalidation**:
   - A cache entry is valid only if:
     * `bundle_hash == cached["bundle_hash"]`
     * `scanner_version == "skills-guard-v2"`
     * `source == cached["source"]`
     * `source_url == cached["source_url"]`
   - Cache hit: Reconstructs `ScanResult` with `scan_provenance["fresh"] = false`.
   - Cache miss: Executes full `scan_skill`, builds provenance JSON with `fresh = true`, and writes JSON atomically to disk.

---

### 4.4 Project Skills Quarantine Integration Gate

When connecting `skills_guard` to project skills loading ([`agent/skill_utils.py:is_quarantined_project_skill`](file:///home/eins0fx/development/hermes-agent-port/rust/analysis/project-skills-integration-map.md)):

1. **Quarantine Source Identity**:
   - Source is `"project-local"`, which maps to trust level `"community"` via [`trust_level()`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/skills_guard.rs#L502-L528).

2. **Verdict Decision Gate**:
   - Only verdict `"dangerous"` triggers quarantine.
   - Verdicts `"safe"` and `"caution"` are permitted to load without quarantine.

3. **Fail-Closed Guarantee**:
   - Any unhandled error during scanning (e.g. symlink loop error propagation or corrupted file read) must result in `quarantined = true`.
   - The scanner must never allow an unscanned or error-yielding skill to bypass quarantine.

---

## 5. Summary Checklist of Next Steps

- [ ] Clean up `check_structure`:
  - Cache `resolve(directory)` outside the file iteration loop instead of re-evaluating it for every symlink.
  - Remove redundant secondary `std::fs::metadata(&file)?` at line 306, reusing the existing `metadata` from line 269.
  - Normalize Windows path separators to forward slashes before evaluating `IgnoreRules`.
- [ ] Implement Canonical Hashing in `skills_guard.rs`:
  - Implement `_content_digest` with sorted POSIX relative paths and null-byte separators.
  - Expose `content_hash` (`sha256:<16-hex>`) and `full_content_hash` (`sha256:<64-hex>`).
- [ ] Implement `scan_skill_cached`:
  - Add `scan_provenance` to `ScanResult`.
  - Implement cache read, validation, and JSON write under `.scan-cache`.
- [ ] Wire Quarantine into Skill Discovery:
  - Connect `is_quarantined_project_skill` gate into the project skills loader.
  - Enforce fail-closed quarantine on `"dangerous"` verdict or scanner errors.
