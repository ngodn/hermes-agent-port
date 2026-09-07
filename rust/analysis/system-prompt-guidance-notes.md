# System Prompt Guidance Notes & AST Import Analysis

This document records the AST extraction analysis performed by
`rust/tools/gen_system_prompt_guidance.py` on `agent/system_prompt.py` and
`agent/prompt_builder.py`. It documents both the extracted static constants and
the non-literal / computed imports that cannot be evaluated at parse time without
guessing runtime behavior.

## Summary

- **Total imports from `agent.prompt_builder`**: 20
- **Static constants extracted**: 18 (written to `rust/tools/system-prompt-guidance.json`)
- **Computed / non-literal imports reported**: 2

## Extracted Static Constants

These constants are literal or constant-folded string/list/dict definitions,
plus MEMORY_GUIDANCE and USER_PROFILE_GUIDANCE, whose aliases evaluate the
reviewed pure build_memory_guidance function with literal booleans. No runtime
modules are imported. The results are extracted into
`rust/tools/system-prompt-guidance.json` with exact string bytes, unicode sequences,
and element order preserved:

| Constant Name | Data Type | Size / Length | Purpose / Tier |
|---|---|---|---|
| `DEFAULT_AGENT_IDENTITY` | String (UTF-8) | 663 chars (667 bytes) | Tier 1 (Stable) |
| `EXECUTION_GUIDANCE_MODELS` | Array (ordered) | 11 items | Tier 1 (Model-family gate) |
| `GOOGLE_MODEL_OPERATIONAL_GUIDANCE` | String (UTF-8) | 860 chars (864 bytes) | Tier 1 (Stable) |
| `HERMES_AGENT_HELP_GUIDANCE` | String (UTF-8) | 600 chars (606 bytes) | Tier 1 (Stable) |
| `HERMES_AGENT_HELP_GUIDANCE_NO_SKILLS` | String (UTF-8) | 466 chars (470 bytes) | Tier 1 (Stable) |
| `KANBAN_GUIDANCE` | String (UTF-8) | 6584 chars (6606 bytes) | Tier 1 (Stable) |
| `MEMORY_GUIDANCE` | String (UTF-8) | 1036 chars (1046 bytes) | Tier 1 (Stable) |
| `USER_PROFILE_GUIDANCE` | String (UTF-8) | 1128 chars (1140 bytes) | Tier 1 (Stable) |
| `OPENAI_MODEL_EXECUTION_GUIDANCE` | String (UTF-8) | 3885 chars (3915 bytes) | Tier 1 (Stable) |
| `PARALLEL_TOOL_CALL_GUIDANCE` | String (UTF-8) | 618 chars (620 bytes) | Tier 1 (Stable) |
| `PLATFORM_HINTS` | Map (ordered) | 21 entries | Tier 1 / Context (Platform hint) |
| `SESSION_SEARCH_GUIDANCE` | String (UTF-8) | 186 chars (186 bytes) | Tier 1 (Stable) |
| `SKILLS_GUIDANCE` | String (UTF-8) | 441 chars (443 bytes) | Tier 1 (Stable) |
| `STEER_CHANNEL_NOTE` | String (UTF-8) | 724 chars (728 bytes) | Tier 1 (Stable) |
| `TASK_COMPLETION_GUIDANCE` | String (UTF-8) | 769 chars (771 bytes) | Tier 1 (Stable) |
| `TELEGRAM_RICH_MESSAGES_HINT` | String (UTF-8) | 863 chars (863 bytes) | Tier 1 (Stable) |
| `TOOL_USE_ENFORCEMENT_GUIDANCE` | String (UTF-8) | 824 chars (828 bytes) | Tier 1 (Stable) |
| `TOOL_USE_ENFORCEMENT_MODELS` | Array (ordered) | 9 items | Tier 1 (Model-family gate) |

## Computed / Non-Literal Imports Analysis

The following symbols imported by `agent/system_prompt.py` from `agent/prompt_builder.py`
are computed rather than literal in Python AST. Rather than guessing their runtime values
or executing unconstrained Python code during AST extraction, their AST structure,
computational nature, and Rust port recommendations are documented below:

### `drain_truncation_warnings`

- **Import Site**: `agent/system_prompt.py:34`
- **AST Node Type**: `FunctionDef`
- **Classification Reason**: Function definition 'drain_truncation_warnings' is a runtime function, not a constant
- **Definition in `prompt_builder.py:1585`**:
  ```python
  def drain_truncation_warnings() -> list:
  ```
- **Analysis**: This is a runtime helper function, not a constant. It mutates and drains
  the module-level `_TRUNCATION_WARNINGS` queue populated during context file reads.
- **Rust Tier Assembler Recommendation**: Truncation warnings should be tracked as part of
  context file discovery state in Rust and emitted through the agent status channel during assembly.

### `execution_guidance_text`

- **Import Site**: `agent/system_prompt.py:616`
- **AST Node Type**: `FunctionDef`
- **Classification Reason**: Function definition 'execution_guidance_text' is a runtime function, not a constant
- **Definition in `prompt_builder.py:672`**:
  ```python
  def execution_guidance_text(valid_tool_names=None) -> str:
  ```
- **Analysis**: This is a runtime formatting function, not a constant. It transforms
  `OPENAI_MODEL_EXECUTION_GUIDANCE` by conditionally stripping out references to `web_search`
  when `web_search` is not present in `valid_tool_names`.
- **Rust Tier Assembler Recommendation**: Implement `execution_guidance_text` as a Rust helper:
  ```rust
  pub fn execution_guidance_text(valid_tool_names: Option<&HashSet<String>>) -> String
  ```
  which applies the string replacements onto the static constant
  `OPENAI_MODEL_EXECUTION_GUIDANCE` extracted in `system-prompt-guidance.json`.

## Invariants & Preservation Guarantees

1. **Zero Runtime Imports**: Extracted strictly via Python's standard `ast` library.
   No project modules (e.g. `agent.prompt_builder`, `utils`, `yaml`) are imported.
2. **Byte-Level String Fidelity**: Preserves all multi-line indentation, markdown syntax,
   control characters, and unicode characters (`—`, `•`, `≤`, `企业微信`, `腾讯元宝`, etc.).
3. **Ordered Preservation**: Preserves the exact ordering of keys in `PLATFORM_HINTS` (21 platforms),
   model families in `EXECUTION_GUIDANCE_MODELS` (11 models) and `TOOL_USE_ENFORCEMENT_MODELS` (9 models),
   and the top-level import ordering from `agent/system_prompt.py`.
