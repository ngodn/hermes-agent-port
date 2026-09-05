# Base Profile Credentials and Auth Source Review

## 1. Declared Key-Name Order
- Profiles in the 13 modules selected by [`gen_bundled_base_profiles.py`](../tools/gen_bundled_base_profiles.py) declare credentials in `ProviderProfile.env_vars`.
- Single-key profiles specify a single token (`DASHSCOPE_API_KEY`, `ARCEEAI_API_KEY`, `FIREWORKS_API_KEY`, `HF_TOKEN`, `KILOCODE_API_KEY`, `STEPFUN_API_KEY`, `XAI_API_KEY`, `XIAOMI_API_KEY`).
- Cascading profiles declare explicit fallback priority in [`_resolve_api_key_provider_secret`](../../hermes_cli/auth.py):
  - `alibaba-coding-plan`: `ALIBABA_CODING_PLAN_API_KEY` -> `DASHSCOPE_API_KEY`.
  - `alibaba-coding-plan-cn`: `ALIBABA_CODING_PLAN_CN_API_KEY` -> `ALIBABA_CODING_PLAN_API_KEY` -> `DASHSCOPE_API_KEY`.
  - `alibaba-token-plan-cn`: `ALIBABA_TOKEN_PLAN_CN_API_KEY` -> `ALIBABA_TOKEN_PLAN_API_KEY`.
  - `openai-codex`: empty `env_vars=()`, authenticated exclusively via external OAuth.
- Regional and tier-specific keys evaluate before general vendor keys, returning on first match.

## 2. URL-Name Exclusion
- In plugin profiles, base URL variables share the `env_vars` tuple alongside API key names.
- In [`hermes_cli/auth.py`](../../hermes_cli/auth.py) dynamic registration, URL names are filtered out of `_api_key_vars`:
  - Condition: `not v.endswith("_BASE_URL") and not v.endswith("_URL")`.
  - Captures `_base_url_var = next((v for v in _pp.env_vars if v.endswith("_BASE_URL") or v.endswith("_URL")), None)`.
  - Excludes `DASHSCOPE_CN_BASE_URL`, `ALIBABA_TOKEN_PLAN_BASE_URL`, `ALIBABA_TOKEN_PLAN_CN_BASE_URL`, `ALIBABA_CODING_PLAN_BASE_URL`, `ALIBABA_CODING_PLAN_CN_BASE_URL`, `AZURE_FOUNDRY_BASE_URL`, `GMI_BASE_URL`, `NOVITA_BASE_URL` from secret resolution.
- In static `PROVIDER_REGISTRY`, `api_key_env_vars` and `base_url_env_var` are decoupled directly. Static definitions add `_BASE_URL` variables for `arcee`, `huggingface`, `kilocode`, `stepfun`, `xai`, and `xiaomi` despite omission in plugin files.
- `fireworks` is not in static `PROVIDER_REGISTRY` and omits URL vars in its plugin, leaving `base_url_env_var=""`.

## 3. Dotenv-versus-Env Semantics
- Secret resolution in [`_resolve_api_key_provider_secret`](../../hermes_cli/auth.py) uses [`get_env_value_prefer_dotenv`](../../hermes_cli/config.py).
- `get_env_value_prefer_dotenv` inspects `load_env()` (`~/.hermes/.env`) first: non-empty file values take precedence over `os.environ` so rotated file credentials cannot be shadowed by parent shell exports.
- If unset in `.env`, it falls back to [`secret_scope.get_secret`](../../agent/secret_scope.py), then `os.environ`.
- Divergence on base URLs: [`resolve_api_key_provider_credentials`](../../hermes_cli/auth.py) uses `os.getenv(pconfig.base_url_env_var)` directly, ignoring `.env` unless synced into process environment and bypassing `secret_scope`.

## 4. Placeholder Rejection
- Handled by [`has_usable_secret`](../../hermes_cli/auth.py) for all env values and credential-pool entries:
  - Rejects non-string inputs and strips leading/trailing whitespace.
  - Enforces minimum length: `len(cleaned) >= 4`.
  - Case-insensitively blocks placeholders: `*`, `**`, `***`, `changeme`, `your_api_key`, `your_api_key_here`, `your-api-key`, `placeholder`, `example`, `dummy`, `null`, `none`.
- Prefix gating: `_secret_matches_declared_prefix` validates known formats in `KNOWN_PROVIDER_KEY_PREFIXES`. None of the 13 bundled base profiles register prefixes, so all operate fail-open on non-placeholder strings.
- Sentinels (`LMSTUDIO_NOAUTH_PLACEHOLDER`, `ACTUAL_LOCAL_NOAUTH_PLACEHOLDER`) apply only to local runners.

## 5. Endpoint Overrides
- [`resolve_api_key_provider_credentials`](../../hermes_cli/auth.py) applies overrides in order:
  1. `env_url = os.getenv(pconfig.base_url_env_var, "").strip()`.
  2. If present, `base_url = env_url.rstrip("/")`; otherwise `base_url = pconfig.inference_base_url`.
  3. Non-empty string guard restores `pconfig.inference_base_url` if empty.
- `azure-foundry`: Configures `base_url=""` in both plugin and static registry. Without `AZURE_FOUNDRY_BASE_URL`, `base_url` resolves empty and catalog probes return `None`.
- `openai-codex`: Declares `auth_type="oauth_external"`; calling `resolve_api_key_provider_credentials` raises `AuthError` (`invalid_provider`).
- None of the 13 modules require bespoke URL resolvers (`_resolve_kimi_base_url`, `_resolve_zai_base_url`, Copilot token exchange).

## 6. Credential-Pool and Secret-Scope Limitations
- Credential pool ([`agent/credential_pool.py`](../../agent/credential_pool.py)):
  - Pool lookup is strictly a secondary fallback in `_resolve_api_key_provider_secret` when all declared env vars are absent or placeholders.
  - `_seed_from_env` only processes providers registered with `auth_type == "api_key"`; `openai-codex` is skipped.
  - A pool entry stores a single static `access_token` and loses profile multi-key cascade semantics.
- Secret scope ([`agent/secret_scope.py`](../../agent/secret_scope.py)):
  - In multiplex mode (`_MULTIPLEX_ACTIVE=True`), unscoped secret reads raise `UnscopedSecretError`.
  - Precedence flaw: `get_env_value_prefer_dotenv` reads `~/.hermes/.env` prior to `secret_scope.get_secret`. In multiplexed gateways, shared host `.env` files can leak across profiles unless `HERMES_HOME_OVERRIDE` isolates home directories.
  - Endpoint blind spot: `resolve_api_key_provider_credentials` reads `base_url_env_var` via `os.getenv`, ignoring tenant secret scopes.
