# Token-Budget-Aware Tool-Result Pruning and Pressure Demotion Analysis

## Overview and Scope

This analysis investigates the authoritative Python implementation of token-budget-aware tool-result pruning and protected-tail pressure demotion. The primary focus is `agent/context_compressor.py` and its directly called pure helpers in `agent/model_metadata.py` and `agent/turn_context.py`.

In accordance with task constraints, provider network usage, SQLite publication, ingress wiring, micro-compaction, and LLM summarization are excluded.

In this document, observed behaviors (verified directly from source code and executable tests) are strictly separated from architectural inferences (rationales derived from docstrings, issues, or commit comments).

---

## 1. Token-Protected Tail Boundary Computation

### 1.1 Source Location and Core Signature

The token-protected tail boundary is computed in `ContextCompressor._prune_old_tool_results` at `agent/context_compressor.py:4200-4243`.

```python
# agent/context_compressor.py:4146-4150
def _prune_old_tool_results(
    self, messages: List[Dict[str, Any]], protect_tail_count: int,
    protect_tail_tokens: int | None = None,
    min_prune_chars: int = _PRUNE_MIN_CHARS,
) -> tuple[List[Dict[str, Any]], int]:
```

### 1.2 Observed Behavior

1. **Count-Only Branch (`protect_tail_tokens is None or protect_tail_tokens <= 0`)**:
   - Location: `agent/context_compressor.py:4241-4242`.
   - Behavior: Sets `prune_boundary = len(result) - protect_tail_count`.

2. **Token-Budget Branch (`protect_tail_tokens is not None and protect_tail_tokens > 0`)**:
   - Location: `agent/context_compressor.py:4200-4240`.
   - **Minimum Message Floor (`min_protect`)**:
     - Location: `agent/context_compressor.py:4207-4211`.
     - Formula: `min_protect = min(protect_tail_count, len(result), _MAX_TAIL_MESSAGE_FLOOR)`.
     - Constant: `_MAX_TAIL_MESSAGE_FLOOR = 8` (`agent/context_compressor.py:1363`).
     - Note: Unlike the summarizer tail-cut (`_find_tail_cut_by_tokens` at `agent/context_compressor.py:7117`), `_prune_old_tool_results` has no lower bound clamp such as `max(3, ...)`. If `protect_tail_count` is 1 or 0, `min_protect` will be 1 or 0.
   - **Wire Thinking Accounting Setup**:
     - Location: `agent/context_compressor.py:4216-4217`.
     - `_newest_asst_idx = _last_assistant_index(result)` (`agent/context_compressor.py:1654-1664`): index of the newest message with `role == "assistant"`.
     - `_charge_all_thinking = self._stale_thinking_on_wire()` (`agent/context_compressor.py:7065-7087`): calls `agent.message_sanitization.stale_thinking_reaches_wire`, returning `True` for echo-back providers (DeepSeek, Kimi, MiMo thinking mode).
   - **Per-Message Token Estimator (`_estimate_msg_budget_tokens`)**:
     - Location: `agent/context_compressor.py:1582-1651`.
     - Message content:
       - String content: `estimate_tokens_rough(content) + 10` (`agent/model_metadata.py:3650-3695`).
         - ASCII path: `(len(text) + 3) // 4`.
         - Non-ASCII: codepoints matching `_CJK_DENSE_RE` (`[\u1100-\u11ff\u2e80-\u9fff\ua960-\ua97f\uac00-\ud7af\uf900-\ufaff\uff00-\uffef]`) count as 1 token each; non-CJK remainder is measured in UTF-8 byte length via `(len(bytes) + 3) // 4`.
       - Non-string / multimodal content: `_content_length_for_budget(content) // 4 + 10` (`agent/context_compressor.py:1464-1485`). Image parts (`image_url`, `input_image`, `image`) count as `_IMAGE_CHAR_EQUIVALENT = 6400` chars (`_IMAGE_TOKEN_ESTIMATE = 1600` tokens, `agent/context_compressor.py:1342, 1346`).
     - Tool calls: For each dict `tc` in `msg.get("tool_calls") or []`, adds `estimate_tokens_rough(str(tc))`. This measures the stringified full tool call structure (`id`, `type`, `function.name`, `function.arguments`), not only arguments.
     - Always-replayed metadata keys (`_ALWAYS_REPLAYED_BUDGET_KEYS = ("codex_reasoning_items", "codex_message_items")`, `agent/context_compressor.py:1528-1531`): adds `_serialized_length_for_budget(msg.get(key)) // 4`.
     - Generic thinking keys (`_NEWEST_TURN_ONLY_BUDGET_KEYS = ("reasoning", "reasoning_content")`, `agent/context_compressor.py:1532-1535`):
       - If `charge_stale_thinking` is false (`_charge_all_thinking` is false and `i != _newest_asst_idx`), returns before inspecting these keys.
       - If `charge_stale_thinking` is true: if `reasoning_content` is non-empty string, `reasoning` is skipped to avoid double-charging; otherwise `reasoning` is charged.
       - If neither `reasoning` nor `reasoning_content` is present, textual chars inside `reasoning_details` are counted via `_reasoning_details_text_chars` and charged at 4 chars/token (`agent/context_compressor.py:1549-1579`).
   - **Backward Walk and Loop Invariant**:
     - Location: `agent/context_compressor.py:4205-4230`.
     - Initialization: `accumulated = 0`, `boundary = len(result)`.
     - Iteration: `for i in range(len(result) - 1, -1, -1):`
     - Condition:
       ```python
       if accumulated + msg_tokens > protect_tail_tokens and (len(result) - i) >= min_protect:
           boundary = i
           break
       accumulated += msg_tokens
       boundary = i
       ```
   - **Equality and Boundary Cases**:
     - Strict token inequality: `accumulated + msg_tokens > protect_tail_tokens`. If `accumulated + msg_tokens == protect_tail_tokens`, the break does not trigger; `msg_tokens` is added to `accumulated`, `boundary` becomes `i`, and the backward walk continues.
     - Floor inequality: `(len(result) - i) >= min_protect`. If `(len(result) - i) == min_protect`, the condition is satisfied and the break triggers if the token threshold is exceeded.
     - Token exceeded but floor not reached: If `accumulated + msg_tokens > protect_tail_tokens` while `(len(result) - i) < min_protect`, the break does NOT trigger. The message floor takes precedence; tokens continue accumulating, and the boundary moves backward until `len(result) - i >= min_protect`.
     - Break index assignment: When the break triggers at index `i`, `boundary` is set to `i`. Message `i` is included in the protected region.
     - Loop finishes without break: If all messages fit within `protect_tail_tokens`, the loop runs down to `i = 0`, leaving `boundary = 0`.
   - **Count-Space Conversion**:
     - Location: `agent/context_compressor.py:4238-4240`.
     - `budget_protect_count = len(result) - boundary`
     - `protected_count = max(budget_protect_count, min_protect)`
     - `prune_boundary = len(result) - protected_count`
     - Note: When the break fires, `budget_protect_count == len(result) - i >= min_protect`, so `protected_count == budget_protect_count`, giving `prune_boundary = boundary = i`. When the loop finishes without break, `budget_protect_count = len(result)`, giving `prune_boundary = 0`.
   - **Role and Tool-Group Safety**:
     - `_prune_old_tool_results` does NOT call boundary alignment helpers such as `_align_boundary_forward` or `_align_boundary_backward`.
     - Boundary alignment is not needed because `_prune_old_tool_results` never removes, inserts, or splits messages. It rewrites contents strictly in place.
     - Individual passes enforce role filters: Pass 2 filters for `role == "tool"`, Pass 3 filters for `role == "assistant"`, Pass 4 targets `role == "tool"` and `role == "assistant"`. User and system messages are never modified.

### 1.3 Inferred Design Rationale

- Capping `min_protect` at `_MAX_TAIL_MESSAGE_FLOOR = 8` ensures that a high default setting (such as `protect_last_n = 20`) does not freeze an entire sequence of bulky tool results, which would trigger the issue #61932 dead-end.
- Converting from `boundary` to `budget_protect_count` and applying `max(budget_protect_count, min_protect)` in count space avoids direction inversion in index space, where a smaller index corresponds to a larger protected window.

---

## 2. Pruning Passes Before and After the Boundary

### 2.1 Pass Sequence and Scope

Pruning passes in `ContextCompressor._prune_old_tool_results` run in the following exact order:

| Step | Pass Name | Location | Target Index Scope | Target Roles | Action Summary |
|---|---|---|---|---|---|
| 0 | Tool Indexing | `context_compressor.py:4183-4198` | Full transcript | `assistant` | Populates `call_id_to_tool` mapping `tool_call_id -> (tool_name, arguments_json)`. |
| 1 | Boundary Calc | `context_compressor.py:4200-4243` | Full transcript | All | Computes `prune_boundary` using token budget or message count. |
| 2 | Pass 1: Tool Dedup | `context_compressor.py:4244-4269` | Full transcript (backward) | `tool` | Deduplicates string content >= 200 chars using MD5 prefix; older duplicates replaced with duplicate marker. Increments `pruned`. |
| 3 | Skill Detection | `context_compressor.py:4275` | Full transcript | `assistant`, `user` | Scans for protected skill names surviving into recent window or protected tail. |
| 4 | Pass 2: Summarization | `context_compressor.py:4350-4353` | Before boundary (`0 <= i < prune_boundary`) | `tool` | Summarizes tool results >= `min_prune_chars`; strips images from multimodal results. Increments `pruned`. |
| 5 | Pass 3: Arg Truncate | `context_compressor.py:4354-4364` | Before boundary (`0 <= i < prune_boundary`) | `assistant` | Truncates tool argument strings > 200 chars inside JSON if total JSON > 500 chars. Does not increment `pruned`. |
| 6 | Pass 3.5: Image Retirement | `context_compressor.py:4365-4371` | Full transcript (backward) | `tool` | Keeps newest 3 image-bearing tool messages; strips images from older tool messages. Increments `pruned`. |
| 7 | Pass 4: Pressure Demotion | `context_compressor.py:4372-4446` | Inside tail (`prune_boundary <= i < len`) | `tool`, `assistant` | Demotes bulky tool results and truncates assistant arguments inside protected tail if tokens exceed soft ceiling. |

### 2.2 Detailed Pass Mechanics

1. **Pass 1: Tool Output Deduplication (`agent/context_compressor.py:4244-4269`)**:
   - Scans backward `for i in range(len(result) - 1, -1, -1):`.
   - Filters: `role == "tool"`, `content` is `str`, and `len(content) >= _PRUNE_MIN_CHARS` (200).
   - Non-string and multimodal envelope content is skipped.
   - Hash: `h = hashlib.md5(content.encode("utf-8", errors="replace")).hexdigest()[:12]`.
   - The first (newest) occurrence records `content_hashes[h] = (i, tool_call_id)`.
   - Older duplicates are replaced:
     `result[i] = {**msg, "content": "[Duplicate tool output - same content as a more recent call]"}`.
   - Increments `pruned += 1`.
   - Boundary relation: Tail-agnostic. Operates across the whole message list, protecting the newest copy regardless of boundary position.

2. **Ghost-Skill Name Collection (`agent/context_compressor.py:4275`, helper at `1295-1333`)**:
   - `protected_skills = _collect_protected_skill_names(result, prune_boundary)`.
   - A skill name is protected if:
     - Its `skill_view` call site index is within the last `_SKILL_PRUNE_RECENT_WINDOW = 10` messages (`idx >= len(result) - 10`); OR
     - Its `skill_view` call site index is at or after `prune_boundary` (`idx >= prune_boundary`); OR
     - Its lowercased name appears in any user message at or after `prune_boundary`.

3. **Pass 2: Tool Result Summarization (`agent/context_compressor.py:4350-4353`)**:
   - Scans forward `for i in range(max(0, prune_boundary)):`.
   - Calls `_demote_tool_result_at(i, spare_protected_skills=True)` (`agent/context_compressor.py:4277-4329`).
   - If multimodal list or `_multimodal` dict: strips images via `_strip_images_from_tool_msg(msg)`. If stripped, replaces message and increments `pruned += 1`.
   - If string: skips if empty, already a placeholder (`_PRUNED_TOOL_PLACEHOLDER`), starts with `"[Duplicate tool output"`, starts with `"[screenshot removed"`, already summarized (`content.startswith("[") and " chars)" in content and len(content) < 400`), or `len(content) <= min_prune_chars`.
   - Ghost-skill guard: skips if `tool_name == "skill_view"`, `spare_protected_skills=True`, and `name.lower() in protected_skills`.
   - Replacement: calls `_summarize_tool_result(tool_name, tool_args, content)`, sets `result[idx] = {**msg, "content": summary}`, and increments `pruned += 1`.

4. **Pass 3: Assistant Tool Call Argument Truncation (`agent/context_compressor.py:4354-4364`)**:
   - Scans forward `for i in range(max(0, prune_boundary)):`.
   - Calls `_truncate_tool_call_args_at(i)` (`agent/context_compressor.py:4330-4348`).
   - If `role == "assistant"` and `tool_calls` present:
     - For each `tc`: if `isinstance(tc, dict)` and `len(args) > 500`:
       - Parses JSON and truncates string leaves > 200 chars to `leaf[:200] + "...[truncated]"` via `_truncate_tool_call_args_json` (`agent/context_compressor.py:1823-1866`).
       - Non-string leaves (integers, booleans, nulls) and short strings remain intact.
       - Re-serializes with `json.dumps(..., ensure_ascii=False)`.
     - Replaces `result[idx] = {**msg, "tool_calls": new_tcs}`.
   - Note: Modifies messages when args exceed 500 chars, but does NOT increment the `pruned` counter.

5. **Pass 3.5: Image Retirement in Tail (`agent/context_compressor.py:4365-4371`)**:
   - Calls `_retire_stale_tool_result_images(result, keep_newest=_MAX_KEEP_TOOL_IMAGES)` (`agent/context_compressor.py:1773-1803`), where `_MAX_KEEP_TOOL_IMAGES = 3` (`agent/context_compressor.py:1381`).
   - Scans backward `i` from `len(result) - 1` down to `0`.
   - Counts tool messages carrying images (`_tool_content_has_images`).
   - Leaves the newest 3 image-bearing tool messages intact.
   - Strips images from all older image-bearing tool messages via `_strip_images_from_tool_msg(msg)`.
   - Increments `pruned += 1` for each stripped message.

6. **Pass 4: Protected-Tail Pressure Demotion (`agent/context_compressor.py:4372-4446`)**:
   - Runs strictly INSIDE the protected tail (`max(0, prune_boundary) <= i < len(result)`).
   - Covered in detail in Section 3.

---

## 3. Protected-Tail Pressure Demotion Mechanism

### 3.1 Target Calculation

Location: `agent/context_compressor.py:4381-4392`.

1. **Precondition**: `protect_tail_tokens is not None and protect_tail_tokens > 0 and result`.
2. **Soft Ceiling**:
   `soft_ceiling = int(protect_tail_tokens * 1.5)`
3. **Protected Region Scope**:
   `start = max(0, prune_boundary)` up to `len(result)`.
4. **Token Accumulation Metric**:
   ```python
   def _protected_region_tokens() -> int:
       start = max(0, prune_boundary)
       return sum(
           _estimate_msg_budget_tokens(result[i])
           for i in range(start, len(result))
       )
   ```
5. **Target**: Demote messages until `_protected_region_tokens() <= soft_ceiling`.

### 3.2 Candidate Selection and Cascade Stages

Pressure demotion executes in three sequential stages:

#### Sub-stage 4a: Demote Before Recent Floor
- Location: `agent/context_compressor.py:4383-4404`.
- Floor calculation: `keep_recent = min(_PRESSURE_KEEP_RECENT_MESSAGES, len(result))` where `_PRESSURE_KEEP_RECENT_MESSAGES = 3` (`agent/context_compressor.py:1374`).
- Boundary: `demote_end = len(result) - keep_recent`.
- Trigger condition: `demote_end > prune_boundary and _protected_region_tokens() > soft_ceiling`.
- Walk: Iterates forward `for i in range(max(0, prune_boundary), demote_end):`.
- Candidates:
  - If `role == "tool"`: calls `_demote_tool_result_at(i, spare_protected_skills=False)`.
  - If `role == "assistant"`: calls `_truncate_tool_call_args_at(i)`.
- Stopping check: Evaluated after each candidate:
  `if _protected_region_tokens() <= soft_ceiling: break`.

#### Sub-stage 4b: Demote Inside Recent Floor (Sparing Newest Tool)
- Location: `agent/context_compressor.py:4409-4424`.
- Trigger condition: `if _protected_region_tokens() > soft_ceiling:`.
- Target identification: Finds `last_tool_idx` by scanning backward from `len(result) - 1` down to `0` for the first message with `role == "tool"`.
- Walk: Iterates forward `for i in range(max(0, prune_boundary), len(result)):`.
- Candidates:
  - If `last_tool_idx is not None and i == last_tool_idx`: skips (spares newest tool result).
  - If `result[i].get("role") == "tool"`: calls `_demote_tool_result_at(i, spare_protected_skills=False)`.
  - Else if `result[i].get("role") == "assistant"`: calls `_truncate_tool_call_args_at(i)`.
- Stopping check: Observed code runs this loop to completion without an internal `break`.

#### Sub-stage 4c: Absolute Last Resort (Demote Newest Tool)
- Location: `agent/context_compressor.py:4428-4436`.
- Trigger condition:
  ```python
  if (
      last_tool_idx is not None
      and last_tool_idx >= prune_boundary
      and _protected_region_tokens() > soft_ceiling
  ):
  ```
- Action: Calls `_demote_tool_result_at(last_tool_idx, spare_protected_skills=False)`.

### 3.3 Preservation Rules (Recent Messages, Skills, Images)

1. **Preservation of Recent Messages**:
   - Sub-stage 4a leaves the trailing `_PRESSURE_KEEP_RECENT_MESSAGES = 3` messages completely unvisited.
   - Sub-stage 4b continues to spare the newest tool result (`last_tool_idx`).
   - Active user messages (which typically occupy the final message index) have `role == "user"`. Neither `_demote_tool_result_at` nor `_truncate_tool_call_args_at` modifies `user` messages. User messages are preserved intact across all stages of Pass 4.
2. **Preservation of Skills**:
   - In Pass 4, all calls pass `spare_protected_skills=False` (`agent/context_compressor.py:4399, 4419, 4434`).
   - Observed behavior: Pass 4 deliberately overrides skill protection. Protected skills inside an over-budget tail are demoted.
   - Protection continuity: When a `skill_view` output (> 5000 chars) is demoted, `_summarize_tool_result_unguarded` appends the canonical marker `_skill_pruned_marker(name)` (`"[SKILL_PRUNED: content lost in compression; reload with skill_view(name='{name}')]"` at `agent/context_compressor.py:940-943, 2218-2221`).
3. **Preservation of Images**:
   - Pass 3.5 runs immediately before Pass 4 and guarantees that at most `_MAX_KEEP_TOOL_IMAGES = 3` newest image-bearing tool outputs exist in the transcript.
   - If an image-bearing tool message is selected for demotion in Pass 4, `_demote_tool_result_at` routes it through `_strip_images_from_tool_msg(msg)`. This replaces image parts with text placeholders and drops `api_content`.

### 3.4 Stopping Conditions

- Overall skip: If `_protected_region_tokens() <= soft_ceiling` upon entering Pass 4, no pressure passes run.
- Sub-stage 4a stop: Halts immediately on `_protected_region_tokens() <= soft_ceiling`.
- Sub-stage 4b skip: Skipped if Sub-stage 4a brought tokens within `soft_ceiling`.
- Sub-stage 4c skip: Skipped if Sub-stage 4b brought tokens within `soft_ceiling`.
- Termination: Terminates after Sub-stage 4c evaluates `last_tool_idx`.

---

## 4. Field Mutation, Byte-Stability, and api_content Lifecycle

### 4.1 Mutable Fields

| Role | Field | Pass | Mutation Behavior |
|---|---|---|---|
| `tool` | `content` | Pass 1 | Replaced with `"[Duplicate tool output - same content as a more recent call]"`. |
| `tool` | `content` | Pass 2, Pass 4 | Replaced with 1-line summary from `_summarize_tool_result`. |
| `tool` | `content` | Pass 2, 3.5, 4 | For `_multimodal` dict: replaced with string `"[screenshot removed] <text_summary[:200]>"`. For part list: image parts replaced with `{"type": "text", "text": "[screenshot removed to save context]"}`. |
| `assistant` | `tool_calls` | Pass 3, Pass 4 | String leaves > 200 chars in `function.arguments` JSON truncated to `head + "...[truncated]"`. |
| `tool` | `api_content` | Pass 2, 3.5, 4 | Cleared / popped via `drop_stale_api_content(msg)` if and only if image stripping occurred. |

### 4.2 Lifecycle of `api_content`

- **Implementation**: `drop_stale_api_content` (`agent/turn_context.py:189-198`) executes `msg.pop("api_content", None)`.
- **Exact Clearing Trigger**: `drop_stale_api_content` is called inside `_strip_images_from_tool_msg` (`agent/context_compressor.py:1763, 1769`).
- **When Cleared**:
  1. In Pass 2 or Pass 4, when `_demote_tool_result_at` encounters a multimodal part list or `_multimodal` envelope containing images.
  2. In Pass 3.5, when `_retire_stale_tool_result_images` strips images from tool results outside the newest 3.
- **When Preserved (NOT Cleared)**:
  1. Pass 1 deduplication preserves `api_content` if present (`result[i] = {**msg, "content": ...}`).
  2. Pass 2 and Pass 4 text summarization preserves `api_content` if present (`result[idx] = {**msg, "content": summary}`).
  3. Pass 3 and Pass 4 assistant argument truncation preserves `api_content` if present (`result[idx] = {**msg, "tool_calls": new_tcs}`).

### 4.3 Byte-Stable Invariants

The following fields must remain strictly unchanged:

1. `role`: Must never change across any pass.
2. `tool_call_id`: On `tool` rows, must remain exact byte-for-byte to maintain pairing with the assistant's `tool_calls[*].id`.
3. `assistant` fields:
   - `tool_calls[*].id`, `tool_calls[*].type`, `tool_calls[*].function.name` must remain byte-stable.
   - Non-string leaves in `arguments` JSON (integers, booleans, arrays, nested dict keys) and string leaves <= 200 chars must remain byte-stable.
   - Assistant `content` (text or None) must remain byte-stable.
   - Replay fields (`reasoning`, `reasoning_content`, `reasoning_details`, `codex_reasoning_items`, `codex_message_items`) are untouched by tool-result pruning.
4. `user` and `system` messages: 100% byte-stable and never modified.
5. Message array structure: The message count, index positions, and relative order must remain 1:1. No rows are inserted, deleted, or reordered.

---

## 5. Source Locations, Tests, and Fixture Citations

### 5.1 Authoritative Source Locations

- `agent/context_compressor.py`:
  - Line 880: `_PRUNE_MIN_CHARS = 200`
  - Lines 922-943: `SKILL_PRUNED_MARKER_PREFIX` and `_skill_pruned_marker`
  - Line 926: `_SKILL_VIEW_PRUNE_MIN_CHARS = 5000`
  - Lines 1265-1292: `_skill_view_call_sites`
  - Lines 1295-1333: `_collect_protected_skill_names`
  - Line 1336: `_CHARS_PER_TOKEN = 4`
  - Lines 1342, 1346: `_IMAGE_TOKEN_ESTIMATE = 1600`, `_IMAGE_CHAR_EQUIVALENT = 6400`
  - Line 1363: `_MAX_TAIL_MESSAGE_FLOOR = 8`
  - Line 1374: `_PRESSURE_KEEP_RECENT_MESSAGES = 3`
  - Line 1381: `_MAX_KEEP_TOOL_IMAGES = 3`
  - Lines 1464-1485: `_content_length_for_budget`
  - Lines 1488-1498: `_serialized_length_for_budget`
  - Lines 1528-1535: `_ALWAYS_REPLAYED_BUDGET_KEYS`, `_NEWEST_TURN_ONLY_BUDGET_KEYS`
  - Lines 1549-1579: `_reasoning_details_text_chars`
  - Lines 1582-1651: `_estimate_msg_budget_tokens`
  - Lines 1654-1664: `_last_assistant_index`
  - Lines 1708-1730: `_strip_image_parts_from_parts`
  - Lines 1733-1741: `_tool_content_has_images`
  - Lines 1744-1770: `_strip_images_from_tool_msg`
  - Lines 1773-1803: `_retire_stale_tool_result_images`
  - Lines 1823-1866: `_truncate_tool_call_args_json`
  - Lines 2100-2125: `_summarize_tool_result`
  - Lines 2128-2304: `_summarize_tool_result_unguarded`
  - Lines 4146-4447: `ContextCompressor._prune_old_tool_results`
  - Lines 4200-4243: Token-budget boundary calculation
  - Lines 4244-4269: Pass 1 (Deduplication)
  - Lines 4277-4329: `_demote_tool_result_at`
  - Lines 4330-4348: `_truncate_tool_call_args_at`
  - Lines 4350-4353: Pass 2 (Summarization)
  - Lines 4354-4364: Pass 3 (Argument Truncation)
  - Lines 4365-4371: Pass 3.5 (Image Retirement)
  - Lines 4372-4446: Pass 4 (Pressure Demotion)
  - Lines 7065-7087: `_stale_thinking_on_wire`
  - Lines 8140-8144: Phase-1 call site in `compress()` passing `protect_tail_tokens=self.tail_token_budget`
- `agent/model_metadata.py`:
  - Lines 3640-3647: `_CJK_DENSE_RE`
  - Lines 3650-3695: `estimate_tokens_rough`
  - Lines 3697-3720: `estimate_messages_tokens_rough`
- `agent/turn_context.py`:
  - Lines 189-198: `drop_stale_api_content`

### 5.2 Python Tests and Fixtures Proving Behaviors

1. **Issue #61932 Protected Tail Pressure Demotion**:
   - File: `tests/agent/test_protected_tail_pressure_61932.py`.
   - `_unique_tool_pair` (lines 29-52): Generates assistant tool_call + unique tool result body of configurable char size.
   - `_already_compacted_session` (lines 55-83): Fixture simulating compacted head + handoff + heavy tool tail.
   - `compressor_128k` (lines 86-103): Fixture configuring `protect_last_n=20`, `threshold_percent=0.50`, `summary_target_ratio=0.20`.
   - `test_compress_escapes_cannot_compress_further_dead_end` (lines 109-148): Verifies that multipass compression shrinks an over-context tail without hitting the "Cannot compress further" dead-end.
   - `test_all_oversized_tail_dead_end_shape_now_compresses` (lines 149-206): Direct proof of Pass 4 demotion on an 11-message transcript (3 head + 8 oversized tail messages). Proves oversized tail tools demote while tool_call/tool_result pairing remains intact and user rows are untouched.

2. **Ghost-Skill Protection and Override**:
   - File: `tests/agent/test_ghost_skill_pruning.py`.
   - `test_pressure_demotion_overrides_skill_protection` (lines 153-169): Proves that when `protect_tail_tokens` is small (100) and the tail exceeds soft ceiling, Pass 4 demotes `skill_view` bodies despite `protected_skills`, inserting `_skill_pruned_marker`.

3. **Stale Tool Image Retirement and api_content Dropping**:
   - File: `tests/agent/test_compressor_stale_tool_images.py`.
   - `test_keep_newest_images_inside_a_large_protect_window` (lines 64-79): Proves Pass 3.5 keeps at most `_MAX_KEEP_TOOL_IMAGES = 3` images inside `protect_tail_count=20`.
   - `test_multimodal_envelopes_outside_keep_window_become_text` (lines 80-120): Proves `_multimodal` dict envelope outside keep window converts to `[screenshot removed] ...` string.
   - `test_user_uploads_are_not_retired` (lines 121-141): Proves user image uploads are never retired.
   - `TestSharedImageStripHelper.test_demote_pass_drops_stale_api_content_on_image_strip` (lines 146-174): Direct proof that `_strip_images_from_tool_msg` removes `api_content` while preserving other fields.

4. **Token-Budget Boundary and Floor**:
   - File: `tests/agent/test_context_compressor.py`.
   - `test_prune_short_conv_protects_entire_tail` (lines 2009-2036): Proves that when `len(messages) <= protect_tail_count` with a generous token budget (`1_000_000`), `pruned == 0` and all tail tools are preserved verbatim.
   - `test_multimodal_message_accumulates_text_chars_not_block_count` (lines 2038-2060): Proves `_estimate_msg_budget_tokens` accumulates char length rather than part count.
   - `test_pass3_emits_valid_json_for_downstream_provider` (lines 2363-2398): Proves Pass 3 argument truncation shrinks large write_file payloads (> 500 chars) while preserving JSON validity.
   - `test_prunes_clarify_tool_result` (lines 130-166): Proves clarify summaries stay under `_PRUNE_MIN_CHARS` and remain stable across repeated prune passes.

5. **Computer Use Multimodal Handling**:
   - File: `tests/tools/test_computer_use.py`.
   - `test_prunes_openai_content_parts_image` (lines 511-541): Proves OpenAI-style content parts replace image dict with text placeholder.
   - `test_prunes_multimodal_envelope_dict` (lines 542-561): Proves `_multimodal` dict collapses to string.

---

## 6. Minimal Pure-Rust Implementation Checklist

To port token-budget-aware pruning and protected-tail pressure demotion into `rust/crates/hermes-gateway/src/tool_result_prune.rs`, implement the following components:

- [ ] **Step 1: Token Estimators and Cost Model**
  - Implement `estimate_tokens_rough(text: &str) -> usize` matching ASCII `(len + 3) / 4`, CJK regex/range counting (1 token/char), and UTF-8 byte length for remainder.
  - Implement `estimate_msg_budget_tokens(msg: &CompressionHistoryMessage, charge_stale_thinking: bool) -> usize`:
    - Clean content tokens (+10 overhead). Multimodal image parts count as 1600 tokens (6400 chars).
    - Assistant `tool_calls`: parse JSON array or format full JSON representation and charge via `estimate_tokens_rough`.
    - Always-replayed keys (`codex_reasoning_items`, `codex_message_items`) charged at 4 chars/token.
    - Generic thinking keys (`reasoning`, `reasoning_content`) charged if `charge_stale_thinking` is true.

- [ ] **Step 2: Token-Protected Boundary Calculation**
  - Implement `compute_prune_boundary(messages: &[CompressionHistoryMessage], protect_tail_count: usize, protect_tail_tokens: Option<usize>, charge_all_thinking: bool) -> usize`.
  - Floor: `min_protect = protect_tail_count.min(messages.len()).min(8)`.
  - Walk backward from `messages.len() - 1` down to 0:
    - Compute `msg_tokens` with `charge_stale_thinking = charge_all_thinking || i == newest_asst_idx`.
    - If `accumulated + msg_tokens > budget && (messages.len() - i) >= min_protect`: set `boundary = i; break`.
    - Accumulate tokens and update `boundary = i`.
  - Count conversion: `protected_count = (messages.len() - boundary).max(min_protect)`. Return `messages.len() - protected_count`.

- [ ] **Step 3: Pass 4 Protected-Tail Pressure Demotion**
  - Compute `soft_ceiling = (protect_tail_tokens * 3) / 2`.
  - Metric: `protected_region_tokens(messages, prune_boundary)`.
  - If `protected_region_tokens > soft_ceiling`:
    - Sub-stage 4a: Set `keep_recent = 3.min(messages.len())`, `demote_end = messages.len() - keep_recent`.
      - Walk forward `prune_boundary..demote_end`. Demote tool rows (`spare_protected_skills = false`) and truncate assistant tool arguments.
      - Break if `protected_region_tokens <= soft_ceiling`.
    - Sub-stage 4b: If still over ceiling, find `last_tool_idx` (newest tool message in list).
      - Walk forward `prune_boundary..messages.len()`, skipping `last_tool_idx`. Demote tool rows (`spare_protected_skills = false`) and truncate assistant arguments.
    - Sub-stage 4c: If still over ceiling and `last_tool_idx >= prune_boundary`, demote `last_tool_idx` (`spare_protected_skills = false`).

- [ ] **Step 4: Public Function and Option Integration**
  - Extend `prune_old_tool_results` or add `prune_old_tool_results_with_budget`:
    ```rust
    pub fn prune_old_tool_results_with_budget(
        messages: &[CompressionHistoryMessage],
        protect_tail_count: usize,
        protect_tail_tokens: Option<usize>,
        min_prune_chars: usize,
        charge_all_thinking: bool,
    ) -> PruneOutcome;
    ```
  - Ensure Pass 1 (dedup), Pass 2 (summarization outside boundary), Pass 3 (argument truncation outside boundary), Pass 3.5 (image retirement), and Pass 4 (pressure demotion inside boundary) execute in exact Python order.
  - Maintain exact `api_content` lifecycle: clear only on image strip rewrites; keep byte-stable on all other edits.

- [ ] **Step 5: Port Verification Unit Tests**
  - Port `TestProtectedTailPressure61932` cases (`test_compress_escapes_cannot_compress_further_dead_end` and `test_all_oversized_tail_dead_end_shape_now_compresses`).
  - Port `test_pressure_demotion_overrides_skill_protection` with ghost-skill marker emission.
  - Port `test_prune_short_conv_protects_entire_tail` verifying boundary behavior when `len <= protect_tail_count`.
