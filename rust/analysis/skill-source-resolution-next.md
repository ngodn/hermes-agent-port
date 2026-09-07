> Follow-up: profile_extra_dirs now resolves creation/external roots from an
> explicit config value and expansion callback. Its real-files test and a direct
> Python get_all_skills_dirs config probe pass. Config mtime caching and concrete
> captured expansion remain pending. Treat this helper report as advisory:
> do not add the suggested ambient-environment fallback without source evidence.
> External arrays also accept non-string JSON entries through Python str().

# Skill Source Resolution and Prompt Assembly Connection Map

**Target Document:** `rust/analysis/skill-source-resolution-next.md`
**Scope:** Concrete missing captured config, path expansion, and orchestrator steps required to connect `skill_loader::PromptLoader` to native prompt assembly in `system_prompt.rs`.
**Mode:** Review only. No source files modified. No Cargo commands executed. Authentication unchanged. No em dashes used.

---

## 1. Executive Summary

The Rust gateway crate provides core components for skills evaluation:
1. `skill_loader::PromptLoader` manages an in-memory 32-entry LRU cache, owns `ProjectAdmission` quarantine decisions, and orchestrates cold/warm index generation via `load_index`.
2. `skill_discovery.rs` implements filesystem index walks, frontmatter parsing, snapshot persistence, and project candidate discovery via `project_root` and `project_dirs`.
3. `skills_index.rs` implements platform/environment compatibility filters, tool/toolset condition matching, disabled skill exclusions, and markdown index formatting.
4. `system_prompt.rs` provides `ResolvedPromptSections` and `StableGuidance`, which structure the system prompt across stable, context, and volatile tiers.

However, an execution gap remains between raw configuration and `PromptLoader::render`. No production pipeline in Rust resolves external skill directories (`skills.external_dirs` and `skills.create_dir`), performs profile-relative path expansion against session-captured environments, wires `runtime_cwd::CwdInputs` into project discovery, or connects the rendered output into `system_prompt.rs`.

This document identifies the concrete missing config extraction, path expansion, and lifecycle steps required to complete native prompt assembly.

---

## 2. Reference Architecture and Symbol Mapping

The table below maps the Python reference functions to their Rust counterparts and highlights current port status.

| Stage | Python Reference (`agent/`) | Rust Port (`rust/crates/hermes-gateway/src/`) | Parity Status |
| :--- | :--- | :--- | :--- |
| Tool Gating | `agent/system_prompt.py:619` | `system_prompt.rs:set_skills_index` (L439-444) | Partial: gate exists in helper, but no caller invokes it during assembly |
| Owning Home Scope | `agent/prompt_builder.py:1890-1895` | `runtime_cwd::CwdInputs.home`, `ProfilePlatformGuidance.home` | Present in structures; not passed to skills loader |
| Create Dir Resolution | `agent/skill_utils.py:617-658` (`get_skill_create_dir`) | *None* | Missing: no Rust parser resolves `skills.create_dir` |
| External Dirs Resolution | `agent/skill_utils.py:557-614` (`get_external_skills_dirs`) | *None* | Missing: no Rust parser resolves `skills.external_dirs` |
| Combined External Sources | `agent/skill_utils.py:680-701` (`get_all_skills_dirs()[1:]`) | *None* | Missing: no helper prepends `create_dir` before `external_dirs` |
| Working Directory Scope | `agent/skill_utils.py:757-763` (`scope_terminal_cwd`) | `runtime_cwd::CwdInputs::agent_cwd` / `context_cwd` | Present in `runtime_cwd.rs`; unlinked to `skill_discovery.rs` |
| Project Root Resolution | `agent/skill_utils.py:743-780` (`find_project_root`) | `skill_discovery.rs:162-176` (`project_root`) | Implemented and verified |
| Project Trust & Candidate Dirs | `agent/skill_utils.py:783-852` (`get_project_skills_dirs`) | `skill_discovery.rs:106-157` (`project_dirs`) | Implemented; requires caller expansion closure and start path |
| Security Quarantine Gate | `agent/skill_utils.py` / `tools/skills_guard.py` | `skill_discovery.rs:41-74` (`ProjectAdmission::is_quarantined`) | Implemented via `skills_guard::scan_skill_cached` |
| Category Demotion Mode | `agent/coding_context.py:coding_compact_skill_categories` | `coding_prompt.rs:588-597` (`compact_skill_categories`) | Implemented; unlinked to `PromptLoader` caller |
| Prompt Loader & 32-LRU | `agent/prompt_builder.py:1856-1949` | `skill_loader.rs:86-140` (`PromptLoader`) | Implemented; needs long-lived instance holder |
| Combined Index Assembly | `agent/prompt_builder.py:1950-2240` | `skill_loader.rs:144-172` (`load_index`) | Implemented and verified against Python scenarios |
| Help Guidance Coupling | `agent/system_prompt.py:655-656` | `system_prompt.rs:268-275` (`stable_guidance`) | Implemented; requires rendered string prior to `initialize_stable` |
| Volatile Section Staging | `agent/system_prompt.py:911-926` | `system_prompt.rs:655-666` (`assemble`) | Implemented: `self.skills` placed at position 0 of volatile tier |

---

## 3. Concrete Missing Resolution and Expansion Steps

To connect `PromptLoader` to native prompt assembly, five specific configuration resolution and path expansion adapters must be introduced.

### Step 1: External and Create Directory Source Resolution

Python reference: `agent/skill_utils.py:get_all_skills_dirs()`, `get_external_skills_dirs()`, and `get_skill_create_dir()`.

A dedicated helper (for example `skill_discovery::resolve_external_dirs`) must be added to parse the raw configuration and return `Vec<PathBuf>` representing external sources.

Required resolution order and rules:
1. `skills.create_dir`:
   - Read string from `config["skills"]["create_dir"]`. Trim Python whitespace.
   - If empty or non-string, ignore.
   - Expand environment variables and tilde (see Step 2).
   - If relative, resolve relative to the owning Hermes profile home (`HERMES_HOME`), not process cwd.
   - Canonicalize via `file_read_safety::realpath_abs`.
   - If canonical path equals `local_skills` (`<home>/skills`), treat as unset (ignore).
   - Only include in external prompt sources if `path.is_dir()` is true.
2. `skills.external_dirs`:
   - Read from `config["skills"]["external_dirs"]`. Accepts either a single string or an array of strings.
   - For each entry:
     - Trim Python whitespace. If empty, skip.
     - Expand environment variables and tilde.
     - If relative, resolve relative to `HERMES_HOME`, not process cwd.
     - Canonicalize via `file_read_safety::realpath_abs`.
     - Skip if canonical path equals `local_skills`.
     - Skip if already present in the candidate set (`seen` deduplication).
     - Check `path.is_dir()`. If true, append to results; if false, log debug message and omit.
3. Ordering:
   - When `skills.create_dir` exists on disk and is distinct from `local_skills`, it must be inserted as index 0 of external directories, followed by `skills.external_dirs` in config order.
   - This preserves Python's `get_all_skills_dirs()[1:]` contract where created skills precede external skills in precedence.

### Step 2: Session-Captured Path and Environment Expansion

Python reference: `os.path.expanduser(os.path.expandvars(entry))`.

In `skill_discovery.rs:project_dirs`, path normalization expects a closure:
```rust
mut expand: impl FnMut(&str) -> std::io::Result<PathBuf>
```

Currently, `webhook_filters::expandvars` reads from `std::env::var`. In a multi-session or gateway runtime, environment variable lookups must not read ambient process state.

Required expansion specifications:
1. Environment Variable Expansion:
   - Variables must expand against the session's captured environment (`RuntimeGuidance.env`, `BTreeMap<OsString, OsString>`), falling back to process env only if the captured map omits the key.
   - Expansion must support both `$VAR` and `${VAR}` forms while preserving unterminated tokens.
2. User Tilde Expansion:
   - Must use `file_read_safety::expand_user(raw, user_home)`.
   - Expansion order is critical: evaluate environment variables first, then evaluate tilde expansion on the result.
3. Base Directory Anchoring Divergence:
   - For `skills.create_dir` and `skills.external_dirs`: relative paths resolve against `home` (profile home).
   - For `skills.trusted_project_dirs`: relative paths resolve against process cwd or launch cwd (`launch.join(path)`).

### Step 3: Working Directory Resolution and Project Source Assembly

Python reference: `agent/skill_utils.py:get_project_skills_dirs()`, `find_project_root()`, and `_candidate_project_skills_dirs()`.

To invoke `skill_discovery::project_dirs`, the caller must supply:
1. `start`:
   - Must be derived from `runtime_cwd::CwdInputs`.
   - Prefer `scope.context_cwd()` or `scope.agent_cwd()`. This preserves the terminal scope working directory across interactive sessions and cron executions.
2. `user_home`:
   - Must be the OS user home directory (`dirs::home_dir()` or captured OS home), not the profile home (`~/.hermes` or profile directory).
   - Python explicitly checks `cur == home` to prevent a git repository located at the user's home directory (such as a dotfiles repository) from turning every session into a project-local skill scope.
3. `local_skills`:
   - Explicitly `<home>/skills`. Used by `project_dirs` to prevent `<root>/.hermes/skills` from colliding when `HERMES_HOME` itself resides inside a git checkout.
4. `config`:
   - The session configuration value (`&serde_json::Value`).
   - Project discovery must check `skills.project_discovery != false`. The check must fail only on exact boolean `false`; null, integers, or missing values permit discovery.

### Step 4: Visibility, Toolset Mapping, and Category Demotion

Python reference: `agent/system_prompt.py:620-645` and `agent/prompt_builder.py:1929-1943`.

Before invoking `PromptLoader::render`, the caller must construct `Visibility` and compact categories:
1. Active Skills Tool Gating:
   - Check whether `tools` contains any of `skills_list`, `skill_view`, or `skill_manage`.
   - If none are present, skills prompt generation is bypassed entirely, returning an empty string without invoking `PromptLoader`.
2. Toolset Ingestion:
   - Map each active tool name to its parent toolset using `toolset_resolution::ToolsetResolver` or static registry mapping.
   - Pass resolved toolset set into `Visibility.toolsets`.
3. Disabled Names Resolution:
   - Call `skills_index::disabled_names(config, platform_hint)`.
   - Strips essential skills (`hermes-agent`) from the disabled set.
4. Compact Categories Resolution:
   - Call `coding_prompt::compact_skill_categories(is_coding, config_mode)`.
   - When coding posture is active and mode is `focus`, demotes non-coding categories to names-only lines.

### Step 5: Native Prompt Assembly Integration and Help Guidance Ordering

Python reference: `agent/system_prompt.py:655-656` and `agent/system_prompt.py:911-926`.

Integration into `system_prompt.rs` requires respecting execution order dependencies:
1. Upstream Ordering Dependency:
   - `system_prompt.rs:initialize_stable` evaluates `input.skills_index.contains("- hermes-agent:")` to select between `HERMES_AGENT_HELP_GUIDANCE` and `HERMES_AGENT_HELP_GUIDANCE_NO_SKILLS`.
   - Consequently, `PromptLoader::render` must be executed before `initialize_stable` is called. Calling `initialize_stable` prior to rendering causes `skills_index` to be empty, producing incorrect help guidance.
2. Volatile Placement:
   - The rendered skills prompt string must be populated into `ResolvedPromptSections.skills`.
   - `ResolvedPromptSections::assemble` places `self.skills` at position 0 of the volatile tier, preceding memory and runtime timestamps. This maintains prefix cache stability across prompt rebuilds.
3. Method Adjustment on `ResolvedPromptSections`:
   - `ResolvedPromptSections::set_skills_index` currently accepts `&skills_index::Index` and re-renders it internally.
   - Because `PromptLoader` caches the final rendered `String`, `ResolvedPromptSections` requires a method to accept the pre-rendered string directly (for example `set_skills_prompt(Option<String>)`), avoiding double rendering and cache bypass.

### Step 6: Synchronized PromptLoader Lifecycle

PORT.md reference: line 18 ("Production must retain and synchronize one PromptLoader owner across builds").

1. Persistence:
   - `PromptLoader` contains an internal `VecDeque` of up to 32 cached prompts and maintains `ProjectAdmission` quarantine decisions.
   - It must be held in a long-lived state (such as `Arc<tokio::sync::Mutex<PromptLoader>>` or within the long-lived session manager). Recreating `PromptLoader` per turn destroys the LRU cache and re-scans disk snapshots.
2. Invalidation Hooks:
   - When skills are added, removed, or modified (via `skill_manage`, marketplace installation, or session reset), `PromptLoader::clear(home, clear_snapshot)` must be invoked to invalidate in-memory prompts and purge `.skills_prompt_snapshot.json`.

---

## 4. Comprehensive Pitfall Catalog

| # | Category | Pitfall Description and Failure Mode | Mitigation Strategy |
| :- | :--- | :--- | :--- |
| 1 | Path Resolution | **Relative Path Anchoring:** Resolving relative paths in `skills.external_dirs` or `skills.create_dir` against cwd instead of `HERMES_HOME`. Causes skill paths to break whenever the agent or terminal changes working directory. | Explicitly anchor relative `create_dir` and `external_dirs` paths using `hermes_home.join(path)`. Anchor only `trusted_project_dirs` to cwd/launch. |
| 2 | Isolation | **Thread-Bound Profile Leaks:** In Python, un-scoped threads fall back to the default home and leak the default profile skills. In Rust, sharing global paths across sessions creates cross-tenant bleed. | Pass explicit `home: &Path` and `skills: &Path` from the owning session database path throughout all resolution functions. |
| 3 | Project Scope | **Dotfiles False-Positive:** Passing profile home instead of OS user home to `skill_discovery::project_root`. When a user has a git repository in their user home (such as dotfiles), every session is treated as project-scoped. | Ensure `user_home` passed to `project_root` is strictly the OS home directory (`Path.home()` / `dirs::home_dir()`), distinct from `profile_home`. |
| 4 | Expansion | **Process Environment Poisoning:** Using `std::env::var` for expanding `$VAR` in config paths. In gateway multi-tenant runtimes, this leaks host variables or ignores session-specific environment overrides. | Supply an expansion closure that looks up variables in the session's captured `RuntimeGuidance.env` map. |
| 5 | Expansion | **Expansion Evaluation Order:** Performing tilde expansion before variable expansion. If a config entry contains `~/$SKILL_PATH` or `$BASE_DIR/skills`, expanding `~` first fails to handle variables that expand to home paths. | Always apply variable expansion (`expandvars`) prior to user tilde expansion (`expanduser`). |
| 6 | Precedence | **Create-Dir Ordering:** Appending `skills.create_dir` at the end of external dirs rather than at index 0. Reverses precedence between agent-created skills and secondary external collections. | Ensure `create_dir` is evaluated first and placed ahead of `skills.external_dirs` in the combined external list. |
| 7 | Deduplication | **Local Skills Duplication:** Failing to filter out `create_dir` or `external_dirs` that point to `<home>/skills`. Results in duplicate scanning, repeated category entries, and corrupted snapshot manifests. | Canonicalize all candidate paths with `realpath_abs` and explicitly reject paths equal to `local_skills`. |
| 8 | Availability | **Non-Existent Directory Rejection:** Failing to drop non-existent external directories from the sources slice. Causes spurious I/O errors or unnecessary walk attempts. | Test `path.is_dir()` during external directory resolution. Non-existent directories must be discarded with debug logging. |
| 9 | Assembly | **Help Guidance Race / Misordering:** Invoking `system_prompt::initialize_stable` before rendering skills. Stable guidance checks `input.skills_index.contains("- hermes-agent:")` to select help text; premature invocation forces the no-skills default. | Sequence `PromptLoader::render` before `initialize_stable` in the prompt assembly pipeline. |
| 10 | Lifecycle | **Transient Loader Allocation:** Constructing a fresh `PromptLoader` on every prompt assembly call. Discards the 32-entry rendered string LRU cache and clears `ProjectAdmission` quarantine decisions, forcing full cold scans. | Store `PromptLoader` in shared session/agent state and synchronize access via a mutex across builds. |

---

## 5. Concrete Implementation Roadmap

To connect `PromptLoader` to native prompt assembly cleanly, implement the following changes in order:

1. **Implement `skill_discovery::resolve_external_dirs`:**
   - Input: `config: &Value`, `home: &Path`, `local_skills: &Path`, and variable expansion closure `expand`.
   - Reads `skills.create_dir` and `skills.external_dirs`.
   - Expands `$VAR` and `~`, anchors relative paths to `home`, resolves canonical paths, drops `local_skills` and non-directories, and returns `Vec<PathBuf>` with `create_dir` first.

2. **Implement Session-Scoped Environment Expander:**
   - Provide a helper function `expand_session_path(entry: &str, home: &Path, user_home: &Path, env: &BTreeMap<OsString, OsString>) -> io::Result<PathBuf>` combining `expandvars` against `env` and `file_read_safety::expand_user`.

3. **Wire Source Resolution in Session Prompt Builder:**
   - Resolve `project_dirs` using `CwdInputs` start path, OS `user_home`, `local_skills`, and the session expander.
   - Resolve `external_dirs` using `resolve_external_dirs`.
   - Assemble `Sources { home, skills, project: &project_dirs, external: &external_dirs }`.

4. **Add Direct String Injection on `ResolvedPromptSections`:**
   - Add `pub fn set_rendered_skills(&mut self, rendered: Option<String>)` or allow direct assignment to `self.skills`.
   - Ensure `assemble()` continues placing `self.skills` at position 0 in `volatile`.

5. **Sequence Prompt Construction in Pipeline:**
   - Check tools gate (`skills_list`, `skill_view`, `skill_manage`).
   - If active, call `prompt_loader.lock().await.render(...)`.
   - Pass rendered string into `StableGuidance { skills_index: &rendered, .. }`.
   - Call `initialize_stable`.
   - Call `set_rendered_skills(Some(rendered))`.
   - Assemble final prompt.
