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
        let agent_model = std::env::var("HERMES_AGENT_MODEL").ok().filter(|s| !s.is_empty());
        let telegram_token = std::env::var("HERMES_TELEGRAM_TOKEN")
            .ok()
            .filter(|s| !s.is_empty());
        let discord_token = std::env::var("HERMES_DISCORD_TOKEN")
            .ok()
            .filter(|s| !s.is_empty());
        Ok(Self {
            bind,
            agent_python,
            agent_cwd,
            agent_model,
            telegram_token,
            discord_token,
        })
    }
}
