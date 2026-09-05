# Native Bundled Base-Profile Loading Source Review

This document provides an exhaustive, read-only architectural audit and source review of the native bundled base-profile loader in the Hermes Rust gateway port. It inspects:
1. The profile generator script: [`rust/tools/gen_bundled_base_profiles.py`](../../rust/tools/gen_bundled_base_profiles.py).
2. The intermediate JSON artifact: [`rust/tools/bundled-base-profiles.json`](../../rust/tools/bundled-base-profiles.json).
3. The native Rust registry loader: [`ProviderRegistry::register_bundled_base_profiles`](../../rust/crates/hermes-gateway/src/provider_registry.rs#L314-L337).
4. Each of the 13 selected bundled provider modules in [`plugins/model-providers/`](../../plugins/model-providers).

---

## 1. Scope, Authority, and Architectural Boundaries

### 1.1 Scope of Review
The native bundled base-profile loader is designed to translate and embed fully declarative provider profiles from Python plugin directories directly into the compiled Rust gateway binary. This mechanism eliminates the runtime Python interpreter dependency for provider definitions whose behavior does not exceed the standard [`ProviderProfile`](../../providers/base.py#L38-L112) dataclass contract.

```
┌────────────────────────────────────────┐
│  Python Plugin Modules (13 Selected)   │
│  plugins/model-providers/<module>/     │
└──────────────────┬─────────────────────┘
                   │
                   ▼  AST parsing, type checks, temperature serialization
┌────────────────────────────────────────┐
│   rust/tools/gen_bundled_base_profiles.py│
└──────────────────┬─────────────────────┘
                   │
                   ▼  Generates verified static JSON (13 modules, 17 profiles)
┌────────────────────────────────────────┐
│  rust/tools/bundled-base-profiles.json │
└──────────────────┬─────────────────────┘
                   │
                   ▼  include_str! compile-time embedding & version replacement
┌────────────────────────────────────────┐
│ ProviderRegistry::register_bundled_... │
│ rust/crates/hermes-gateway/...         │
└──────────────────┬─────────────────────┘
                   │
                   ▼  Registers Arc<RwLock<ProviderProfile>> into registry state
┌────────────────────────────────────────┐
│      Live ProviderRegistry State       │
│  (17 profiles indexed by name/alias)   │
└────────────────────────────────────────┘
```

### 1.2 Boundary Definitions and Non-Goals
> [!IMPORTANT]
> **Strict Non-Goals**:
> 1. **No Dynamic Plugin Discovery**: This loader does not scan filesystem directories at runtime. It does not discover or load user plugins from `$HERMES_HOME/plugins/` or `$HERMES_HOME/plugins/model-providers/`, nor does it inspect entry points from installed Python packages.
> 2. **No Non-Base Provider Hooks**: This loader does not implement or claim coverage for plugins requiring custom provider behavior, such as custom transport clients (e.g. [`plugins/model-providers/copilot-acp`](../../plugins/model-providers/copilot-acp/__init__.py)), custom catalog fetch logic (e.g. [`plugins/model-providers/anthropic`](../../plugins/model-providers/anthropic/__init__.py)), custom header/body transformations (e.g. [`plugins/model-providers/openrouter`](../../plugins/model-providers/openrouter/__init__.py)), or OAuth credentials exchange (e.g. [`plugins/model-providers/qwen-oauth`](../../plugins/model-providers/qwen-oauth/__init__.py)).
> 3. **No Execution Guarantee for Non-Standard Protocols**: While declarative attributes like `api_mode = "codex_responses"` or `auth_type = "oauth_external"` are faithfully preserved in the registered profile structs, registering these profiles does not by itself supply the protocol handlers, token refresh services, or wire decoders necessary to execute inference requests against those endpoints.

---

## 2. Pipeline Mechanics and Implementation Analysis

### 2.1 Code Generator: [`gen_bundled_base_profiles.py`](../../rust/tools/gen_bundled_base_profiles.py)

The Python generation script acts as an offline build verification tool and compiler from Python plugin declarations to Rust-compatible JSON data.

```python
# rust/tools/gen_bundled_base_profiles.py
MODULES = ["alibaba", "alibaba-coding-plan", "arcee", "azure-foundry", "fireworks", "gmi",
           "huggingface", "kilocode", "novita", "openai-codex", "stepfun", "xai", "xiaomi"]
VERSION = "__HERMES_NATIVE_VERSION__"
```

#### Guard Invariants Enforced by the Generator:
1. **AST Purity Check ([lines 32-34](../../rust/tools/gen_bundled_base_profiles.py#L32-L34))**:
   ```python
   tree = ast.parse(path.read_text())
   if any(isinstance(node, (ast.ClassDef, ast.FunctionDef, ast.AsyncFunctionDef)) for node in tree.body):
       raise ValueError(f"{name} now defines behavior requiring a native hook port")
   ```
   Ensures that none of the candidate modules define custom classes (subclasses of [`ProviderProfile`](../../providers/base.py#L38-L112)), functions, or async functions. Top-level function and class definitions fail the generator; this AST check does not prove the absence of arbitrary procedural logic.
2. **Isolated Import Execution ([lines 22-37](../../rust/tools/gen_bundled_base_profiles.py#L22-L37))**:
   The script creates stub module environments for `providers`, `providers.base`, and `hermes_cli`. It intercepts calls to `providers.register_provider` using a local accumulator list `registrations.append`.
3. **Exact Type Validation ([lines 39-41](../../rust/tools/gen_bundled_base_profiles.py#L39-L41))**:
   ```python
   if type(profile) is not base.ProviderProfile:
       raise TypeError(f"{name}: inherited provider behavior requires a native implementation")
   ```
   Requires exact object identity with the base dataclass; subclasses are rejected even if they were instantiated dynamically.
4. **Attribute Set Integrity ([lines 42-43](../../rust/tools/gen_bundled_base_profiles.py#L42-L43))**:
   ```python
   if set(vars(profile)) != set(profile.__dataclass_fields__):
       raise ValueError(f"{name}: runtime extensions require a native implementation")
   ```
   Guarantees that no ad-hoc monkey patching or dynamic instance attributes were attached to the profile instance during module execution.
5. **Temperature Tri-State Serialization ([lines 45-48](../../rust/tools/gen_bundled_base_profiles.py#L45-L48))**:
   ```python
   temperature = profile.fixed_temperature
   row["fixed_temperature"] = ({"kind": "inherit"} if temperature is None else
                               {"kind": "omit"} if temperature is base.OMIT_TEMPERATURE else
                               {"kind": "fixed", "value": temperature})
   ```
   Serializes Python's sentinel-based temperature semantics into an internally tagged JSON object compatible with Rust's [`Temperature`](../../rust/crates/hermes-gateway/src/provider_registry.rs#L14-L19) enum (`Inherit`, `Omit`, `Fixed(Value)`).
6. **Version Sentinel Injection ([lines 23-24](../../rust/tools/gen_bundled_base_profiles.py#L23-L24))**:
   By injecting `hermes_cli.__version__ = "__HERMES_NATIVE_VERSION__"`, any module formatting `_HERMES_VERSION` into headers produces a deterministic token that Rust replaces at runtime.

### 2.2 Intermediate Data: [`bundled-base-profiles.json`](../../rust/tools/bundled-base-profiles.json)

The output file contains an array of 13 module entries representing 17 total registered profiles:
- Root schema: `[ { "module": string, "profiles": [ ProviderProfile, ... ] }, ... ]`
- Total modules: 13
- Total profiles: 17
- Profiles per module:
  - `alibaba`: 4 profiles
  - `alibaba-coding-plan`: 2 profiles
  - Remaining 11 modules: 1 profile each

All fields match the Rust [`ProviderProfile`](../../rust/crates/hermes-gateway/src/provider_registry.rs#L25-L50) struct definition with strict serde field enforcement (`#[serde(deny_unknown_fields)]`).

### 2.3 Rust Registry Engine: [`register_bundled_base_profiles`](../../rust/crates/hermes-gateway/src/provider_registry.rs#L314-L337)

The Rust loader consumes the embedded JSON artifact via compile-time inclusion:

```rust
// rust/crates/hermes-gateway/src/provider_registry.rs
pub fn register_bundled_base_profiles(&self, hermes_version: &str) -> Vec<String> {
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Module {
        module: String,
        profiles: Vec<ProviderProfile>,
    }
    let modules: Vec<Module> =
        serde_json::from_str(include_str!("../../../tools/bundled-base-profiles.json"))
            .expect("valid embedded native provider definitions");
    let mut loaded = Vec::new();
    for module in modules {
        for mut profile in module.profiles {
            for value in profile.default_headers.values_mut() {
                if let Value::String(header) = value {
                    *header = header.replace("__HERMES_NATIVE_VERSION__", hermes_version);
                }
            }
            self.register(Arc::new(RwLock::new(profile)));
        }
        loaded.push(module.module);
    }
    loaded
}
```

#### Registration Semantics and Invariants:
1. **Compile-Time Embedding**: The JSON is baked into the gateway binary via `include_str!`, incurring zero runtime disk I/O or filesystem parsing overhead.
2. **Dynamic Version Interpolation**: Header string values containing `"__HERMES_NATIVE_VERSION__"` are replaced in-memory with the runtime `hermes_version` provided by the caller before insertion into the registry.
3. **Locking & Thread Safety**: Each [`ProviderProfile`](../../rust/crates/hermes-gateway/src/provider_registry.rs#L25-L50) is wrapped in `Arc<RwLock<ProviderProfile>>` and registered via [`ProviderRegistry::register`](../../rust/crates/hermes-gateway/src/provider_registry.rs#L339-L356). Writing acquires the registry lock, invalidates `list_cache`, registers the canonical name into `state.profiles`, and registers all aliases into `state.aliases`.
4. **Return Value Contract**: The method returns `Vec<String>` containing the list of loaded module directory names (length 13), *not* the canonical profile names (length 17). Callers must not assume a 1:1 correspondence between the returned strings and registered provider names.

---

## 3. Exhaustive Source Audit of the 13 Selected Modules

Each module was inspected against the following four criteria:
- **Custom behavior beyond `ProviderProfile`**: Subclasses, hook overrides (`prepare_messages`, `build_extra_body`, `create_client`, etc.), or procedural execution.
- **Version-dependent headers**: References to `hermes_cli.__version__` or other dynamic header generation.
- **Multiple registrations**: Whether the module registers more than one profile.
- **Unsupported assumptions**: Unstated runtime requirements, protocol mismatches, external auth dependencies, or endpoint oddities.

---

### 3.1 `alibaba`
- **Source Files**: [`plugins/model-providers/alibaba/__init__.py`](../../plugins/model-providers/alibaba/__init__.py), [`plugins/model-providers/alibaba/plugin.yaml`](../../plugins/model-providers/alibaba/plugin.yaml)
- **Custom Behavior Beyond `ProviderProfile`**:
  - **None**. The module only constructs four [`ProviderProfile`](../../providers/base.py#L38-L112) instances and registers them.
- **Version-Dependent Headers**:
  - **None**. `default_headers` is empty across all four profiles.
- **Multiple Registrations**:
  - **YES (4 profiles)**:
    1. `alibaba`:
       - Aliases: `("dashscope", "alibaba-cloud", "qwen-dashscope")`
       - Env vars: `("DASHSCOPE_API_KEY",)`
       - Base URL: `https://dashscope-intl.aliyuncs.com/compatible-mode/v1`
    2. `alibaba-cn`:
       - Aliases: `("dashscope-cn", "alibaba-cloud-cn")`
       - Env vars: `("DASHSCOPE_API_KEY", "DASHSCOPE_CN_BASE_URL")`
       - Base URL: `https://dashscope.aliyuncs.com/compatible-mode/v1`
    3. `alibaba-token-plan`:
       - Aliases: `("dashscope-token-plan",)`
       - Env vars: `("ALIBABA_TOKEN_PLAN_API_KEY", "ALIBABA_TOKEN_PLAN_BASE_URL")`
       - Base URL: `https://token-plan.ap-southeast-1.maas.aliyuncs.com/compatible-mode/v1`
    4. `alibaba-token-plan-cn`:
       - Aliases: `("dashscope-token-plan-cn",)`
       - Env vars: `("ALIBABA_TOKEN_PLAN_CN_API_KEY", "ALIBABA_TOKEN_PLAN_API_KEY", "ALIBABA_TOKEN_PLAN_CN_BASE_URL")`
       - Base URL: `https://token-plan.cn-beijing.maas.aliyuncs.com/compatible-mode/v1`
- **Unsupported Assumptions**:
  - **Multiple Env Var Fallback**: `alibaba-token-plan-cn` specifies both `ALIBABA_TOKEN_PLAN_CN_API_KEY` and `ALIBABA_TOKEN_PLAN_API_KEY`. The caller's credential resolver must support ordered priority lookup among multiple keys.
  - **Return Cardinality**: The module registers 4 profiles, but [`register_bundled_base_profiles`](../../rust/crates/hermes-gateway/src/provider_registry.rs#L314-L337) pushes only `"alibaba"` to its return vector.

---

### 3.2 `alibaba-coding-plan`
- **Source Files**: [`plugins/model-providers/alibaba-coding-plan/__init__.py`](../../plugins/model-providers/alibaba-coding-plan/__init__.py), [`plugins/model-providers/alibaba-coding-plan/plugin.yaml`](../../plugins/model-providers/alibaba-coding-plan/plugin.yaml)
- **Custom Behavior Beyond `ProviderProfile`**:
  - **None**. Pure base profile instantiations.
- **Version-Dependent Headers**:
  - **None**. `default_headers` is empty.
- **Multiple Registrations**:
  - **YES (2 profiles)**:
    1. `alibaba-coding-plan`:
       - Aliases: `("alibaba_coding", "alibaba-coding", "dashscope-coding")`
       - Env vars: `("ALIBABA_CODING_PLAN_API_KEY", "DASHSCOPE_API_KEY", "ALIBABA_CODING_PLAN_BASE_URL")`
       - Base URL: `https://coding-intl.dashscope.aliyuncs.com/v1`
    2. `alibaba-coding-plan-cn`:
       - Aliases: `("alibaba-coding-cn", "dashscope-coding-cn")`
       - Env vars: `("ALIBABA_CODING_PLAN_CN_API_KEY", "ALIBABA_CODING_PLAN_API_KEY", "DASHSCOPE_API_KEY", "ALIBABA_CODING_PLAN_CN_BASE_URL")`
       - Base URL: `https://coding.dashscope.aliyuncs.com/v1`
- **Unsupported Assumptions**:
  - **Fallback Credential Cascade**: Note that `alibaba-coding-plan-cn` falls back from `ALIBABA_CODING_PLAN_CN_API_KEY` to `ALIBABA_CODING_PLAN_API_KEY` to general `DASHSCOPE_API_KEY`. Credential resolution must test environment variables in declared order.

---

### 3.3 `arcee`
- **Source Files**: [`plugins/model-providers/arcee/__init__.py`](../../plugins/model-providers/arcee/__init__.py), [`plugins/model-providers/arcee/plugin.yaml`](../../plugins/model-providers/arcee/plugin.yaml)
- **Custom Behavior Beyond `ProviderProfile`**:
  - **None**.
- **Version-Dependent Headers**:
  - **None**.
- **Multiple Registrations**:
  - **No (1 profile)**:
    - Name: `arcee`
    - Aliases: `("arcee-ai", "arceeai")`
    - Env vars: `("ARCEEAI_API_KEY",)`
    - Base URL: `https://api.arcee.ai/api/v1`
- **Unsupported Assumptions**:
  - Standard OpenAI-compatible chat completions. No custom assumptions.

---

### 3.4 `azure-foundry`
- **Source Files**: [`plugins/model-providers/azure-foundry/__init__.py`](../../plugins/model-providers/azure-foundry/__init__.py), [`plugins/model-providers/azure-foundry/plugin.yaml`](../../plugins/model-providers/azure-foundry/plugin.yaml)
- **Custom Behavior Beyond `ProviderProfile`**:
  - **None**.
- **Version-Dependent Headers**:
  - **None**.
- **Multiple Registrations**:
  - **No (1 profile)**:
    - Name: `azure-foundry`
    - Aliases: `("azure", "azure-ai-foundry", "azure-ai")`
    - Env vars: `("AZURE_FOUNDRY_API_KEY", "AZURE_FOUNDRY_BASE_URL")`
    - Base URL: `""` (empty string)
- **Unsupported Assumptions**:
  - **CRITICAL: Empty Base URL (`base_url = ""`)**:
    - Microsoft Azure Foundry endpoints are per-resource (e.g. `https://<resource>.openai.azure.com/...` or project-specific endpoints).
    - If a caller does not provide `AZURE_FOUNDRY_BASE_URL` or an explicit `base_url` configuration, `base_url` remains `""`.
    - Under [`ProviderProfile::model_catalog_url`](../../rust/crates/hermes-gateway/src/provider_registry.rs#L94-L113), when `caller_base` is empty and `self.base_url` is empty, the function returns `None`, causing [`fetch_models`](../../rust/crates/hermes-gateway/src/provider_registry.rs#L117-L127) to cleanly return `None` without initiating network calls. Downstream runtime components must not assume a statically routable endpoint exists out-of-the-box.

---

### 3.5 `fireworks`
- **Source Files**: [`plugins/model-providers/fireworks/__init__.py`](../../plugins/model-providers/fireworks/__init__.py), [`plugins/model-providers/fireworks/plugin.yaml`](../../plugins/model-providers/fireworks/plugin.yaml)
- **Custom Behavior Beyond `ProviderProfile`**:
  - **None**. Pure [`ProviderProfile`](../../providers/base.py#L38-L112) instance.
  - Declares curated `fallback_models`:
    `"accounts/fireworks/models/kimi-k2p6"`, `"accounts/fireworks/models/glm-5p2"`, `"accounts/fireworks/models/kimi-k2p7-code"`.
  - Declares `default_aux_model`: `"accounts/fireworks/models/glm-5p2"`.
- **Version-Dependent Headers**:
  - **YES**:
    ```python
    from hermes_cli import __version__ as _HERMES_VERSION
    ...
    default_headers={
        "HTTP-Referer": "https://hermes-agent.nousresearch.com",
        "X-Title": "Hermes Agent",
        "User-Agent": f"HermesAgent/{_HERMES_VERSION}",
    }
    ```
  - Formatted without spaces or hyphens: `HermesAgent/<version>`.
  - Stored in JSON as `"User-Agent": "HermesAgent/__HERMES_NATIVE_VERSION__"`.
  - Dynamically replaced with `hermes_version` at native registration time.
- **Multiple Registrations**:
  - **No (1 profile)**:
    - Name: `fireworks`
    - Aliases: `("fireworks-ai", "fw")`
    - Env vars: `("FIREWORKS_API_KEY",)`
    - Base URL: `https://api.fireworks.ai/inference/v1`
- **Unsupported Assumptions**:
  - **User-Agent Overwrite During Catalog Fetch**: In [`ProviderProfile::fetch_models_with_ca`](../../rust/crates/hermes-gateway/src/provider_registry.rs#L151-L157), the generic caller `user_agent` is inserted first, but then overwritten by `self.default_headers["User-Agent"]` (`HermesAgent/<version>`). This correctly mirrors Python's `req.add_header(k, v)` behavior.
  - **Catalog Model ID Aging**: Statically declared `default_aux_model` and `fallback_models` may age upstream, but serve as offline fallbacks.

---

### 3.6 `gmi`
- **Source Files**: [`plugins/model-providers/gmi/__init__.py`](../../plugins/model-providers/gmi/__init__.py), [`plugins/model-providers/gmi/plugin.yaml`](../../plugins/model-providers/gmi/plugin.yaml)
- **Custom Behavior Beyond `ProviderProfile`**:
  - **None**.
  - Declares `default_aux_model`: `"google/gemini-3.1-flash-lite-preview"`.
  - Declares 7 `fallback_models`:
    `"zai-org/GLM-5.1-FP8"`, `"deepseek-ai/DeepSeek-V3.2"`, `"moonshotai/Kimi-K2.5"`, `"google/gemini-3.1-flash-lite-preview"`, `"anthropic/claude-sonnet-5"`, `"anthropic/claude-sonnet-4.6"`, `"openai/gpt-5.4"`.
- **Version-Dependent Headers**:
  - **YES**:
    ```python
    default_headers={"User-Agent": f"HermesAgent/{_HERMES_VERSION}"}
    ```
  - Stored in JSON as `"User-Agent": "HermesAgent/__HERMES_NATIVE_VERSION__"`.
- **Multiple Registrations**:
  - **No (1 profile)**:
    - Name: `gmi`
    - Aliases: `("gmi-cloud", "gmicloud")`
    - Env vars: `("GMI_API_KEY", "GMI_BASE_URL")`
    - Base URL: `https://api.gmi-serving.com/v1`
- **Unsupported Assumptions**:
  - **Slash-Form Model Identifiers**: Models on GMI use slash IDs (e.g. `zai-org/GLM-5.1-FP8`). Model prefix stripping in [`strip_model_prefix`](../../rust/crates/hermes-gateway/src/provider_registry.rs#L385-L399) splits only on the first `:` colon and checks registered names, preventing slash confusion.

---

### 3.7 `huggingface`
- **Source Files**: [`plugins/model-providers/huggingface/__init__.py`](../../plugins/model-providers/huggingface/__init__.py), [`plugins/model-providers/huggingface/plugin.yaml`](../../plugins/model-providers/huggingface/plugin.yaml)
- **Custom Behavior Beyond `ProviderProfile`**:
  - **None**.
  - Declares 2 `fallback_models`: `"Qwen/Qwen3.5-72B-Instruct"`, `"deepseek-ai/DeepSeek-V3.2"`.
- **Version-Dependent Headers**:
  - **None**.
- **Multiple Registrations**:
  - **No (1 profile)**:
    - Name: `huggingface`
    - Aliases: `("hf", "hugging-face", "huggingface-hub")`
    - Env vars: `("HF_TOKEN",)`
    - Base URL: `https://router.huggingface.co/v1`
- **Unsupported Assumptions**:
  - **Token Variable Name**: Uses `HF_TOKEN` rather than `HUGGINGFACE_API_KEY`. The auth resolver must read the profile's declared `env_vars`.

---

### 3.8 `kilocode`
- **Source Files**: [`plugins/model-providers/kilocode/__init__.py`](../../plugins/model-providers/kilocode/__init__.py), [`plugins/model-providers/kilocode/plugin.yaml`](../../plugins/model-providers/kilocode/plugin.yaml)
- **Custom Behavior Beyond `ProviderProfile`**:
  - **None**.
  - Declares `default_aux_model`: `"google/gemini-3.6-flash"`.
- **Version-Dependent Headers**:
  - **None**.
- **Multiple Registrations**:
  - **No (1 profile)**:
    - Name: `kilocode`
    - Aliases: `("kilo-code", "kilo", "kilo-gateway")`
    - Env vars: `("KILOCODE_API_KEY",)`
    - Base URL: `https://api.kilo.ai/api/gateway`
- **Unsupported Assumptions**:
  - Standard OpenAI-compatible gateway. No custom assumptions.

---

### 3.9 `novita`
- **Source Files**: [`plugins/model-providers/novita/__init__.py`](../../plugins/model-providers/novita/__init__.py), [`plugins/model-providers/novita/plugin.yaml`](../../plugins/model-providers/novita/plugin.yaml)
- **Custom Behavior Beyond `ProviderProfile`**:
  - **None**.
  - Declares `default_aux_model`: `"deepseek/deepseek-v3-0324"`.
  - Declares 6 `fallback_models`:
    `"moonshotai/kimi-k2.5"`, `"minimax/minimax-m2.7"`, `"zai-org/glm-5"`, `"deepseek/deepseek-v3-0324"`, `"deepseek/deepseek-r1-0528"`, `"qwen/qwen3-235b-a22b-fp8"`.
- **Version-Dependent Headers**:
  - **None**.
- **Multiple Registrations**:
  - **No (1 profile)**:
    - Name: `novita`
    - Aliases: `("novita-ai", "novitaai")`
    - Env vars: `("NOVITA_API_KEY", "NOVITA_BASE_URL")`
    - Base URL: `https://api.novita.ai/openai/v1`
- **Unsupported Assumptions**:
  - Standard chat completions. No custom assumptions.

---

### 3.10 `openai-codex`
- **Source Files**: [`plugins/model-providers/openai-codex/__init__.py`](../../plugins/model-providers/openai-codex/__init__.py), [`plugins/model-providers/openai-codex/plugin.yaml`](../../plugins/model-providers/openai-codex/plugin.yaml)
- **Custom Behavior Beyond `ProviderProfile`**:
  - **None in module definition** (instantiates [`ProviderProfile`](../../providers/base.py#L38-L112) directly).
- **Version-Dependent Headers**:
  - **None**.
- **Multiple Registrations**:
  - **No (1 profile)**:
    - Name: `openai-codex`
    - Aliases: `("codex", "openai_codex")`
    - Env vars: `()` (empty tuple)
    - Base URL: `https://chatgpt.com/backend-api/codex`
- **Unsupported Assumptions**:
  - **CRITICAL: Protocol and Authentication Mismatches**:
    1. **`api_mode = "codex_responses"`**: In Python, this switches request formatting from OpenAI `/chat/completions` to ChatGPT backend's `/responses` wire protocol. The Rust gateway profile registry stores this field, but registering the profile does *not* provide the Responses API serializer or wire transport.
    2. **`auth_type = "oauth_external"`**: Unlike all other base providers, `openai-codex` does not read an API key from `env_vars`. It relies on external OAuth token exchange via ChatGPT backend session credentials (`hermes_cli/auth.py`). A naive API-key request will fail with missing credentials.

---

### 3.11 `stepfun`
- **Source Files**: [`plugins/model-providers/stepfun/__init__.py`](../../plugins/model-providers/stepfun/__init__.py), [`plugins/model-providers/stepfun/plugin.yaml`](../../plugins/model-providers/stepfun/plugin.yaml)
- **Custom Behavior Beyond `ProviderProfile`**:
  - **None**.
  - Declares `default_aux_model`: `"step-3.5-flash"`.
- **Version-Dependent Headers**:
  - **None**.
- **Multiple Registrations**:
  - **No (1 profile)**:
    - Name: `stepfun`
    - Aliases: `("step", "stepfun-coding-plan")`
    - Env vars: `("STEPFUN_API_KEY",)`
    - Base URL: `https://api.stepfun.ai/step_plan/v1`
- **Unsupported Assumptions**:
  - Note alias `stepfun-coding-plan` matches StepFun's dedicated coding tier endpoint; does not collide with Alibaba's coding plan.

---

### 3.12 `xai`
- **Source Files**: [`plugins/model-providers/xai/__init__.py`](../../plugins/model-providers/xai/__init__.py), [`plugins/model-providers/xai/plugin.yaml`](../../plugins/model-providers/xai/plugin.yaml)
- **Custom Behavior Beyond `ProviderProfile`**:
  - **None in module definition**.
- **Version-Dependent Headers**:
  - **YES**:
    ```python
    default_headers={"User-Agent": f"Hermes-Agent/{_HERMES_VERSION}"}
    ```
  - **Subtle Formatting Divergence**:
    - Note the hyphen: `Hermes-Agent/<version>`!
    - Contrast with Fireworks and GMI, which use `HermesAgent/<version>` (no hyphen).
    - Preserved faithfully in JSON as `"User-Agent": "Hermes-Agent/__HERMES_NATIVE_VERSION__"` and interpolated by Rust.
- **Multiple Registrations**:
  - **No (1 profile)**:
    - Name: `xai`
    - Aliases: `("grok", "x-ai", "x.ai")`
    - Env vars: `("XAI_API_KEY",)`
    - Base URL: `https://api.x.ai/v1`
- **Unsupported Assumptions**:
  - **`api_mode = "codex_responses"`**: Like `openai-codex`, xAI sets `api_mode = "codex_responses"`. However, unlike `openai-codex`, xAI uses standard `auth_type = "api_key"`. The transport must support the Responses API wire protocol to execute requests against xAI under this profile configuration.

---

### 3.13 `xiaomi`
- **Source Files**: [`plugins/model-providers/xiaomi/__init__.py`](../../plugins/model-providers/xiaomi/__init__.py), [`plugins/model-providers/xiaomi/plugin.yaml`](../../plugins/model-providers/xiaomi/plugin.yaml)
- **Custom Behavior Beyond `ProviderProfile`**:
  - **None**.
- **Version-Dependent Headers**:
  - **None**.
- **Multiple Registrations**:
  - **No (1 profile)**:
    - Name: `xiaomi`
    - Aliases: `("mimo", "xiaomi-mimo")`
    - Env vars: `("XIAOMI_API_KEY",)`
    - Base URL: `https://api.xiaomimimo.com/v1`
- **Unsupported Assumptions**:
  - **`supports_health_check = False`**:
    - Upstream `/v1/models` returns HTTP 401 even with a valid key. The Python doctor utility skips health check probes when this flag is false. The Rust gateway's health checking or diagnostic paths must respect `supports_health_check`.
  - **`supports_vision = True` and `supports_vision_tool_messages = False`**:
    - Xiaomi's MiMo endpoint supports vision input on multimodal user messages, but strictly rejects list-type tool message content (returning HTTP 400 `"text is not set"`).
    - Downstream prompt formatting and tool-result message serializers must check `profile.supports_vision_tool_messages` and avoid sending structured multipart image arrays in tool responses.

---

## 4. Cross-Cutting Analysis of Selected Dimensions

### 4.1 Custom Behavior Beyond `ProviderProfile`
Across all 13 modules, the AST validation in [`gen_bundled_base_profiles.py`](../../rust/tools/gen_bundled_base_profiles.py#L32-L34) and the runtime type check `type(profile) is base.ProviderProfile` confirm that:
1. None of the 13 modules define subclasses of `ProviderProfile`.
2. None of the 13 modules implement or override any of the 9 hook methods provided by Python's [`ProviderProfile`](../../providers/base.py#L115-L260):
   - `resolve_aux_model()`
   - `get_hostname()`
   - `prepare_messages()`
   - `build_extra_body()`
   - `build_api_kwargs_extras()`
   - `default_vision_model()`
   - `get_max_tokens()`
   - `supported_reasoning_efforts()`
   - `create_client()`
3. In contrast, complex in-tree bundled plugins such as:
   - `copilot-acp` (overrides `create_client` to spawn ACP subprocess, overrides `fetch_models` to return None),
   - `anthropic` (overrides `fetch_models` to supply `x-api-key` and `anthropic-version`),
   - `gemini` / `vertex` / `bedrock` (subclass profiles with custom SDK clients),
   - `openrouter` (overrides `build_api_kwargs_extras` to inject reasoning parameters),
   are legitimately excluded from this loader and require dedicated native implementation.

### 4.2 Version-Dependent Headers
Three of the 13 modules inject Hermes version information into HTTP request headers:
- `fireworks`: Sets `User-Agent: HermesAgent/<version>`, plus attribution headers `HTTP-Referer` and `X-Title`.
- `gmi`: Sets `User-Agent: HermesAgent/<version>`.
- `xai`: Sets `User-Agent: Hermes-Agent/<version>`.

```
                    Header Format Matrix
┌────────────┬─────────────────────────────┬───────────────────────────┐
│ Module     │ User-Agent Value            │ Attribution Headers       │
├────────────┼─────────────────────────────┼───────────────────────────┤
│ fireworks  │ HermesAgent/<version>       │ HTTP-Referer, X-Title     │
│ gmi        │ HermesAgent/<version>       │ None                      │
│ xai        │ Hermes-Agent/<version>      │ None                      │
│ (others)   │ (default generic agent UA)  │ None                      │
└────────────┴─────────────────────────────┴───────────────────────────┘
```

#### Replacement Mechanics:
1. The generator replaces the runtime import with the constant sentinel string `__HERMES_NATIVE_VERSION__`.
2. In Rust, [`ProviderRegistry::register_bundled_base_profiles`](../../rust/crates/hermes-gateway/src/provider_registry.rs#L327-L331) traverses each `profile.default_headers.values_mut()`.
3. If a value is a `Value::String`, it executes `header.replace("__HERMES_NATIVE_VERSION__", hermes_version)`.
4. Non-matching strings (such as `https://hermes-agent.nousresearch.com` or `"Hermes Agent"`) remain unaltered. Non-string JSON values (if any existed) would be safely bypassed by pattern matching.
5. In [`ProviderProfile::fetch_models_with_ca`](../../rust/crates/hermes-gateway/src/provider_registry.rs#L151-L157), `default_headers` are applied *after* standard headers (`Authorization`, `Accept`, `User-Agent`), ensuring that provider-specific attribution headers take precedence on the wire.

### 4.3 Multiple Registrations and Alias Namespace Integrity
The 13 modules yield 17 distinct registered profiles:
- Two modules register multiple profiles:
  - `alibaba` registers 4 profiles (`alibaba`, `alibaba-cn`, `alibaba-token-plan`, `alibaba-token-plan-cn`).
  - `alibaba-coding-plan` registers 2 profiles (`alibaba-coding-plan`, `alibaba-coding-plan-cn`).
- Eleven modules register 1 profile each.

#### Namespace Verification:
All 17 canonical names and 20 declared aliases are completely disjoint:
- **Canonical Names (17)**:
  `alibaba`, `alibaba-cn`, `alibaba-token-plan`, `alibaba-token-plan-cn`, `alibaba-coding-plan`, `alibaba-coding-plan-cn`, `arcee`, `azure-foundry`, `fireworks`, `gmi`, `huggingface`, `kilocode`, `novita`, `openai-codex`, `stepfun`, `xai`, `xiaomi`.
- **Aliases (20)**:
  `dashscope`, `alibaba-cloud`, `qwen-dashscope`, `dashscope-cn`, `alibaba-cloud-cn`, `dashscope-token-plan`, `dashscope-token-plan-cn`, `alibaba_coding`, `alibaba-coding`, `dashscope-coding`, `alibaba-coding-cn`, `dashscope-coding-cn`, `arcee-ai`, `arceeai`, `azure`, `azure-ai-foundry`, `azure-ai`, `fireworks-ai`, `fw`, `gmi-cloud`, `gmicloud`, `hf`, `hugging-face`, `huggingface-hub`, `kilo-code`, `kilo`, `kilo-gateway`, `novita-ai`, `novitaai`, `codex`, `openai_codex`, `step`, `stepfun-coding-plan`, `grok`, `x-ai`, `x.ai`, `mimo`, `xiaomi-mimo`.
- **Deduplication & Replacement Invariants**:
  When [`ProviderRegistry::register`](../../rust/crates/hermes-gateway/src/provider_registry.rs#L339-L356) inserts a profile, existing profiles with the same canonical name are replaced in-place, but existing alias mappings survive replacement and resolve to the current canonical profile.

### 4.4 Unsupported Assumptions and Downstream Runtime Dependencies

| Module | Profile | Unsupported Assumption / Runtime Dependency |
|---|---|---|
| `openai-codex` | `openai-codex` | **Responses API & OAuth**: Declares `api_mode = "codex_responses"` and `auth_type = "oauth_external"`. The profile loader records these attributes, but wire formatting for ChatGPT backend's Responses API and OAuth token retrieval must be provided by external runtime layers. |
| `xai` | `xai` | **Responses API**: Declares `api_mode = "codex_responses"`. Even though `auth_type = "api_key"`, the transport must send `/responses` rather than `/chat/completions`. |
| `azure-foundry` | `azure-foundry` | **Empty Base URL**: Declares `base_url = ""`. Model catalog fetch returns `None` and inference routing fails unless caller provides an explicit per-resource base URL. |
| `xiaomi` | `xiaomi` | **Disabled Health Probe**: Upstream `/v1/models` returns 401 even when valid. Doctor/health check components must honor `supports_health_check = false`. |
| `xiaomi` | `xiaomi` | **Tool Message Rejection**: Upstream rejects multipart/list content in tool messages (`supports_vision_tool_messages = false`). Prompt serializers must serialize tool results as plain strings. |
| `alibaba`, `alibaba-coding-plan` | All CN variants | **Fallback Env Keys**: CN variants declare multiple keys in `env_vars` (e.g. `ALIBABA_CODING_PLAN_CN_API_KEY` falling back to `ALIBABA_CODING_PLAN_API_KEY` and `DASHSCOPE_API_KEY`). Credential resolution must evaluate environment keys in priority order. |
| `fireworks`, `gmi`, `novita`, `stepfun`, `kilocode` | Multiple | **Hardcoded Model IDs**: Profiles define static `default_aux_model` and `fallback_models`. These strings provide a bootstrap safety net, but upstream catalog deprecations can cause 404s if not backed by live discovery. |

---

## 5. Comprehensive Summary Matrix

The following table summarizes all 13 modules, their 17 profiles, and their runtime characteristics:

| Module | Profile Canonical Name | Aliases | `api_mode` | `auth_type` | Base URL | Version Headers | Auxiliary / Fallback Models | Special Flags |
|---|---|---|---|---|---|---|---|---|
| `alibaba` | `alibaba` | `dashscope`, `alibaba-cloud`, `qwen-dashscope` | `chat_completions` | `api_key` | `https://dashscope-intl.aliyuncs.com/compatible-mode/v1` | None | None | Standard defaults |
| `alibaba` | `alibaba-cn` | `dashscope-cn`, `alibaba-cloud-cn` | `chat_completions` | `api_key` | `https://dashscope.aliyuncs.com/compatible-mode/v1` | None | None | Env: `DASHSCOPE_API_KEY`, `DASHSCOPE_CN_BASE_URL` |
| `alibaba` | `alibaba-token-plan` | `dashscope-token-plan` | `chat_completions` | `api_key` | `https://token-plan.ap-southeast-1.maas.aliyuncs.com/compatible-mode/v1` | None | None | Env: `ALIBABA_TOKEN_PLAN_API_KEY`, `ALIBABA_TOKEN_PLAN_BASE_URL` |
| `alibaba` | `alibaba-token-plan-cn` | `dashscope-token-plan-cn` | `chat_completions` | `api_key` | `https://token-plan.cn-beijing.maas.aliyuncs.com/compatible-mode/v1` | None | None | Env: `ALIBABA_TOKEN_PLAN_CN_API_KEY`, `ALIBABA_TOKEN_PLAN_API_KEY`, `...` |
| `alibaba-coding-plan` | `alibaba-coding-plan` | `alibaba_coding`, `alibaba-coding`, `dashscope-coding` | `chat_completions` | `api_key` | `https://coding-intl.dashscope.aliyuncs.com/v1` | None | None | Env: `ALIBABA_CODING_PLAN_API_KEY`, `DASHSCOPE_API_KEY`, `...` |
| `alibaba-coding-plan` | `alibaba-coding-plan-cn` | `alibaba-coding-cn`, `dashscope-coding-cn` | `chat_completions` | `api_key` | `https://coding.dashscope.aliyuncs.com/v1` | None | None | Env: `ALIBABA_CODING_PLAN_CN_API_KEY`, `ALIBABA_CODING_PLAN_API_KEY`, `...` |
| `arcee` | `arcee` | `arcee-ai`, `arceeai` | `chat_completions` | `api_key` | `https://api.arcee.ai/api/v1` | None | None | Env: `ARCEEAI_API_KEY` |
| `azure-foundry` | `azure-foundry` | `azure`, `azure-ai-foundry`, `azure-ai` | `chat_completions` | `api_key` | `""` (per-resource) | None | None | Base URL empty; catalog probe returns `None` when empty |
| `fireworks` | `fireworks` | `fireworks-ai`, `fw` | `chat_completions` | `api_key` | `https://api.fireworks.ai/inference/v1` | `User-Agent: HermesAgent/<ver>`, `HTTP-Referer`, `X-Title` | Aux: `.../glm-5p2`<br>Fallback: 3 models | Precedence override for `User-Agent` |
| `gmi` | `gmi` | `gmi-cloud`, `gmicloud` | `chat_completions` | `api_key` | `https://api.gmi-serving.com/v1` | `User-Agent: HermesAgent/<ver>` | Aux: `.../gemini-3.1-flash-lite-preview`<br>Fallback: 7 models | Slash-form model IDs |
| `huggingface` | `huggingface` | `hf`, `hugging-face`, `huggingface-hub` | `chat_completions` | `api_key` | `https://router.huggingface.co/v1` | None | Fallback: 2 models | Env: `HF_TOKEN` |
| `kilocode` | `kilocode` | `kilo-code`, `kilo`, `kilo-gateway` | `chat_completions` | `api_key` | `https://api.kilo.ai/api/gateway` | None | Aux: `google/gemini-3.6-flash` | Standard defaults |
| `novita` | `novita` | `novita-ai`, `novitaai` | `chat_completions` | `api_key` | `https://api.novita.ai/openai/v1` | None | Aux: `deepseek/deepseek-v3-0324`<br>Fallback: 6 models | Standard defaults |
| `openai-codex` | `openai-codex` | `codex`, `openai_codex` | `codex_responses` | `oauth_external` | `https://chatgpt.com/backend-api/codex` | None | None | **Requires external OAuth and Responses wire protocol** |
| `stepfun` | `stepfun` | `step`, `stepfun-coding-plan` | `chat_completions` | `api_key` | `https://api.stepfun.ai/step_plan/v1` | None | Aux: `step-3.5-flash` | Standard defaults |
| `xai` | `xai` | `grok`, `x-ai`, `x.ai` | `codex_responses` | `api_key` | `https://api.x.ai/v1` | `User-Agent: Hermes-Agent/<ver>` | None | **Requires Responses wire protocol**; note hyphen in UA |
| `xiaomi` | `xiaomi` | `mimo`, `xiaomi-mimo` | `chat_completions` | `api_key` | `https://api.xiaomimimo.com/v1` | None | None | `supports_health_check = false`, `supports_vision = true`, `supports_vision_tool_messages = false` |

---

## 6. Verification and Audit Conclusions

1. **Declarative Base Profile Parity**: All 13 selected modules in [`plugins/model-providers/`](../../plugins/model-providers) strictly adhere to pure [`ProviderProfile`](../../providers/base.py#L38-L112) dataclass instantiation with no custom subclasses, functions, or hook overrides.
2. **Generator Robustness**: [`rust/tools/gen_bundled_base_profiles.py`](../../rust/tools/gen_bundled_base_profiles.py) verifies AST node types, exact profile type identity, and dataclass attribute matching. Re-running with `--check` passes cleanly against [`rust/tools/bundled-base-profiles.json`](../../rust/tools/bundled-base-profiles.json).
3. **Runtime Substitution Integrity**: The sentinel `__HERMES_NATIVE_VERSION__` safely accommodates version-dependent headers in `fireworks`, `gmi`, and `xai`. The hyphenation divergence between `HermesAgent/<ver>` (`fireworks`, `gmi`) and `Hermes-Agent/<ver>` (`xai`) is preserved accurately.
4. **Boundary Clarity**: While the native loader provides immediate, native availability of these 17 bundled profiles within the gateway's [`ProviderRegistry`](../../rust/crates/hermes-gateway/src/provider_registry.rs#L306-L308), it does not satisfy dynamic plugin discovery or provider-specific transport hooks. Complex plugins (such as `copilot-acp` or `anthropic`) and specialized wire protocols (`codex_responses`, external OAuth) remain the responsibility of dedicated native subsystems.
