//! Gateway configuration.
//!
//! Ported incrementally from `gateway/config.py`. For now it covers what the
//! runnable skeleton needs: the bind address and how to reach the Python agent
//! subprocess. Everything else lands as the corresponding Python behavior is
//! brought over.

use std::net::SocketAddr;
use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct Config {
    pub bind: SocketAddr,
    /// Python interpreter used to run the agent bridge shim.
    pub agent_python: String,
    /// Working directory for the agent subprocess (the hermes repo root), so
    /// `python -m hermes_cli.stream_turn` resolves.
    pub agent_cwd: PathBuf,
    /// Optional model override passed to the agent.
    pub agent_model: Option<String>,
    /// Telegram bot token; when set, the Telegram push path is started.
    pub telegram_token: Option<String>,
    /// Discord bot token; when set, the Discord push path is started.
    pub discord_token: Option<String>,
    /// Slack app-level token (xapp-) for Socket Mode, and bot token (xoxb-) for
    /// posting. Both are required to start the Slack push path.
    pub slack_app_token: Option<String>,
    pub slack_bot_token: Option<String>,
    /// Opt in to the native (in-Rust) agent client instead of the Python
    /// subprocess bridge. Requires `llm_api_key` and a resolved model.
    pub agent_native: bool,
    /// Explicit native API key (`HERMES_LLM_API_KEY`). Otherwise native startup
    /// resolves credentials from the selected provider's environment and .env.
    pub llm_api_key: Option<String>,
    /// API root for the native client (`HERMES_LLM_BASE_URL`), else config's
    /// `model.base_url`, then the selected profile's endpoint override/default,
    /// or OpenRouter when no bundled profile is selected.
    pub llm_base_url: Option<String>,
    /// CLI-backend agent program (`HERMES_AGENT_CLI`, e.g. "claude" or "agy").
    /// When set it takes precedence: turns run via that CLI, no Python or HTTP.
    pub agent_cli: Option<String>,
    /// Extra args for the CLI backend (`HERMES_AGENT_CLI_ARGS`), whitespace-split.
    pub agent_cli_args: Option<String>,
    /// Flag used to pass the prompt to the CLI (`HERMES_AGENT_CLI_PROMPT_FLAG`,
    /// default `-p`; set empty to pass the prompt as a trailing positional).
    pub agent_cli_prompt_flag: Option<String>,
    /// Enable built-in tools on the native HTTP client (`HERMES_AGENT_TOOLS=1`).
    pub agent_tools: bool,
}

impl Config {
    /// Build config from the environment, falling back to defaults.
    ///
    /// - `HERMES_GATEWAY_BIND`  bind address (default `127.0.0.1:8787`)
    /// - `HERMES_AGENT_PYTHON`  interpreter (default `python3`)
    /// - `HERMES_AGENT_CWD`     repo root for the agent subprocess (default `.`)
    /// - `HERMES_AGENT_MODEL`   optional model override
    pub fn from_env() -> anyhow::Result<Self> {
        let bind = std::env::var("HERMES_GATEWAY_BIND")
            .unwrap_or_else(|_| "127.0.0.1:8787".to_string())
            .parse()?;
        let agent_python =
            std::env::var("HERMES_AGENT_PYTHON").unwrap_or_else(|_| "python3".to_string());
        let agent_cwd = std::env::var("HERMES_AGENT_CWD")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("."));
        let agent_model = std::env::var("HERMES_AGENT_MODEL")
            .ok()
            .filter(|s| !s.is_empty());
        let telegram_token = std::env::var("HERMES_TELEGRAM_TOKEN")
            .ok()
            .filter(|s| !s.is_empty());
        let discord_token = std::env::var("HERMES_DISCORD_TOKEN")
            .ok()
            .filter(|s| !s.is_empty());
        let slack_app_token = std::env::var("HERMES_SLACK_APP_TOKEN")
            .ok()
            .filter(|s| !s.is_empty());
        let slack_bot_token = std::env::var("HERMES_SLACK_BOT_TOKEN")
            .ok()
            .filter(|s| !s.is_empty());
        let agent_native = std::env::var("HERMES_AGENT_NATIVE")
            .map(|v| {
                matches!(
                    v.trim().to_ascii_lowercase().as_str(),
                    "1" | "true" | "yes" | "on"
                )
            })
            .unwrap_or(false);
        let llm_api_key = std::env::var("HERMES_LLM_API_KEY")
            .ok()
            .filter(|s| !s.is_empty());
        let llm_base_url = std::env::var("HERMES_LLM_BASE_URL")
            .ok()
            .filter(|s| !s.is_empty());
        let agent_cli = std::env::var("HERMES_AGENT_CLI")
            .ok()
            .filter(|s| !s.is_empty());
        let agent_cli_args = std::env::var("HERMES_AGENT_CLI_ARGS")
            .ok()
            .filter(|s| !s.is_empty());
        // Distinguish "unset" (use default -p) from "set empty" (positional).
        let agent_cli_prompt_flag = std::env::var("HERMES_AGENT_CLI_PROMPT_FLAG").ok();
        let agent_tools = std::env::var("HERMES_AGENT_TOOLS")
            .map(|v| {
                matches!(
                    v.trim().to_ascii_lowercase().as_str(),
                    "1" | "true" | "yes" | "on"
                )
            })
            .unwrap_or(false);
        Ok(Self {
            bind,
            agent_python,
            agent_cwd,
            agent_model,
            telegram_token,
            discord_token,
            slack_app_token,
            slack_bot_token,
            agent_native,
            llm_api_key,
            llm_base_url,
            agent_cli,
            agent_cli_args,
            agent_cli_prompt_flag,
            agent_tools,
        })
    }
}
