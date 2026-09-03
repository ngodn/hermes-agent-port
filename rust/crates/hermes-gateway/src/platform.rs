//! Platform adapters and their registry.
//!
// WIP scaffold: wired in as adapters are ported. Allow dead code until then.
#![allow(dead_code)]
//!
//! Ports the intent of `gateway/platform_registry.py`: adapters self-register
//! so the gateway can discover and instantiate them without a hardcoded
//! if/elif chain. Each adapter drives one messaging platform (Telegram,
//! Discord, ...), receiving inbound messages and delivering outbound ones.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use hermes_core::{Message, Result};
use tokio::sync::mpsc;

/// A running platform connection. One instance per configured platform.
///
/// `run` owns the long-lived connection: it pushes inbound [`Message`]s into
/// `inbound` and returns only on shutdown or fatal error. `send` delivers an
/// outbound message. This mirrors the adapter surface the Python gateway
/// drives via `GatewayRunner`.
#[async_trait]
pub trait PlatformAdapter: Send + Sync {
    /// Config identifier, e.g. "telegram".
    fn name(&self) -> &str;

    /// Run the inbound loop until shutdown, forwarding messages to `inbound`.
    async fn run(&self, inbound: mpsc::Sender<Message>) -> Result<()>;

    /// Deliver an outbound message on this platform.
    async fn send(&self, msg: &Message) -> Result<()>;
}

/// Metadata + factory for a platform, registered before instantiation.
/// Mirrors `PlatformEntry` in the Python registry.
pub struct PlatformEntry {
    pub name: &'static str,
    pub label: &'static str,
    /// Environment variables that must be present for this platform to load.
    pub required_env: &'static [&'static str],
    /// Hint shown when requirements are missing.
    pub install_hint: &'static str,
    /// Builds a live adapter from its config.
    pub factory: Box<dyn Fn(&PlatformConfig) -> Result<Arc<dyn PlatformAdapter>> + Send + Sync>,
}

/// Per-platform configuration slice. Filled out as `gateway/config.py` is
/// ported; for now it carries the free-form settings an adapter needs.
#[derive(Debug, Clone, Default)]
pub struct PlatformConfig {
    pub settings: HashMap<String, String>,
}

/// Registry of known platforms. Built-in adapters register at startup;
/// plugins can register more before adapters are created.
#[derive(Default)]
pub struct PlatformRegistry {
    entries: HashMap<&'static str, PlatformEntry>,
}

impl PlatformRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, entry: PlatformEntry) {
        self.entries.insert(entry.name, entry);
    }

    pub fn get(&self, name: &str) -> Option<&PlatformEntry> {
        self.entries.get(name)
    }

    pub fn names(&self) -> impl Iterator<Item = &&'static str> {
        self.entries.keys()
    }

    /// Instantiate an adapter by name, checking required env first.
    pub fn create_adapter(
        &self,
        name: &str,
        config: &PlatformConfig,
    ) -> Result<Arc<dyn PlatformAdapter>> {
        let entry = self
            .get(name)
            .ok_or_else(|| hermes_core::Error::Config(format!("unknown platform: {name}")))?;

        for var in entry.required_env {
            if std::env::var(var).is_err() {
                return Err(hermes_core::Error::Config(format!(
                    "platform {name} requires env {var} ({})",
                    entry.install_hint
                )));
            }
        }

        (entry.factory)(config)
    }
}
