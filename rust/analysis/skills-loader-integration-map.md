# Skills Loader Integration Map

## 1. Exact Execution Order (Load, Scan, Filter, Merge, Render, Cache)
1. **Platform Hint and Disabled Config**:
   - `agent/prompt_builder.py:_current_session_platform_hint` resolves `HERMES_PLATFORM` or `HERMES_SESSION_PLATFORM`.
   - `agent/skill_utils.py:get_disabled_skill_names` (`rust/crates/hermes-gateway/src/skills_index.rs:disabled_names`) reads `skills.disabled` and `skills.platform_disabled.<platform>` from raw config, stripping `ESSENTIAL_SKILLS` (`hermes-agent`).
2. **Layer 1 LRU Cache Probe**:
   - Keyed on 8-tuple `(skills_dir, external_dirs, project_dirs, available_tools, available_toolsets, platform_hint, disabled, compact_categories)` in `agent/prompt_builder.py:_SKILLS_PROMPT_CACHE` (cap 32). On hit, returns cached string.
3. **Layer 2 Disk Snapshot Probe**:
   - `agent/prompt_builder.py:_load_skills_snapshot` (`rust/crates/hermes-gateway/src/skill_discovery.rs:load_snapshot`) reads `<home>/.skills_prompt_snapshot.json`.
   - Validates version 2 and manifest equality against `agent/prompt_builder.py:_build_skills_manifest` (`skill_discovery.rs:manifest`).
4. **Local / Profile Tier Ingestion**:
   - Snapshot hit: Iterates pre-parsed entries, checks `skill_matches_platform_list` (`skills_index.rs:matches_platform`), checks disabled set, and checks `_skill_should_show` (`skills_index.rs:should_show`).
   - Snapshot miss: `agent/skill_utils.py:iter_skill_index_files` (`skill_discovery.rs:walk`) finds `SKILL.md` files. `_parse_skill_file` reads files, calls `agent/skill_utils.py:parse_frontmatter` (strips BOM, YAML/key-value fallback), evaluates `skill_matches_platform` and `skill_matches_environment`, and extracts description. Builds entries via `_build_snapshot_entry` (`skill_discovery.rs:snapshot_entry`), tests disabled names and `_skill_should_show`, and collects `visible_entries`.
5. **Project-Local Tier Scan**:
   - Resolves git root via `agent/skill_utils.py:find_project_root`, validates `agent/skill_utils.py:is_project_root_trusted`, and lists candidate dirs via `agent/skill_utils.py:get_project_skills_dirs`.
   - Walks candidates via `agent/skill_utils.py:iter_project_skill_files`, enforcing `agent/skill_utils.py:is_quarantined_project_skill` (fail-closed scan via `skills_guard`).
   - Parses `SKILL.md`, checks disabled list and `_skill_should_show`. First-seen frontmatter name wins. Adds entries with `[project]` prefix.
6. **Project Shadowing Pass**:
   - Filters `visible_entries` to remove any profile-local or org entry whose name was claimed by project-local tier before collision analysis.
7. **Org Mirror Labeling and Collision Pass**:
   - Gated by `agent/skill_utils.py:read_active_org_id` (`skill_discovery.rs:active_org`).
   - Identifies multi-owner names between personal and org entries in `visible_entries`.
   - Org entries receive `org:<org_id>` category and `[org-shared: by <author>]` tag (author from `.org-provenance.json`).
   - If personal and org entries collide, both receive `[name collision - also exists personally/in your org; load via category path]`.
8. **Snapshot Cold Write**:
   - On snapshot miss, scans `DESCRIPTION.md` via `iter_skill_index_files` for category headers, then atomically writes `<home>/.skills_prompt_snapshot.json` via `_write_skills_snapshot` (`skill_discovery.rs:write_snapshot`).
9. **External Directories Ingestion**:
   - Iterates `skills.external_dirs` via `iter_skill_index_files`. Skips names already in `seen_skill_names` (deduping against project, local, and org), checks disabled list and `_skill_should_show`, and ingests external `DESCRIPTION.md`.
10. **Focus Mode Category Demotion**:
    - Demotes categories matching `compact_categories` to `  <category> [names only]: <names>` and appends the compact footer note.
11. **Index Assembly and Markdown Rendering**:
    - Formats `<available_skills>` markdown block via `skills_index.rs:Index::render`. Replaces `web_search or terminal` with `terminal` when `web_search` is not in `available_tools`.
12. **Layer 1 LRU Cache Insertion**:
    - Stores output under the 8-tuple key in `_SKILLS_PROMPT_CACHE`.

## 2. Precedence Hierarchy and Collision Semantics
1. **Project-Local (Tier 1, Highest)**: `.hermes/skills/`, `.agents/skills/`. Tagged `[project]`. Shadows profile-local and org skills before org collision checks, suppressing false collision warnings on intentional project overrides.
2. **Profile-Local (Tier 2)**: `<home>/skills/<category>/<name>`. Uses category grouping from directory hierarchy. Shadowed by project tier.
3. **Org-Shared (Tier 2, Peer to Local)**: `<home>/skills/_org/<org_id>/...` gated by `.active_org`. Grouped under `org:<org_id>`. Shadowed by project tier.
4. **Personal vs Org Collision**: Fail-loud. Neither entry is suppressed. Both receive `[name collision ...]` prefixes. Runtime `skill_view` rejects bare ambiguous names.
5. **External Directories (Tier 3, Lowest)**: Paths from `skills.external_dirs`. Suppressed if skill name exists in project, local, or org tiers. Never cached in disk snapshot.

## 3. Malformed Snapshot and Frontmatter Behavior
- **UTF-8 BOM**: `parse_frontmatter` strips a single leading `\ufeff`. If preserved, the opening fence triple-dash fails to match.
- **Malformed YAML**: `parse_frontmatter` falls back to line-by-line key:value splitting. Non-dict metadata defaults to empty conditions via `agent/skill_utils.py:extract_skill_conditions` (`skills_index.rs:extract_conditions`).
- **Unreadable SKILL.md**: `_parse_skill_file` catches all exceptions, logs a warning, and returns `(True, {}, "")` (fail-open: skill remains visible in index without description).
- **Snapshot Corruption / Invalidation**: `_load_skills_snapshot` (`skill_discovery.rs:load_snapshot`) discards missing file, non-UTF8 text, malformed JSON, version mismatch (!= 2), or manifest mismatch, returning `None` to trigger a cold scan.
- **Active Org Marker**: Invalid UTF-8 in `_org/.active_org` propagates an `InvalidData` I/O error (`skill_discovery.rs:active_org`).
- **Snapshot Write Failure**: `_write_skills_snapshot` (`skill_discovery.rs:write_snapshot`) catches I/O errors and logs debug output (best-effort write; preserves symlink on managed profile).
- **Condition / Filter Parsing Errors**: `skills_index.rs:should_show` and `disabled_names_raw` return `Err` if types are unhashable or non-iterable. Unknown platform tags fail closed; unknown environment tags fail open.
- **Project Scan Failure**: `is_quarantined_project_skill` treats scanner crash or `dangerous` verdict as quarantined (fail-closed).

## 4. Concrete Missing Consumers in Rust Port
1. **Pipeline Loader Orchestrator**: No Rust function coordinates `skill_discovery.rs` (`load_snapshot`, `walk`, `snapshot_entry`, `write_snapshot`) with `skills_index.rs` (`disabled_names`, `matches_platform`, `matches_environment`, `should_show`, `Index`). Test harnesses build `Index` structs manually.
2. **System Prompt Wire-up**: `rust/crates/hermes-gateway/src/system_prompt.rs:ResolvedPromptSections::set_skills_index` gates the block on skills tools, but neither `PromptAssembler` nor session initialization builds or passes `Index`.
3. **Layer 1 LRU Cache**: No thread-safe in-memory cache (`_SKILLS_PROMPT_CACHE` counterpart, cap 32) exists in Rust.
4. **Project Skills Discovery and Quarantine**: No Rust caller implements git root traversal (`find_project_root`), trust check (`is_project_root_trusted`), or security scan quarantine (`is_quarantined_project_skill`).
5. **External Dirs Resolver**: No consumer resolves and expands `skills.external_dirs` from `config.yaml` to feed discovery.
6. **Help Guidance Cross-Coupling**: `rust/crates/hermes-gateway/src/system_prompt.rs:StableGuidance::stable_guidance` checks `skills_index.contains("- hermes-agent:")`, but no production pipeline produces `skills_index` before finalizing stable guidance.
7. **Cache Invalidation Hooks**: No Rust counterpart for `agent/prompt_builder.py:clear_skills_system_prompt_cache(clear_snapshot=...)` exists for mutation routes (`skill_manage`, hub install, session reset).
