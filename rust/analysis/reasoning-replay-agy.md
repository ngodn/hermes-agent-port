> Historical helper report. Main integration retained the 62 hostname,
> family and apply cases, removed unconsumed fallback/compaction helpers and
> their tests, and removed the blanket dead-code allowance. Current validation
> is in [tool-call-replay-verification.md](tool-call-replay-verification.md).

# reasoning_replay.rs — Port Analysis & Verification

Pure Python source port of the single-owner `reasoning_content` policy and provider family classification from [`agent/message_sanitization.py`](file:///home/eins0fx/development/hermes-agent-port/agent/message_sanitization.py#L813-L1063) and URL parsing helpers from [`utils.py`](file:///home/eins0fx/development/hermes-agent-port/utils.py#L867-L970).

## Origin & Context

Before the F4 audit consolidation in Python, reasoning replay policy was fragmented across multiple incident fixes:
- 2b3a4f0af8: Strip `reasoning_content` for strict providers (Mistral, Groq, Cerebras, SambaNova) which return HTTP 400/422 ("Extra inputs are not permitted").
- b5495db701: Re-pad `reasoning_content` for require-side providers that reject turns without it.
- 94b3131be7 / 9a9f8a6d99: Kimi thinking pad requirements.
- #15250 / #17341: DeepSeek V4 Pro requiring non-empty space padding `" "` (rejecting `""` with HTTP 400).
- #15748: Cross-provider poisoned history defense (preventing CoT leakage when falling back from MiniMax/other models to DeepSeek/Kimi).
- #84371: Shared wire-truth predicate `stale_thinking_reaches_wire` across compaction and tail-budget estimators.

This module ports that consolidated policy directly into `hermes-gateway`.

## Public Surface

- `pub fn needs_echo(provider: &str, model: &str, base_url: &str) -> bool`:
  Authoritative predicate determining if the active route requires `reasoning_content` echo-back. Primary entry point called by [`NativeAgentClient::apply_provider_extras`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/native_agent.rs#L283-L286).
- `pub fn apply(message: &mut serde_json::Value, needs_echo: bool)`:
  Mutates an outbound wire message in-place, adding/preserving or stripping `reasoning_content` while preserving JSON key insertion order.
- `pub fn apply_reasoning_content_policy(source_msg: &serde_json::Value, api_msg: &mut serde_json::Value, needs_thinking_pad: bool)`:
  Copies reasoning fields from `source_msg` to `api_msg`, matching the Python signature.
- `pub fn reasoning_echo_family(provider: &str, model: &str, base_url: &str) -> Option<&'static str>`:
  Classifies the route into `"kimi"`, `"deepseek"`, `"mimo"`, or `None`.
- `pub fn matches_reasoning_echo_family(family: &str, provider: &str, model: &str, base_url: &str) -> bool`:
  Independent family predicate permitting overlap testing.
- `pub fn reapply_reasoning_echo(api_messages: &mut [serde_json::Value], needs_thinking_pad: bool) -> usize`:
  Reconciles already-constructed message arrays when mid-turn provider fallback switches between strict and require-side providers.
- `pub fn stale_thinking_reaches_wire(api_mode: &str, provider: &str, model: &str, base_url: &str) -> bool`:
  Single wire-truth predicate for compaction and tail budgets.
- `pub fn base_url_hostname(base_url: &str) -> String` and `pub fn base_url_host_matches(base_url: &str, domain: &str) -> bool`:
  URL hostname extraction and domain matching.

## Preserved Behaviors

### 1. Provider Family Rule Table (`_REASONING_ECHO_RULES`)
The table is evaluated in exact priority order (`kimi` -> `deepseek` -> `mimo`):
- **Kimi**:
  - Raw provider matches: `kimi-coding`, `kimi-coding-cn`. Raw provider matching is case-sensitive (e.g. `KIMI-CODING` does not match).
  - Hosts: `api.kimi.com`, `moonshot.ai`, `moonshot.cn`.
  - Model substrings: None. Omitted intentionally because aggregators re-exporting Kimi models reject the echo.
- **DeepSeek**:
  - Lowered provider: `deepseek`.
  - Model substring (lowered): `deepseek`.
  - Host: `api.deepseek.com`.
- **MiMo**:
  - Lowered provider: `xiaomi`.
  - Model substring (lowered): `mimo`.
  - Hosts: `api.xiaomimimo.com`, `xiaomimimo.com`.
- **Strict / Indifferent (Everyone Else)**:
  - Mistral, Cerebras, Groq, SambaNova, OpenAI, etc. receive `None` / `false`.

### 2. URL Hostname and Domain Matching
Reuses [`crate::local_probe::urlparse_hostname`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/local_probe.rs#L637) and [`crate::python_value::python_whitespace`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/python_value.rs#L180):
- Strips leading/trailing Python whitespace (including ASCII whitespace and C0/Unicode information separators `\x1c..\x1f`).
- Wraps bare hostnames in `//` before parsing.
- Strips trailing dots from hostnames.
- `base_url_host_matches` matches `hostname == domain || hostname.ends_with("." + domain)`, preventing substring false-positives (such as `evil.com/moonshot.ai` or `moonshot.ai.evil`).

### 3. Outbound Message Mutation & Poisoned History Protection
- **Role Guard**: Non-assistant messages (user, system, tool) are never modified.
- **Existing `reasoning_content`**:
  - Require-side (`needs_echo=true`): non-empty strings are preserved verbatim; empty strings `""` are upgraded to `" "` (satisfying DeepSeek V4 Pro #17341).
  - Strict-side (`needs_echo=false`): removed via `shift_remove` to avoid HTTP 400/422.
- **Cross-Provider Poisoned History (#15748)**:
  - If `needs_echo=true` and an assistant message contains `tool_calls` (evaluated with [`crate::python_value::truthy`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/python_value.rs#L4)) and a non-empty `reasoning` field, but no `reasoning_content`, the `reasoning` was injected by a prior provider (e.g. MiniMax).
  - Padded with `" "` rather than promoting the foreign chain of thought.
- **Healthy Session Promotion**:
  - When `tool_calls` is absent (or falsy) and `reasoning` is a non-empty string, `reasoning` is promoted to `reasoning_content` on require-side.
- **Bare Assistant Turn**:
  - Require-side assistant turns without reasoning content receive a `" "` pad.
- **Null / Compaction Clean-Up**:
  - Explicit `null` or non-string `reasoning_content` is never sent to strict providers.

### 4. Insertion Order Preservation
The `hermes-gateway` crate enables `features = ["preserve_order"]` on `serde_json`, backing `Map` with `IndexMap`.
- Updating existing keys preserves their position in the JSON object.
- Removing keys uses `shift_remove`, preserving relative order of other keys.
- Newly inserted keys append to the end.
- Matches CPython 3.12 `dict` insertion order.

### 5. Main Agent Integration
[`NativeAgentClient::apply_provider_extras`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/native_agent.rs#L282-L296) integrates the policy:
```rust
let needs_echo = self.reasoning_echo || crate::reasoning_replay::needs_echo(
    self.provider_profile.as_ref().map(|p| p.name.as_str()).unwrap_or(""),
    &self.model, &self.base_url,
);
let mut wire = messages.clone();
for message in &mut wire {
    crate::reasoning_replay::apply(message, needs_echo);
    if let Some(object) = message.as_object_mut() {
        object.shift_remove("reasoning");
        object.shift_remove("finish_reason");
    }
}
body["messages"] = Value::Array(crate::chat_message_projection::convert(&wire, &self.model));
```
Stored history retains `reasoning` and `finish_reason` across turns, while outbound wire messages have trajectory keys stripped after policy application.

## Golden Generation & Verification

`rust/tools/gen_reasoning_replay_goldens.py` parses `utils.py` and `agent/message_sanitization.py` via Python `ast`, compiles the AST nodes into isolated modules, and executes them with CPython 3.12.13.

Generated fixtures in `rust/tools/reasoning-replay-goldens.json` cover:
- 18 URL hostname & host matching cases.
- 24 echo-family classification cases.
- 20 `apply` / `apply_reasoning_content_policy` cases (testing both separate output dicts and in-place mutation).
- 5 `reapply_reasoning_echo` cases.
- 9 `stale_thinking_reaches_wire` cases.

### Test Verification

```bash
# Verify goldens against Python AST execution:
mise x python@3.12.13 -- python rust/tools/gen_reasoning_replay_goldens.py --check

# Compile hermes-gateway:
cargo check --bin hermes-gateway

# Run inline reasoning_replay tests (all 7 passed):
cargo test --bin hermes-gateway reasoning_replay

# Run native_agent tests (all 18 passed, including reasoning opt-in & projection):
cargo test --bin hermes-gateway native_agent

# Run full hermes-gateway test suite (all 1142 passed):
cargo test --bin hermes-gateway

# Clippy verification (clean, 0 warnings):
cargo clippy --bin hermes-gateway
```
