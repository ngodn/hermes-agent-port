//! Gateway configuration.
//!
//! Ported incrementally from `gateway/config.py`. For now it covers only what
//! the skeleton server needs: the bind address. Everything else lands as the
//! corresponding Python behavior is brought over.

use std::net::SocketAddr;

#[derive(Debug, Clone)]
pub struct Config {
    pub bind: SocketAddr,
}

impl Config {
    /// Build config from the environment, falling back to defaults.
    ///
    /// `HERMES_GATEWAY_BIND` overrides the bind address (e.g. `0.0.0.0:8080`).
    pub fn from_env() -> anyhow::Result<Self> {
        let bind = std::env::var("HERMES_GATEWAY_BIND")
            .unwrap_or_else(|_| "127.0.0.1:8787".to_string())
            .parse()?;
        Ok(Self { bind })
    }
}
