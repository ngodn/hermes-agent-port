//! Structured streaming events — the agent -> gateway delivery contract.
//!
//! Port of `gateway/stream_events.py`. A small, typed event vocabulary that
//! names *what happened* without prescribing *how it is delivered*: the agent
//! emits structured events, the gateway's stream consumer is the single sink,
//! and each platform adapter decides how to render each event.
//!
//! The Python side used a `Union` of frozen dataclasses specifically so a
//! missing `case` in an exhaustive match is a visible type error. A Rust enum
//! gives that for free: `match` on [`StreamEvent`] is checked for exhaustiveness
//! by the compiler.
//!
//! These carry transport/presentation only. Nothing here is conversation
//! history; history is owned by the agent. No tool *output* travels here.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Every event the consumer's dispatcher accepts. One enum instead of a marker
/// trait so exhaustiveness is compiler-enforced.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum StreamEvent {
    /// A delta of streamed assistant text. The consumer accumulates chunks and
    /// progressively renders them. Reasoning/think-block content is filtered
    /// upstream and never arrives here.
    MessageChunk { text: String },

    /// The current assistant message segment is complete. `final_` is true only
    /// for the terminal stop of the whole turn; an intermediate stop
    /// (text -> tool call -> more text) carries `final_ = false` so the consumer
    /// finalizes the current bubble and starts a fresh segment without treating
    /// the turn as done.
    MessageStop {
        #[serde(rename = "final", default)]
        final_: bool,
    },

    /// A complete interim assistant message emitted between tool iterations
    /// (e.g. "I'll inspect the repo first."). Already-complete text, not a
    /// delta; rendered as its own message so it reads as a distinct beat.
    Commentary { text: String },

    /// A tool invocation started, or its in-progress state changed. Carries the
    /// raw facts; the gateway decides presentation. `index` is a monotonic
    /// per-turn index so a finish can be correlated with its start.
    ToolCallChunk {
        tool_name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        preview: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        args: Option<HashMap<String, Value>>,
        #[serde(default)]
        index: i64,
    },

    /// A tool invocation completed. `duration` is wall-clock seconds; `ok`
    /// reflects whether the tool returned without raising. No tool output here.
    ToolCallFinished {
        tool_name: String,
        #[serde(default)]
        duration: f64,
        #[serde(default = "default_true")]
        ok: bool,
        #[serde(default)]
        index: i64,
    },

    /// One-shot onboarding nudge when a tool runs longer than the threshold.
    /// The gateway owns the "should I surface this here?" decision.
    LongToolHint {
        #[serde(default)]
        tool_name: String,
        #[serde(default)]
        duration: f64,
    },

    /// A gateway-originated control message (restart, online, long-run notice).
    /// `notice_kind` is a stable string the adapter can switch on
    /// ("restart" / "online" / "long_run" / ...).
    GatewayNotice {
        notice_kind: String,
        #[serde(default)]
        text: String,
        #[serde(default)]
        extra: HashMap<String, Value>,
    },
}

fn default_true() -> bool {
    true
}
