# Stable Prompt Guidance & Assembly Parity Review

**Repository:** `/home/eins0fx/development/hermes-agent-port`
**Review Scope:** [`rust/crates/hermes-gateway/src/system_prompt.rs`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/system_prompt.rs) (`stable_guidance`) vs [`agent/system_prompt.py`](file:///home/eins0fx/development/hermes-agent-port/agent/system_prompt.py) (lines 470–657).
**Oracle Evaluation:** [`rust/tools/gen_stable_guidance_goldens.py`](file:///home/eins0fx/development/hermes-agent-port/rust/tools/gen_stable_guidance_goldens.py) and [`rust/tools/stable-guidance-goldens.json`](file:///home/eins0fx/development/hermes-agent-port/rust/tools/stable-guidance-goldens.json).
**Execution Constraints:** Read-only review. No existing code modified. No `cargo` commands executed.

---

## 1. Executive Summary & Verdict

The initial stable guidance implementation in [`stable_guidance`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/system_prompt.rs#L30-L124) faithfully reproduces the ordering, tool-gating, model-family heuristics, and string transformations of the reference Python implementation in [`agent/system_prompt.py`](file:///home/eins0fx/development/hermes-agent-port/agent/system_prompt.py#L470-L657) for the bounded slice it implements.

Specifically:
1. **Section Sequence & Placement:** The 8 core prompt sections (Identity, Help Guidance, Task Completion, Parallel Tool Calls, Tool Guidance, Steer Channel Note, Tool-Use Enforcement + Google Operational Directives, and Execution Guidance) follow the exact order of Python.
2. **Help Guidance Slot & Mutation:** The slot reservation at index 1 and subsequent replacement by `HERMES_AGENT_HELP_GUIDANCE` when `skill_view` is present in tools and `"- hermes-agent:"` exists in the skills index is semantically equivalent to Python's in-place list mutation.
3. **Model Family Heuristics:** The gating logic in [`model_guidance_enabled`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/system_prompt.rs#L166-L187) accurately mirrors Python's truthy/falsy string sets (`"true"`, `"always"`, `"never"`, etc.), case-insensitive array matching with empty-string wildcards, and defaults fallback.
4. **Execution Guidance String Substitution:** When `web_search` is missing from the toolset, the exact two replacement operations on `OPENAI_MODEL_EXECUTION_GUIDANCE` match Python byte-for-byte.

However, critical boundaries and gaps exist:
- **Oracle Isolation:** [`gen_stable_guidance_goldens.py`](file:///home/eins0fx/development/hermes-agent-port/rust/tools/gen_stable_guidance_goldens.py) tests an AST-extracted slice rather than the live Python runtime, couples `soul` and `skills_index` fixtures into only two locked pairs, and leaves certain model-family permutations unverified.
- **Edge-Case Divergences:** Whitespace-only `soul` values do not fall back to `DEFAULT_AGENT_IDENTITY` in Rust, and non-string truthy `_kanban_worker_guidance` causes a server panic via `.expect()`.
- **Pipeline Truncation:** `stable_guidance` is strictly an initial slice. Ten subsequent stable and context sections (Alibaba model workaround, environment hints, coding operating brief, Python toolchain probe, Bot Mode protocol, active profile hint, platform hints, context files, and volatile tier assembly) are absent from `system_prompt.rs`.

---

## 2. Detailed Parity Comparison: `stable_guidance` vs `agent/system_prompt.py` (Lines 470–657)

### 2.1 Identity / SOUL (`DEFAULT_AGENT_IDENTITY`)
- **Python ([`agent/system_prompt.py:475–488`](file:///home/eins0fx/development/hermes-agent-port/agent/system_prompt.py#L475-L488)):**
  Checks `agent.load_soul_identity or not agent.skip_context_files`. Reads via `load_soul_md(_ctx_len, home_override=_agent_home(agent))`, which strips leading/trailing whitespace, scans context, applies context length truncation, and returns `None` if empty. If `_soul_loaded` is `False`, appends `DEFAULT_AGENT_IDENTITY`.
- **Rust ([`system_prompt.rs:39–43`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/system_prompt.rs#L39-L43)):**
  ```rust
  let mut parts = vec![input
      .soul
      .filter(|text| !text.is_empty())
      .unwrap_or_else(|| guidance("DEFAULT_AGENT_IDENTITY"))
      .to_owned()];
  ```
- **Parity Assessment:**
  - *Contract Difference:* Rust assumes `soul` is pre-resolved into `Option<&'a str>`.
  - *Whitespace Divergence:* In Rust, `!text.is_empty()` checks raw length. If `soul` contains whitespace only (e.g. `Some("   \n")`), Rust retains the whitespace string as the identity section. In Python, `load_soul_md` calls `content = (...).strip()`; if empty, it returns `None`, falling back to `DEFAULT_AGENT_IDENTITY`. When `PromptParts::from_sections` later trims boundary whitespace, an all-whitespace identity section is completely discarded, leaving the prompt with no identity block.
  - *Recommendation:* Update Rust filter to check for non-whitespace content:
    ```rust
    input.soul.filter(|text| !text.trim_matches(crate::python_value::python_whitespace).is_empty())
    ```

### 2.2 Help Guidance Slot & Variant Selection
- **Python ([`agent/system_prompt.py:496–498, 651–657`](file:///home/eins0fx/development/hermes-agent-port/agent/system_prompt.py#L496-L498)):**
  Records slot `_help_guidance_slot = len(stable_parts)` (position 1) and appends `HERMES_AGENT_HELP_GUIDANCE_NO_SKILLS`. Later, after the skills prompt is rendered (lines 656–657):
  ```python
  if _has_skill_view and "- hermes-agent:" in skills_prompt:
      stable_parts[_help_guidance_slot] = HERMES_AGENT_HELP_GUIDANCE
  ```
- **Rust ([`system_prompt.rs:44–53`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/system_prompt.rs#L44-L53)):**
  ```rust
  parts.push(
      guidance(
          if has("skill_view") && input.skills_index.contains("- hermes-agent:") {
              "HERMES_AGENT_HELP_GUIDANCE"
          } else {
              "HERMES_AGENT_HELP_GUIDANCE_NO_SKILLS"
          },
      )
      .into(),
  );
  ```
- **Parity Assessment:** Full semantic parity. Because `input.skills_index` is provided in `StableGuidance`, Rust computes the variant eagerly rather than using a deferred index mutation. The position (slot index 1) and condition (`has("skill_view") && input.skills_index.contains("- hermes-agent:")`) match Python exactly.

### 2.3 Universal Task Completion Guidance
- **Python ([`agent/system_prompt.py:506–507`](file:///home/eins0fx/development/hermes-agent-port/agent/system_prompt.py#L506-L507)):**
  `if getattr(agent, "_task_completion_guidance", True) and agent.valid_tool_names: stable_parts.append(TASK_COMPLETION_GUIDANCE)`
- **Rust ([`system_prompt.rs:54–57`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/system_prompt.rs#L54-L57)):**
  `if !input.tools.is_empty() { if enabled("_task_completion_guidance") { parts.push(guidance("TASK_COMPLETION_GUIDANCE").into()); } ... }`
- **Parity Assessment:** Full parity. Both gate on toolset non-emptiness and default to enabled when the setting is omitted.

### 2.4 Universal Parallel Tool Call Guidance
- **Python ([`agent/system_prompt.py:517–519`](file:///home/eins0fx/development/hermes-agent-port/agent/system_prompt.py#L517-L519)):**
  `if getattr(agent, "_parallel_tool_call_guidance", True) and agent.valid_tool_names: stable_parts.append(PARALLEL_TOOL_CALL_GUIDANCE)`
- **Rust ([`system_prompt.rs:58–61`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/system_prompt.rs#L58-L61)):**
  `if enabled("_parallel_tool_call_guidance") { parts.push(guidance("PARALLEL_TOOL_CALL_GUIDANCE").into()); }`
- **Parity Assessment:** Full parity.

### 2.5 Tool-Aware Behavioral Guidance (`tool_parts.join(" ")`)
- **Python ([`agent/system_prompt.py:521–553`](file:///home/eins0fx/development/hermes-agent-port/agent/system_prompt.py#L521-L553)):**
  - `memory`: Injects `MEMORY_GUIDANCE` if `_memory_enabled`, else `USER_PROFILE_GUIDANCE` if `_user_profile_enabled`.
  - `session_search`: Injects `SESSION_SEARCH_GUIDANCE`.
  - `skill_manage`: Injects `SKILLS_GUIDANCE`.
  - `kanban`: Injects `_kanban_worker_guidance` if truthy; else `KANBAN_GUIDANCE` if `_kanban_guidance is None and "kanban_show" in agent.valid_tool_names`.
  - Joins all populated tool guidance elements with a single space `" "`.
- **Rust ([`system_prompt.rs:62–88`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/system_prompt.rs#L62-L88)):**
  Replicates the exact conditional structure and space-joining logic.
- **Parity Assessment & Corner Cases:**
  - *Tool-Free Kanban Injection:* In Python, `_kanban_worker_guidance` is not gated on `agent.valid_tool_names`. If truthy, Python appends it even when tools are empty. Rust matches this behavior: `tool_parts` collects `kanban` even when `input.tools.is_empty()`.
  - *Empty/False Kanban Handling:* In Python, an empty string `""` or `False` means `_kanban_guidance` is falsy, but `_kanban_guidance is None` is `False`, preventing fallback to `KANBAN_GUIDANCE`. In Rust, `kanban.is_null()` is `false` for `""` and `false`, matching Python.
  - *Type Assertion Panic Risk:* In Rust, line 80:
    ```rust
    kanban.as_str().expect("resolved kanban guidance must be text")
    ```
    If `_kanban_worker_guidance` in JSON settings is boolean `true` or numeric `1`, `truthy(kanban)` evaluates to `true`, but `kanban.as_str()` returns `None`, panicking the thread. In Python, non-string values cause a `TypeError` on join. In Rust gateway servers, panics should be avoided by using `kanban.as_str().unwrap_or_default()`.

### 2.6 Steer Channel Note
- **Python ([`agent/system_prompt.py:556–558`](file:///home/eins0fx/development/hermes-agent-port/agent/system_prompt.py#L556-L558)):**
  `if agent.valid_tool_names: stable_parts.append(STEER_CHANNEL_NOTE)`
- **Rust ([`system_prompt.rs:89–90`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/system_prompt.rs#L89-L90)):**
  `if !input.tools.is_empty() { parts.push(guidance("STEER_CHANNEL_NOTE").into()); ... }`
- **Parity Assessment:** Full parity.

### 2.7 Tool-Use Enforcement & Per-Model Directives
- **Python ([`agent/system_prompt.py:566–587`](file:///home/eins0fx/development/hermes-agent-port/agent/system_prompt.py#L566-L587)):**
  - Gated on `agent.valid_tool_names`.
  - Checks `_tool_use_enforcement` via bool, case-insensitive string literals (`"true"`, `"always"`, `"yes"`, `"on"` / `"false"`, `"never"`, `"no"`, `"off"`), substring list against `model_lower`, or defaults (`TOOL_USE_ENFORCEMENT_MODELS`).
  - If active, appends `TOOL_USE_ENFORCEMENT_GUIDANCE`.
  - If `"gemini" in _model_lower or "gemma" in _model_lower`, appends `GOOGLE_MODEL_OPERATIONAL_GUIDANCE`.
- **Rust ([`system_prompt.rs:100–106`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/system_prompt.rs#L100-L106)):**
  - Evaluates `gate("_tool_use_enforcement", "TOOL_USE_ENFORCEMENT_MODELS")`.
  - Pushes `TOOL_USE_ENFORCEMENT_GUIDANCE`.
  - Checks `model.contains("gemini") || model.contains("gemma")` to push `GOOGLE_MODEL_OPERATIONAL_GUIDANCE`.
- **Parity Assessment:** Full parity. The nesting of Google directives inside tool-use enforcement matches Python line 583.

### 2.8 Execution Discipline Guidance
- **Python ([`agent/system_prompt.py:601–618`](file:///home/eins0fx/development/hermes-agent-port/agent/system_prompt.py#L601-L618)):**
  - Gated on `agent.valid_tool_names`.
  - Checks `_execution_guidance` (defaults to `EXECUTION_GUIDANCE_MODELS`).
  - Calls `execution_guidance_text(agent.valid_tool_names)`. If `"web_search"` is not in `valid_tool_names`, removes the bullet `"- Current facts (weather, news, versions) → use web_search\n"` and replaces `"(search_files, web_search, read_file, etc.)"` with `"(search_files, read_file, etc.)"`.
- **Rust ([`system_prompt.rs:107–121`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/system_prompt.rs#L107-L121)):**
  - Evaluates `gate("_execution_guidance", "EXECUTION_GUIDANCE_MODELS")`.
  - Performs the exact two string replacements when `!has("web_search")`.
- **Parity Assessment:** Full parity. Exact string byte match confirmed.

---

## 3. Evaluation of `gen_stable_guidance_goldens.py` Oracle Coverage

[`rust/tools/gen_stable_guidance_goldens.py`](file:///home/eins0fx/development/hermes-agent-port/rust/tools/gen_stable_guidance_goldens.py) generates 192 test vectors verifying [`stable_guidance`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/system_prompt.rs#L30) against Python AST output.

```
4 toolsets × 3 models × 8 settings × 2 (soul/skills) pairs = 192 cases
```

### 3.1 Oracle Strengths
1. **AST-Extracted Execution:** Parses the exact AST of `build_system_prompt_parts` from [`agent/system_prompt.py`](file:///home/eins0fx/development/hermes-agent-port/agent/system_prompt.py) and `execution_guidance_text` from [`agent/prompt_builder.py`](file:///home/eins0fx/development/hermes-agent-port/agent/prompt_builder.py), ensuring tests track upstream code directly rather than manual transcriptions.
2. **Deterministic Hash Invariants:** Encodes `stable_parts` via canonical JSON separators (`separators=(",", ":")`) without stripping internal whitespace, hashing each case with SHA-256 to assert byte-for-byte fidelity in Rust's [`initial_stable_sections_match_python_order_and_bytes`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/system_prompt.rs#L247-L264).
3. **Multi-Axis Permutations:** Sweeps combinations of empty toolsets vs single tool vs full tool guidance vs search tools, along with overridden guidance settings.

### 3.2 Coverage Gaps and Oracle Blind Spots

| Dimension | In Oracle (`gen_stable_guidance_goldens.py`) | Reference Space in Python | Parity Risk / Blind Spot |
|---|---|---|---|
| **Identity & Skills Coupling** | Only 2 locked pairs: `(None, "")` and `(" scoped soul ", "- hermes-agent: reference")` | Independent: any soul string × any skills index | **High:** Never tests `(None, "- hermes-agent: reference")` (default identity with `hermes-agent` skill installed) or `(" scoped soul ", "")` (custom soul without `hermes-agent` skill). |
| **Soul Whitespace Boundary** | `None` and `" scoped soul "` | Empty string `""`, whitespace-only `"   "`, newlines | **Medium:** Does not exercise Rust's `!text.is_empty()` check against whitespace-only inputs, concealing the strip divergence. |
| **Model Family Breadth** | `"llama"`, `"gemini-3"`, `"gpt-5"` | 9 tool-use models, 11 execution models | **Medium:** `"gemma"` is never tested (should trigger Google directives). Models in `EXECUTION_GUIDANCE_MODELS` but not in `TOOL_USE_ENFORCEMENT_MODELS` (e.g. `"kimi"`, `"mistral"`, `"minimax"`, `"mimo"`, `"codex"`) are never tested under default `"auto"` settings. |
| **Settings Value Types** | Booleans `True`/`False` and string kanban | String keywords (`"always"`, `"never"`, `"off"`), arrays (`["my-model"]`), `None` | **Low–Medium:** Gating string/array logic is tested in `assembly_goldens.py`, but not within the holistic `stable_guidance` fixture set. |
| **Skills Tools Breakdown** | `skill_manage` (in toolset 3) and `skill_view` (in toolset 4) | `skills_list`, `skill_view`, `skill_manage` | **Low:** `skills_list` presence without `skill_view` is never exercised in toolsets. |
| **Pipeline Completeness** | Stops at AST node `has_skills_tools` | Full prompt assembly (lines 658–1033) | **High:** Oracle validates the initial slice in isolation; no coverage exists for the remainder of the prompt pipeline. |

---

## 4. Concrete Parity Gaps & Implementation Hazards

### Gap 1: Soul Whitespace Filtering
- **Issue:** Rust `input.soul.filter(|text| !text.is_empty())` does not trim whitespace.
- **Impact:** Passing `Some("   \t\n")` bypasses the fallback to `DEFAULT_AGENT_IDENTITY`. Subsequent tier joining in `PromptParts::from_sections` trims the section to empty and filters it out, resulting in a prompt with **no agent identity**.
- **Remedy:** Filter using Python-compatible whitespace trimming:
  ```rust
  input.soul.filter(|text| !text.trim_matches(crate::python_value::python_whitespace).is_empty())
  ```

### Gap 2: Panic Hazard on Non-String Kanban Settings
- **Issue:** Line 80 performs `.as_str().expect("resolved kanban guidance must be text")`.
- **Impact:** If `_kanban_worker_guidance` in `settings` is deserialized as a boolean `true` or integer `1`, `truthy(kanban)` evaluates to `true`, but `as_str()` returns `None`, panicking the thread.
- **Remedy:** Gracefully extract or ignore non-string values:
  ```rust
  if let Some(text) = kanban.as_str().filter(|s| !s.is_empty()) {
      tool_parts.push(text);
  }
  ```

### Gap 3: Missing Downstream Sections of Stable Tier
The reference Python implementation in [`agent/system_prompt.py`](file:///home/eins0fx/development/hermes-agent-port/agent/system_prompt.py) continues after line 657 with essential prompt sections that are omitted from `system_prompt.rs`:

```
agent/system_prompt.py pipeline:
  [Lines 470–618] Initial stable sections  <-- Implemented in system_prompt.rs
  [Lines 619–650] Skills prompt assembly   <-- Missing (passed as input string)
  [Lines 656–657] Help guidance variant    <-- Implemented in system_prompt.rs
  [Lines 658–671] Alibaba Model Workaround <-- Missing
  [Lines 672–677] Environment Hints        <-- Missing
  [Lines 679–709] Coding Context & Brief   <-- Missing
  [Lines 710–726] Environment Probe        <-- Missing
  [Lines 727–763] Bot Mode Protocol        <-- Missing
  [Lines 764–826] Active Profile Hint      <-- Missing
  [Lines 827–870] Platform-Specific Hints  <-- Missing
  [Lines 871–910] Context Tier Assembly    <-- Missing
  [Lines 911–1033] Volatile Tier Assembly  <-- Missing
```

---

## 5. Next Full Assembler Integration Steps Using Existing Rust Modules

The `hermes-gateway` crate already contains several completed, reviewed modules that provide the necessary building blocks to extend `stable_guidance` into a full prompt assembler without duplicating functionality.

```
+-----------------------------------------------------------------------------------+
|                           Full System Prompt Assembler                            |
+-----------------------------------------+-----------------------------------------+
                                          |
      +-----------------------------------+-----------------------------------+
      |                                   |                                   |
      v                                   v                                   v
+-----------------------+     +-----------------------+     +-----------------------+
|      Stable Tier      |     |     Context Tier      |     |     Volatile Tier     |
+-----------------------+     +-----------------------+     +-----------------------+
| * stable_guidance()   |     | * Coding workspace    |     | * Skills index        |
| * Alibaba workaround  |     |   snapshot            |     | * Memory store        |
| * Environment hints   |     | * Post-workspace tail |     | * User profile        |
|   (Host/User/CWD)     |     | * Context files       |     | * Plugin sections     |
|   [file_read_safety,  |     |   (AGENTS.md, etc.)   |     | * Timestamp & session |
|    cwd_placeholder]   |     |   [file_read_safety]  |     |   identity metadata   |
| * Active profile hint |     | * system_message      |     |   [session_db]        |
|   [profile_name]      |     +-----------------------+     +-----------------------+
| * Platform hints      |                                               |
|   [config_types,      |                                               |
|    catalog]           |                                               |
+-----------------------+                                               |
      |                                                                 |
      +-----------------------------------+-----------------------------+
                                          |
                                          v
                              +-----------------------+
                              |      PromptParts      |
                              |   (stable, context,   |
                              |       volatile)       |
                              +-----------------------+
                                          |
                     +--------------------+--------------------+
                     |                                         |
                     v                                         v
         +-----------------------+                 +-----------------------+
         |      PromptCache      |                 |      NativeAgent      |
         | * Key routing &       |                 | * with_system_prompt  |
         |   bounding            |                 | * prompted_history    |
         | [prompt_cache]        |                 | [native_agent]        |
         +-----------------------+                 +-----------------------+
```

### Step 1: Active Profile Hint via `profile_name::active_profile_name`
- **Module:** [`rust/crates/hermes-gateway/src/profile_name.rs`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/profile_name.rs)
- **Integration:** Call `profile_name::active_profile_name(&home, &root)` to determine whether the running profile is `"default"` or a named profile.
- **Output:** Format the profile warning block matching [`agent/system_prompt.py:798–825`](file:///home/eins0fx/development/hermes-agent-port/agent/system_prompt.py#L798-L825):
  - If `"default"`: `"Active Hermes profile: default. Other profiles (if any) live under <root>/profiles/<name>/..."`
  - If named: `"Active Hermes profile: <name>. This session reads and writes <home>/. The default profile's data lives at <root>/skills/..."`

### Step 2: Platform Hints & Telegram Extension via `catalog` & `config_types`
- **Modules:** [`rust/crates/hermes-gateway/src/config_types.rs`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/config_types.rs), [`rust/tools/system-prompt-guidance.json`](file:///home/eins0fx/development/hermes-agent-port/rust/tools/system-prompt-guidance.json)
- **Integration:**
  1. Retrieve base platform hint from `catalog()["PLATFORM_HINTS"][platform_key]`.
  2. For Telegram (`platform_key == "telegram"`), check `config.gateway.platforms.telegram.extra.rich_messages` (or top-level platform config) and append `catalog()["TELEGRAM_RICH_MESSAGES_HINT"]`.
  3. For TUI (`platform_key == "tui"`), check `HERMES_DESKTOP_TERMINAL=1` and append the embedded pane clarifier.
  4. Support platform hint overrides (`replace` / `append`) from user configuration.

### Step 3: Terminal Environment & Host Info Block
- **Modules:** [`rust/crates/hermes-gateway/src/cwd_placeholder.rs`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/cwd_placeholder.rs), [`rust/crates/hermes-gateway/src/file_read_safety.rs`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/file_read_safety.rs)
- **Integration:**
  1. Resolve effective working directory via `cwd_placeholder::resolve_placeholder_terminal_cwd`.
  2. Assemble the host information block for local backends:
     ```
     Host: <OS> (<release>)
     User home directory: <home>
     Current working directory: <resolved_cwd>
     ```
  3. This ensures the output adheres to the structure expected by [`stored_prompt_matches_runtime`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/system_prompt.rs#L200-L240).

### Step 4: Stored Prompt Lifecycle & Prefix Caching via `session_db`
- **Module:** [`rust/crates/hermes-gateway/src/session_db.rs`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/session_db.rs)
- **Integration:**
  1. Query session prompt snapshot via `session_db.get_session(session_id)` and resolve `_system_prompt_resolved`.
  2. Verify cache validity using `stored_prompt_matches_runtime(&stored, &runtime)`.
  3. If valid, check if `stored.starts_with(&static_prefix)` to restore the two-block cache marker; if invalidated or absent, rebuild via the assembler and persist with `session_db.update_system_prompt(session_id, Some(&joined))`.

### Step 5: Wire `native_agent` Client & Prompt Cache Key Injection
- **Modules:** [`rust/crates/hermes-gateway/src/native_agent.rs`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/native_agent.rs), [`rust/crates/hermes-gateway/src/prompt_cache.rs`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/prompt_cache.rs)
- **Integration:**
  1. Populate `native_agent.with_system_prompt(parts.joined())`.
  2. In the request preparation pipeline, call `prompt_cache::apply(&mut api_kwargs, &messages, tools, supports_caching, session_id, cache_scope_id)`.
  3. Content-addressed routing keys will hash the stable prefix (`parts.stable`) and canonical tool definitions, maintaining cache hits across turns.

---

## 6. Summary Checklist for Full Assembler Port

- [ ] **Fix `input.soul` trimming:** Filter out whitespace-only strings before identity fallback.
- [ ] **Safeguard `_kanban_worker_guidance`:** Replace `.expect()` panic with non-string safe fallback.
- [ ] **Add Alibaba model workaround:** Append explicit model ID directive when `provider == "alibaba"`.
- [ ] **Implement `build_environment_hints`:** Emit `Host:`, `User home directory:`, and `Current working directory:` lines.
- [ ] **Integrate `profile_name::active_profile_name`:** Emit the appropriate profile warning block.
- [ ] **Integrate `PLATFORM_HINTS` & `TELEGRAM_RICH_MESSAGES_HINT`:** Emit platform guidance with config override support.
- [ ] **Construct Context Tier:** Integrate workspace snapshot, trailing coding instructions, and context file discovery.
- [ ] **Construct Volatile Tier:** Render skills index, memory store formats, plugin blocks, and session timestamp lines.
- [ ] **Expand Oracle Tests:** Add test vectors for uncoupled `(soul, skills)` pairs, whitespace-only souls, `"gemma"`, and non-tool enforcement models.
