# Tool Call Replay Source Review: Normalization, In-Flight Replay, and Wire Projection

Read-only source audit comparing Python chat tool response normalization and subsequent replay in [`agent/transports/chat_completions.py`](file:///home/eins0fx/development/hermes-agent-port/agent/transports/chat_completions.py), [`agent/chat_completion_helpers.py`](file:///home/eins0fx/development/hermes-agent-port/agent/chat_completion_helpers.py), [`agent/conversation_loop.py`](file:///home/eins0fx/development/hermes-agent-port/agent/conversation_loop.py), and [`agent/message_sanitization.py`](file:///home/eins0fx/development/hermes-agent-port/agent/message_sanitization.py) against [`rust/crates/hermes-gateway/src/native_tools.rs`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/native_tools.rs) and [`rust/crates/hermes-gateway/src/native_agent.rs`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/native_agent.rs).

---

## 1. Executive Summary & Architecture Boundary

In the Python Hermes agent, the tool-calling lifecycle is strictly divided into three distinct operational tiers:

1. **Ingest & Response Normalization**: Converting raw provider HTTP response payloads (`choices[0].message`) into normalized internal data structures ([`NormalizedResponse`](file:///home/eins0fx/development/hermes-agent-port/agent/transports/types.py#L90-L151) and [`ToolCall`](file:///home/eins0fx/development/hermes-agent-port/agent/transports/types.py#L18-L77)), preserving protocol-specific sidecars (`extra_content`, `reasoning_content`, `reasoning_details`) in `provider_data`.
2. **In-Flight Turn Loop & Replay**: Constructing the canonical assistant message ([`build_assistant_message`](file:///home/eins0fx/development/hermes-agent-port/agent/chat_completion_helpers.py#L2354-L2608)) and appending it alongside tool results in the active turn message sequence ([`messages`](file:///home/eins0fx/development/hermes-agent-port/agent/conversation_loop.py#L7827-L8083)).
3. **Outbound Wire Projection & Sanitization**: Transforming internal message history into schema-valid payloads for the target provider immediately before HTTP dispatch ([`convert_messages`](file:///home/eins0fx/development/hermes-agent-port/agent/transports/chat_completions.py#L346-L541), [`apply_reasoning_content_policy`](file:///home/eins0fx/development/hermes-agent-port/agent/message_sanitization.py#L925-L1014)).

In the Rust port, work is currently split across two complementary efforts:
- **Main Agent**: Preserving tool-call replay state across intermediate iterations of the native turn loop ([`run_tool_loop_with_content`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/native_tools.rs#L168-L238)).
- **Claude (Parallel Task)**: Porting [`convert_messages`](file:///home/eins0fx/development/hermes-agent-port/agent/transports/chat_completions.py#L346-L541) into [`rust/crates/hermes-gateway/src/chat_message_projection.rs`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/chat_message_projection.rs).

This audit focuses on three critical runtime data surfaces:
1. **Gemini thought signatures & `extra_content`**
2. **Raw function arguments (string vs decoded AST)**
3. **Assistant reasoning & text content**

---

## 2. Detailed Python Source Map

### 2.1 Gemini Thought Signatures & `extra_content`
- **Ingest Normalization** ([`agent/transports/chat_completions.py:1026-1057`](file:///home/eins0fx/development/hermes-agent-port/agent/transports/chat_completions.py#L1026-L1057)):
  When a Gemini model returns a tool call through Google's OpenAI-compatible endpoint or OpenRouter, it attaches `extra_content` (containing `{"google": {"thought_signature": "..."}}` or `{"thought_signature": "..."}`).
  `ChatCompletionsTransport.normalize_response` inspects `getattr(tc, "extra_content", None)` and falls back to `tc.model_extra.get("extra_content")` (Pydantic SDK compatibility). If present, it executes `model_dump()` and stores the dictionary in `tc_provider_data["extra_content"]`, wrapping it in [`ToolCall(..., provider_data=tc_provider_data)`](file:///home/eins0fx/development/hermes-agent-port/agent/transports/types.py#L38).
- **Assistant Message Construction** ([`agent/chat_completion_helpers.py:2594-2605`](file:///home/eins0fx/development/hermes-agent-port/agent/chat_completion_helpers.py#L2594-L2605)):
  `build_assistant_message` pulls `extra = getattr(tool_call, "extra_content", None)`. If present, it copies `tc_dict["extra_content"] = extra`. The resulting assistant turn stored in `messages` retains `extra_content` on each tool call.
- **Wire Projection & Gated Stripping** ([`agent/transports/chat_completions.py:313-326, 386-388, 514-532`](file:///home/eins0fx/development/hermes-agent-port/agent/transports/chat_completions.py#L313-L326)):
  [`_model_consumes_thought_signature(model)`](file:///home/eins0fx/development/hermes-agent-port/agent/transports/chat_completions.py#L313-L326) checks `"gemini" in m or "gemma" in m`.
  - If the outgoing model **is** Gemini/Gemma: `extra_content` is preserved verbatim on the wire. Gemini 3 thinking models reject follow-up tool replays with HTTP 400 if `extra_content` / `thought_signature` is omitted ([`gemini_native_adapter.py:353-359`](file:///home/eins0fx/development/hermes-agent-port/agent/gemini_native_adapter.py#L353-L359)).
  - If the outgoing model **is not** Gemini/Gemma: `extra_content` is stripped copy-on-write. Strict providers (Mistral, Fireworks) reject payloads containing it with `Extra inputs are not permitted, field: 'messages[N].tool_calls[M].extra_content'`.

### 2.2 Raw Function Arguments
- **Ingest Normalization** ([`agent/transports/chat_completions.py:1025, 1050-1054`](file:///home/eins0fx/development/hermes-agent-port/agent/transports/chat_completions.py#L1025)):
  `function_arguments = getattr(tc_function, "arguments", None)`. `ToolCall.arguments` stores the verbatim JSON string (defaulting to `"{}"` if None). The raw string is never parsed or re-serialized during normalization.
- **In-History Preservation** ([`agent/chat_completion_helpers.py:2572-2593`](file:///home/eins0fx/development/hermes-agent-port/agent/chat_completion_helpers.py#L2572-L2593)):
  `build_assistant_message` assigns `"arguments": tool_call.function.arguments`. Crucially, arguments are **not** redacted or re-dumped here. Masking or formatting alterations at this step corrupt the model's replayable context (e.g. invalidating tokens or breaking prefix cache stability).
- **In-Flight Validation & Repair** ([`agent/conversation_loop.py:7849-7876`](file:///home/eins0fx/development/hermes-agent-port/agent/conversation_loop.py#L7849-L7876)):
  The agent loop validates JSON syntax using `json.loads(args)`. Empty or whitespace-only strings are normalized to `"{}"` (`tc.function.arguments = "{}"`). If a provider returned a Python dict/list instead of a string, it is stringified via `json.dumps(args)`. If arguments are truncated due to output limits, the loop halts without executing the damaged call.
- **Send-Path Canonicalization vs History Immutability** ([`agent/conversation_loop.py:1431-1545`](file:///home/eins0fx/development/hermes-agent-port/agent/conversation_loop.py#L1431-L1545)):
  Before dispatching outbound requests, `_canonicalize_api_tool_calls` structurally clones the message (`_clone_message_for_send`) and applies `json.dumps(json.loads(arg_str), separators=(",", ":"), sort_keys=True)`. This canonicalization runs **only on the outgoing copy (`api_messages`)**; the persisted/in-memory history remains byte-stable.

### 2.3 Assistant Reasoning & Text Content
- **Ingest Normalization** ([`agent/transports/chat_completions.py:1068-1127`](file:///home/eins0fx/development/hermes-agent-port/agent/transports/chat_completions.py#L1068-L1127)):
  - Captures `reasoning` (generic), `reasoning_content` (DeepSeek, Moonshot/Kimi), and `reasoning_details` (OpenRouter structured thinking).
  - Captures `content` and handles `refusal` promotion when content is empty.
- **Assistant Message Assembly** ([`agent/chat_completion_helpers.py:2361-2509`](file:///home/eins0fx/development/hermes-agent-port/agent/chat_completion_helpers.py#L2361-L2509)):
  - Extracts reasoning text; strips inline `<think>...</think>` tags from `_san_content` so thoughts do not leak into text channels.
  - Preserves legitimate narration text alongside tool calls: `msg["content"] = _san_content`.
  - Enforces thinking pad for require-side providers ([`agent/chat_completion_helpers.py:2449-2462`](file:///home/eins0fx/development/hermes-agent-port/agent/chat_completion_helpers.py#L2449-L2462)): if `assistant_tool_calls` is present and `_needs_thinking_reasoning_pad()` is true, missing `reasoning_content` is padded with `reasoning_text or " "` (single space, because DeepSeek V4 Pro rejects `""`).
- **Send-Path Replay Separation** ([`agent/conversation_loop.py:2581-2591`](file:///home/eins0fx/development/hermes-agent-port/agent/conversation_loop.py#L2581-L2591), [`agent/message_sanitization.py:925-1014`](file:///home/eins0fx/development/hermes-agent-port/agent/message_sanitization.py#L925-L1014)):
  - `agent._copy_reasoning_content_for_api`: copies `reasoning_content` to the outgoing message for thinking-mode models (DeepSeek / Kimi / MiMo); strips it when sending to strict APIs (Mistral, Fireworks, Cerebras, Groq).
  - `api_msg.pop("reasoning")`: `reasoning` is an **internal trajectory field** and is **never** sent over the wire.
  - `api_msg.pop("finish_reason")`: stripped before wire dispatch.

---

## 3. Comparative Audit: Python vs. `native_tools.rs`

### 3.1 Initial Rust Baseline vs. In-Flight Main Agent Draft
In the initial baseline of [`rust/crates/hermes-gateway/src/native_tools.rs`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/native_tools.rs):
- `assistant_tool_calls_msg` hardcoded `"content": null`.
- `arguments` was parsed into a `Value` and re-serialized via `c.arguments.to_string()`, corrupting custom formatting and replacing parse failures with `{}`.
- `extra_content` was completely omitted.
- `reasoning`, `reasoning_content`, and `reasoning_details` were completely omitted.

In the in-flight main agent revision ([`native_tools.rs:51-61, 100-154`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/native_tools.rs#L51-L61)):
- Added `Step::ToolCalls { calls: Vec<ToolCall>, assistant_message: Value }`.
- In `parse_message_step`:
  - Captures `message.get("content").cloned().unwrap_or(Value::Null)` into `assistant_message["content"]`.
  - Preserves incoming `call["function"]["arguments"]` as a raw cloned value (defaulting to `json!("{}")` when missing/null).
  - Preserves `call["extra_content"]` on `replay["extra_content"]`.
  - Copies `reasoning`, `reasoning_content`, and `reasoning_details` into `assistant_message` if present on `message`.
  - In `run_tool_loop_with_content`: pushes `assistant_message` directly to `messages`.

### 3.2 Detailed Parity Matrix

| Feature / Field | Python Agent Runtime | `native_tools.rs` (Initial) | `native_tools.rs` (In-Flight Draft) | Status & Residual Risk |
| :--- | :--- | :--- | :--- | :--- |
| **Gemini `extra_content` capture** | Captured in `ToolCall.provider_data` and copied to `assistant_msg["tool_calls"][i]["extra_content"]`. | Dropped entirely. | Cloned into `replay["extra_content"]`. | **Preserved**. Gemini 3 replay works in isolation. |
| **Gemini `extra_content` wire gating** | Stripped copy-on-write if model is not Gemini/Gemma (`convert_messages`). | N/A (was dropped). | Sent unconditionally on subsequent rounds. | **Runtime Hazard**: Fireworks/Mistral will 400 on round 2 unless filtered by `chat_message_projection`. |
| **Tool call arguments for execution** | Parsed to Python dict via `json.loads` for tool dispatch. | Decoded to `Value` leniently (`unwrap_or_else(|| json!({}))`). | Decoded to `Value` leniently. | **Preserved**. Execution receives parsed AST. |
| **Tool call arguments for replay** | Verbatim raw string preserved in history; normalized on send copy. | Re-serialized from parsed AST via `.to_string()`. | Raw string cloned from `function["arguments"]`. | **Preserved**. Formatting and key order preserved. |
| **Empty argument string normalization** | Empty string or whitespace coerced to `"{}"`. | Converted to `"{}"` via unwrap fallback. | Kept as `""` if input is `""`. | **Residual Risk**: Some strict engines reject empty argument strings with 400. |
| **Object-form argument coercion** | Coerced to JSON string via `json.dumps(args)` if SDK returns dict. | Handled via serde Value mapping. | Kept as `Value::Object` if provider returned object. | **Residual Risk**: OpenAI wire schema strictly requires `arguments` to be a string. |
| **Assistant text narration** | Stored in `content` alongside tool calls; fallback captured. | Hardcoded to `null`. | Cloned from `message["content"]`. | **Preserved**. Narration text survives round-trip. |
| **`reasoning_content` capture** | Stored on message; padded with `" "` for DeepSeek/Kimi if missing. | Dropped entirely. | Cloned into `assistant_message["reasoning_content"]`. | **Partially Preserved**. Cloned when present, but missing pad on unannotated tool calls. |
| **Internal `reasoning` field** | Kept in trajectory history, but **strictly popped** (`api_msg.pop("reasoning")`) before wire dispatch. | Dropped entirely. | Cloned into `assistant_message["reasoning"]`. | **Runtime Hazard**: Leaks internal field to wire; strict providers (Mistral) reject with 400/422. |
| **`reasoning_details` capture** | Preserved verbatim for OpenRouter / Anthropic continuity. | Dropped entirely. | Cloned into `assistant_message["reasoning_details"]`. | **Preserved**. |

---

## 4. Distinguishing Confirmed Runtime Loss from Future Persistence Work

To keep the implementation clean and avoid premature complexity, runtime concerns within the active tool loop must be strictly separated from multi-turn session persistence:

```
┌─────────────────────────────────────────────────────────────────────────────┐
│                       ACTIVE TURN: IN-FLIGHT RUNTIME                        │
│                                                                             │
│   NativeAgentClient::step                                                   │
│        │                                                                    │
│        ▼                                                                    │
│   parse_message_step(choices[0].message)                                    │
│        │                                                                    │
│        ├─► calls: Vec<ToolCall> (args: Value) ──► tool.call(&args)          │
│        │                                                                    │
│        └─► assistant_message (args: String, extra_content, reasoning_*)     │
│                 │                                                           │
│                 ▼                                                           │
│            messages.push(assistant_message)                                 │
│            messages.push(tool_result_message)                               │
│                 │                                                           │
│                 ▼                                                           │
│            Next iteration: chat_message_projection::convert(&messages, ...) │
│                 │                                                           │
│                 ▼                                                           │
│            POST /chat/completions (wire-valid schema)                       │
└─────────────────────────────────────────────────────────────────────────────┘
                                      │
                         Turn finishes (Step::Final)
                                      │
                                      ▼
┌─────────────────────────────────────────────────────────────────────────────┐
│                       SESSION_DB: FUTURE PERSISTENCE                        │
│                                                                             │
│   - HistoryMessage currently only models (role: String, content: String)    │
│   - Persisting tool_calls, tool_call_id, tool_name to SQLite messages table  │
│   - Persisting reasoning, reasoning_content, extra_content sidecars         │
│   - Reconstructing multi-turn history on session resume                     │
│   - Trajectory FTS5 search indexing and context compaction                  │
└─────────────────────────────────────────────────────────────────────────────┘
```

### 4.1 Confirmed Runtime Loss (Active Turn Scope)
These are failures that occur **immediately during the current user request** across iterations of `run_tool_loop_with_content`:
1. **Gemini 3 Thinking Crash**: Omitting `extra_content` (`thought_signature`) causes Gemini 3 thinking models to reject the round 2 tool replay with HTTP 400 (`Missing thought signature`).
2. **Fireworks / Mistral 422 Rejection**: Passing `extra_content` to a non-Gemini provider on round 2 causes HTTP 422 (`Extra inputs are not permitted`).
3. **DeepSeek v4 / Moonshot Kimi 400 Rejection**: Omitting `reasoning_content` on an assistant tool-call turn causes DeepSeek v4 and Kimi K3 to reject round 2 replay with HTTP 400 (`The reasoning_content in the thinking mode must be passed back to the API`).
4. **Internal `reasoning` Leak**: Sending top-level `"reasoning"` in `assistant_message` causes strict OpenAI proxies (Mistral, Fireworks) to reject the request with HTTP 400/422.
5. **Argument Deserialization/Reserialization Drift**: Re-serializing tool arguments from decoded `Value` can perturb floating-point numbers, strip key ordering, and destroy unparseable tool arguments that the model was nudged to repair.
6. **Loss of Assistant Narration**: Discarding `message.content` when tool calls are present loses the model's user-facing explanation accompanying the action.

### 4.2 Future Persistence Work (SessionDB / Cross-Turn Scope)
These concerns do **not** affect intermediate rounds of an active tool loop, but govern saving and loading turns across conversation sessions:
1. **[`HistoryMessage`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/session_db.rs#L69-L74) Schema Expansion**: Currently, `HistoryMessage` contains only `pub role: String` and `pub content: String`. It has no fields for `tool_calls`, `tool_call_id`, `tool_name`, or `reasoning`.
2. **[`build_messages_with_content`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/native_agent.rs#L43-L54)**: Filters history strictly by `role in ("user", "assistant", "system")` and outputs `{ "role": m.role, "content": m.model_content() }`. It cannot currently resurrect historical tool interactions from past turns.
3. **SQLite `messages` Table Columns**: Writing `tool_calls` JSON, `tool_call_id`, and `tool_name` into the SQLite database rows, firing FTS5 triggers, and handling DB migrations.
4. **Context Compaction & Secret Redaction**: Pruning older tool turns during context window overflow and redacting sensitive credentials prior to SQLite disk writes.

---

## 5. Minimal Faithful Rust Preservation Path

To achieve parity with Python without introducing circular dependencies or premature persistence complexity, the minimal faithful implementation requires two coordinated adjustments:

### 5.1 In `native_tools.rs`: Normalize and Sanitize `assistant_message`
In [`parse_message_step`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/native_tools.rs#L100):
1. **Ensure Argument String Invariant**:
   If `function.get("arguments")` is an empty string `""` or whitespace, normalize to `json!("{}")`. If `arguments` is a JSON Object (e.g. from non-standard providers), convert it via `Value::String(obj.to_string())`.
2. **Do Not Attach Internal `reasoning`**:
   Only attach `reasoning_content` and `reasoning_details` to `assistant_message`. Do **not** copy the generic `reasoning` key into `assistant_message`, as `reasoning` is an internal trajectory artifact and is rejected by strict Chat Completions APIs (Python lines [`agent/conversation_loop.py:2587-2588`](file:///home/eins0fx/development/hermes-agent-port/agent/conversation_loop.py#L2587-L2588)).
3. **Preserve `extra_content` Verbatim**:
   Retain `replay["extra_content"] = extra.clone()`. The in-memory turn sequence must retain this metadata so that Gemini targets receive it.

```rust
// In native_tools.rs: parse_message_step
let arguments_val = match function.get("arguments") {
    Some(Value::String(s)) if s.trim().is_empty() => json!("{}"),
    Some(Value::String(s)) => Value::String(s.clone()),
    Some(v @ (Value::Object(_) | Value::Array(_))) => Value::String(v.to_string()),
    _ => json!("{}"),
};

let mut replay = json!({
    "id": call["id"],
    "type": "function",
    "function": {
        "name": function["name"],
        "arguments": arguments_val,
    }
});
if let Some(extra) = call.get("extra_content").filter(|v| !v.is_null()) {
    replay["extra_content"] = extra.clone();
}
```

### 5.2 In `native_agent.rs`: Wire Projection via Claude's `chat_message_projection`
In [`NativeAgentClient::step`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/native_agent.rs#L501):
Before sending `messages` to the HTTP endpoint, pass `messages` through [`chat_message_projection::convert`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/chat_message_projection.rs):

```rust
// In native_agent.rs: NativeAgentClient::step
let wire_messages = crate::chat_message_projection::convert(messages, &self.model);
let mut body = json!({
    "model": self.model,
    "messages": wire_messages,
    "stream": false,
});
```

This ensures:
- When calling a Gemini model: `extra_content` is retained on the wire.
- When calling a strict non-Gemini model (Fireworks, Mistral): `extra_content` is stripped copy-on-write without altering the internal `messages` vector.
- Any empty `tool_calls: []` or `tool_calls: null` are stripped.
- Internal sidecars and `_`-prefixed keys never reach the wire.
