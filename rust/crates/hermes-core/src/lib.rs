//! Shared types for the Hermes Rust rewrite.
//!
//! This crate holds the vocabulary that every other crate agrees on: the
//! error type, and the message/channel shapes that cross the gateway <-> agent
//! RPC boundary. It intentionally has no async or IO dependencies so it stays
//! cheap to depend on.

use serde::{Deserialize, Serialize};

pub mod error;

pub use error::{Error, Result};

/// A messaging platform Hermes can talk on. Mirrors the Python
/// `gateway.platform_registry` set. Kept as an enum so routing is exhaustive.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Platform {
    Cli,
    Telegram,
    Discord,
    Slack,
    WhatsApp,
    Signal,
}

/// A single inbound or outbound message on some platform. This is the minimal
/// shape the gateway needs before it hands off to the agent; richer per-turn
/// context is layered on later in the port.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub platform: Platform,
    /// Opaque per-platform conversation id (chat id, channel id, ...).
    pub channel_id: String,
    /// Opaque per-platform sender id.
    pub sender_id: String,
    pub text: String,
}
