# Project Skills Discovery and Quarantine Integration Map

Follow-up: the missing-component inventory below reflects the helper's earlier
read. skill_discovery::project_root and project_dirs have since been added and
tested. Production cwd/config expansion and scanner integration remain open;
use PORT.md and current call sites for current status.

## 1. Python Discovery and Trust Normalization (`agent/skill_utils.py`)
- `find_project_root(start=None)`: Traverses ancestor directories up to `_PROJECT_ROOT_MAX_DEPTH = 64` steps looking for `.git` (dir or file). Resolves `start` via `scope_terminal_cwd()` (`TERMINAL_CWD`), falling back to `Path.cwd()`. Returns `None` if `cur == Path.home().resolve()` to avoid treating dotfiles home checkouts as projects.
- `_project_trusted_dirs_from_config()`: Reads `skills.trusted_project_dirs` from raw config. Normalizes single string to list; strips whitespace; expands env vars (`os.path.expandvars`) and tilde (`os.path.expanduser`); resolves canonical paths (`Path.resolve()`).
- `is_project_root_trusted(root)`: Checks canonical resolved root membership in trusted set.
- `_candidate_project_skills_dirs(root)`: Tests `root / sub` for `PROJECT_SKILLS_SUBDIRS` (`.hermes/skills`, `.agents/skills`). Skips directories resolving to `local_skills` (`get_skills_dir().resolve()`) to prevent duplicate indexing when `HERMES_HOME` is inside the checkout.
- `get_project_skills_dirs()`: Returns empty list if `skills.project_discovery` is `false` (default: enabled), if not in a git checkout (`root is None`), or if root is untrusted.
- `get_untrusted_project_skills_root()`: Returns `(root, count)` to notify CLI users about `hermes skills trust` when untrusted candidate directories contain `SKILL.md` files.
- `iter_project_skill_files(project_dir)`: Iterates `iter_skill_index_files(project_dir, "SKILL.md")` and filters out entries where `is_quarantined_project_skill(skill_md)` is true.

## 2. Quarantine Gate and Cache Contract (`agent/skill_utils.py`)
- `is_quarantined_project_skill(skill_md)`: Fail-closed gate. Evaluates parent directory of `SKILL.md`.
  - Source identity: Passes `source = "project-local"` (`_PROJECT_SCAN_SOURCE`).
  - Cache dir: `~/.hermes/cache/project_skill_scans` (`_project_scan_cache_dir()`).
  - Process memory cache: `_PROJECT_QUARANTINE_CACHE` keyed by resolved skill dir path string.
  - Verdict gating: Quarantined if `result.verdict == "dangerous"`. Verdicts `"safe"` and `"caution"` pass without quarantine. Any scanner error or exception sets `quarantined = True` (fail-closed).

## 3. Scanner Stages and Limits (`tools/skills_guard.py`)
- **Trust Level Resolution** (`_resolve_trust_level`): `source = "project-local"` does not match builtins or `TRUSTED_REPOS` (`openai/skills`, `anthropics/skills`, `huggingface/skills`, `NVIDIA/skills`), resolving to trust level `"community"`.
- **Stage 1: Ignore Rules** (`_load_skill_ignore`): Reads `.skillignore` or `.clawhubignore`. Evaluates gitignore-style globs (`fnmatch`), comments, directory prefixes. Always ignores ignore files; `SKILL.md` is strictly never ignorable.
- **Stage 2: Structural Verification** (`_check_structure`):
  - `symlink_escape` (critical): Symlink target resolves outside skill directory.
  - `broken_symlink` (medium): Target missing or circular.
  - `binary_file` (critical): Extension in `SUSPICIOUS_BINARY_EXTENSIONS` (`.exe`, `.dll`, `.so`, `.dylib`, `.bin`, `.dat`, `.com`, `.msi`, `.dmg`, `.app`, `.deb`, `.rpm`).
  - `unexpected_executable` (medium): Executable mode bit (`0o111`) on non-script files (`not in {'.sh', '.bash', '.py', '.rb', '.pl'}`).
  - File count cap: `MAX_FILE_COUNT = 50` (`too_many_files`, medium).
  - Single file size cap: `MAX_SINGLE_FILE_KB = 256` (`oversized_file`, medium).
  - Total directory size: `MAX_TOTAL_SIZE_KB = 5120` (`oversized_skill`, low, informational only).
- **Stage 3: Text and Threat Analysis** (`scan_file`):
  - Scans text files matching `SCANNABLE_EXTENSIONS` (24 extensions) or named `SKILL.md`.
  - `_compute_docstring_lines`: State machine tracking `"""` and `'''` to exempt docstring blocks from false-positive code pattern matches.
  - `THREAT_PATTERNS`: Regex patterns detecting exfiltration (credentials, env vars), shell write mechanics (`_shell_write_re`), prose modification directives (`_prose_modify_re`), content contracts (`_content_contract_re`), and destructive commands.
  - `INVISIBLE_CHARS`: Detects 17 zero-width and directional unicode characters (`invisible_unicode`, high).
- **Stage 4: Verdict Determination** (`_determine_verdict`): Critical finding produces `"dangerous"`; high produces `"caution"`; medium/low alone produce `"safe"`.
- **Hashing and Attestation Cache** (`scan_skill_cached`, `full_content_hash`, `_content_digest`):
  - Digest computes canonical SHA-256 over relative POSIX paths sorted case-sensitively and file bytes. Format: `sha256:<hex>`.
  - Attestation stored in JSON: `<cache_dir>/<bundle_hash_hex>-<source_identity_16hex>.json`. Validated against `bundle_hash`, `scanner_version` (`skills-guard-v2`), `source`, and `source_url`.

## 4. Existing Rust Support vs Missing Components
- **Existing in Rust Port**:
  - `rust/crates/hermes-gateway/src/skill_loader.rs`: Implements `merge_profile` and accepts pre-computed project entries in test harnesses (`tests::profile_merge_and_render_match_python_with_preaccepted_edges`).
  - `rust/crates/hermes-gateway/src/skill_discovery.rs`: Implements directory traversal (`walk`, `index_files`) and frontmatter parsing for profile and org tiers.
  - `rust/crates/hermes-gateway/src/threat_patterns.rs`: Ports `tools/threat_patterns.py` for tool result and context threat detection, not skill bundle security.
- **Missing in Rust Port**:
  1. Git root discovery: No Rust implementation of `find_project_root` with depth 64 bound, `TERMINAL_CWD` scoping, and home directory exclusion.
  2. Trust configuration normalization: No parser for `skills.trusted_project_dirs` performing environment expansion, user path expansion, and canonical path resolution.
  3. Candidate directory resolution: No logic discovering `.hermes/skills` and `.agents/skills` or enforcing `skills.project_discovery` and local skills exclusion.
  4. Project skill iteration and quarantine hook: No Rust pipeline calling `iter_project_skill_files` before index merging.
  5. Skills guard scanner: No Rust implementation of `tools/skills_guard.py` (ignore rules, structural verification, docstring-aware regex patterns, invisible unicode, verdict aggregation).
  6. Scan cache and integrity digest: No canonical POSIX path SHA-256 digest calculation (`full_content_hash`) or disk attestation caching under `cache/project_skill_scans`.

## 5. Parity and Security Boundaries
- Parity requires both git-scoped discovery and the complete 4-stage scanner pipeline.
- A scanner stub (always-allow) fails security parity: it allows git-pulled repo updates to inject unverified malicious skills into an already-trusted checkout.
- An always-quarantine stub fails functionality parity: it suppresses all valid project-local skills in trusted repositories.
- Quarantine must evaluate to dangerous verdicts only, allowing caution and safe skills to load while failing closed on any scanner error.
