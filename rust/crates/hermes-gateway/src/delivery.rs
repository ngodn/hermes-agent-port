//! Port of the routing primitives of gateway/delivery.py.
//!
// Public API is ahead of its callers (the delivery router wires it).
#![allow(dead_code)]
//!
//! Delivery-target parsing and the adapter-independent classification helpers:
//! `DeliveryTarget` (parse / render `origin`, `local`, `platform`,
//! `platform:chat_id[:thread_id]`), the silence-narration filter, and the
//! Telegram private-chat-id heuristic. The `DeliveryRouter` itself (send loop,
//! home-channel resolution, dead-target skipping) hangs off the adapter registry
//! and lands with the adapter subsystem; `_classify_dead_from_error_text` needs
//! the platform-base error classifier and comes along then.

use std::sync::OnceLock;

use fancy_regex::Regex;

use crate::session::SessionSource;

/// Cap before gateway-level truncation of cron output for a non-chunking
/// platform (Telegram's hard limit is 4096; the headroom covers the truncation
/// footer). Adapters that split long messages natively bypass this.
pub const MAX_PLATFORM_OUTPUT: usize = 4000;

/// Known platform wire values (`gateway.config.Platform`). An unknown token in
/// a target string is treated as `local`, matching the Python `Platform(...)`
/// ValueError fallback.
const KNOWN_PLATFORMS: &[&str] = &[
    "local",
    "telegram",
    "discord",
    "whatsapp",
    "whatsapp_cloud",
    "slack",
    "signal",
    "mattermost",
    "matrix",
    "homeassistant",
    "email",
    "sms",
    "dingtalk",
    "api_server",
    "webhook",
    "msgraph_webhook",
    "feishu",
    "wecom",
    "wecom_callback",
    "weixin",
    "bluebubbles",
    "qqbot",
    "yuanbao",
    "relay",
];

fn is_known_platform(name: &str) -> bool {
    KNOWN_PLATFORMS.contains(&name)
}

fn silence_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"(?i)^[\s*_~`]*\(?\s*(silent|silence|no\s+response|no\s+reply)\s*\.?\)?[\s*_~`]*$|^[\s*_~`]*[\x{1F507}\.\x{2026}]+[\s*_~`]*$",
        )
        .expect("silence regex")
    })
}

/// True when `content` is only a "silence" narration (with optional markdown
/// wrappers): `*(silent)*`, `_silent_`, `(no response)`, a bare `.`/`…`, the
/// mute emoji, etc. Anchored, so a substantive message that merely contains the
/// word "silent" is never matched.
pub fn is_silence_narration(content: Option<&str>) -> bool {
    match content {
        None => false,
        Some(c) => silence_re().is_match(c).unwrap_or(false),
    }
}

/// True when `chat_id` is a positive integer (Telegram's private-chat shape;
/// groups/channels/supergroups use negative ids).
pub fn looks_like_telegram_private_chat_id(chat_id: Option<&str>) -> bool {
    chat_id
        .and_then(|c| c.trim().parse::<i64>().ok())
        .map(|n| n > 0)
        .unwrap_or(false)
}

/// True when `value` parses as an integer.
pub fn looks_like_int(value: Option<&str>) -> bool {
    value
        .map(|v| v.trim().parse::<i64>().is_ok())
        .unwrap_or(false)
}

/// A single delivery target: `origin` (back to source), `local` (files), a
/// platform home channel, or a specific `platform:chat_id[:thread_id]`.
#[derive(Debug, Clone, PartialEq)]
pub struct DeliveryTarget {
    /// Platform wire value (e.g. `telegram`, `local`).
    pub platform: String,
    pub chat_id: Option<String>,
    pub thread_id: Option<String>,
    pub is_origin: bool,
    pub is_explicit: bool,
}

impl DeliveryTarget {
    fn local() -> Self {
        Self {
            platform: "local".to_string(),
            chat_id: None,
            thread_id: None,
            is_origin: false,
            is_explicit: false,
        }
    }

    /// Parse a delivery target string, resolving `origin` against `origin` when
    /// given. An unknown platform token falls back to `local`.
    pub fn parse(target: &str, origin: Option<&SessionSource>) -> Self {
        let stripped = target.trim();
        let lower = stripped.to_lowercase();

        if lower == "origin" {
            return match origin {
                Some(o) => Self {
                    platform: o.platform.clone(),
                    chat_id: if o.chat_id.is_empty() {
                        None
                    } else {
                        Some(o.chat_id.clone())
                    },
                    thread_id: o.thread_id.clone(),
                    is_origin: true,
                    is_explicit: false,
                },
                None => Self {
                    is_origin: true,
                    ..Self::local()
                },
            };
        }
        if lower == "local" {
            return Self::local();
        }

        // platform:chat_id[:thread_id] — chat_id/thread_id keep original case.
        if stripped.contains(':') {
            let mut parts = stripped.splitn(3, ':');
            let platform_str = parts.next().unwrap_or("").to_lowercase();
            let chat_id = parts.next().map(str::to_string);
            let thread_id = parts.next().map(str::to_string);
            if is_known_platform(&platform_str) {
                return Self {
                    platform: platform_str,
                    chat_id,
                    thread_id,
                    is_origin: false,
                    is_explicit: true,
                };
            }
            return Self::local();
        }

        // Bare platform name (home channel).
        if is_known_platform(&lower) {
            Self {
                platform: lower,
                ..Self::local()
            }
        } else {
            Self::local()
        }
    }

    /// Render back to the string form.
    pub fn to_string_form(&self) -> String {
        if self.is_origin {
            return "origin".to_string();
        }
        if self.platform == "local" {
            return "local".to_string();
        }
        match (&self.chat_id, &self.thread_id) {
            (Some(c), Some(t)) => format!("{}:{c}:{t}", self.platform),
            (Some(c), None) => format!("{}:{c}", self.platform),
            _ => self.platform.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn silence_detection() {
        for s in [
            "silent",
            "*(silent)*",
            "_silence_",
            "(no response)",
            ".",
            "…",
            "\u{1F507}",
            "  `silent` ",
        ] {
            assert!(is_silence_narration(Some(s)), "should be silence: {s:?}");
        }
        for s in [
            "I will stay silent about that",
            "here is the answer",
            "silently working",
        ] {
            assert!(
                !is_silence_narration(Some(s)),
                "should NOT be silence: {s:?}"
            );
        }
        assert!(!is_silence_narration(None));
    }

    #[test]
    fn telegram_private_chat_heuristic() {
        assert!(looks_like_telegram_private_chat_id(Some("12345")));
        assert!(!looks_like_telegram_private_chat_id(Some("-100999")));
        assert!(!looks_like_telegram_private_chat_id(Some("abc")));
        assert!(!looks_like_telegram_private_chat_id(None));
        assert!(looks_like_int(Some("-5")));
        assert!(!looks_like_int(Some("x")));
    }

    #[test]
    fn parse_origin_local_and_explicit() {
        assert_eq!(
            DeliveryTarget::parse("local", None),
            DeliveryTarget::local()
        );
        // origin with no source -> local+is_origin.
        let o = DeliveryTarget::parse("origin", None);
        assert!(o.is_origin && o.platform == "local");
        // origin resolves against the source.
        let src = SessionSource {
            thread_id: Some("t1".into()),
            ..SessionSource::new("telegram", "999")
        };
        let r = DeliveryTarget::parse("origin", Some(&src));
        assert_eq!(r.platform, "telegram");
        assert_eq!(r.chat_id.as_deref(), Some("999"));
        assert_eq!(r.thread_id.as_deref(), Some("t1"));
        assert!(r.is_origin);
    }

    #[test]
    fn parse_platform_and_chat() {
        let bare = DeliveryTarget::parse("telegram", None);
        assert_eq!(bare.platform, "telegram");
        assert!(bare.chat_id.is_none() && !bare.is_explicit);

        let explicit = DeliveryTarget::parse("Telegram:123456:77", None);
        assert_eq!(explicit.platform, "telegram");
        assert_eq!(explicit.chat_id.as_deref(), Some("123456"));
        assert_eq!(explicit.thread_id.as_deref(), Some("77"));
        assert!(explicit.is_explicit);
        assert_eq!(explicit.to_string_form(), "telegram:123456:77");

        // Unknown platform -> local.
        assert_eq!(DeliveryTarget::parse("myspace:1", None).platform, "local");
        assert_eq!(DeliveryTarget::parse("myspace", None).platform, "local");
    }

    #[test]
    fn to_string_forms() {
        assert_eq!(
            DeliveryTarget::parse("local", None).to_string_form(),
            "local"
        );
        assert_eq!(
            DeliveryTarget::parse("slack", None).to_string_form(),
            "slack"
        );
        assert_eq!(
            DeliveryTarget::parse("slack:C1", None).to_string_form(),
            "slack:C1"
        );
    }
}
