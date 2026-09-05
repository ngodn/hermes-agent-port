//! Port of the pending-event merge behavior from `gateway/platforms/base.py`.
//!
// Public API is ahead of its callers while the runner (GatewayRunner) is ported.
#![allow(dead_code)]
//!
//! Photo bursts/albums and rapid follow-ups arrive as several near-simultaneous
//! events. Rather than let the last one clobber the queued turn, the gateway
//! merges them into the pending event so the next turn sees the whole burst.
//! This module ports:
//!
//!   * `merge_pending_message_event` -> [`merge_pending_message_event`]
//!   * `BasePlatformAdapter._merge_caption` -> [`merge_caption`]
//!   * `_invalidate_pending_stt_cache` (the STT-cache side effect) is expressed
//!     through [`PendingStt::invalidate`] on the bundled state.
//!
//! Python carries the transient STT state on the `MessageEvent` via `setattr`.
//! We keep `MessageEvent` untouched and bundle it with its [`PendingStt`] in a
//! [`PendingMessage`], which the runner stores in the per-session pending map.
//! Merging mutates the existing bundle in place (preserving the original event's
//! identity and reply fields) and invalidates only the derived STT cache, so the
//! echo ledger survives; replacing the pending turn drops the old bundle and its
//! STT state entirely.

use std::collections::HashMap;

use crate::pending_stt::PendingStt;
use crate::platform_base_types::{MessageEvent, MessageType};

// Python str.strip also treats the four ASCII information separators as
// whitespace. Rust str.trim alone would leave these in merged captions. This
// mirrors the private helper in `pending_stt`; it is duplicated here to avoid
// reaching across module boundaries for one small function.
fn trim_python_whitespace(text: &str) -> &str {
    text.trim_matches(|c: char| c.is_whitespace() || ('\u{1c}'..='\u{1f}').contains(&c))
}

/// A queued inbound event together with its transient STT preprocessing state.
///
/// The runner stores one of these per session key. Bundling the event with its
/// [`PendingStt`] lets a merge invalidate the derived transcription cache while
/// keeping the echo ledger, exactly as Python's `setattr`-on-event model does.
#[derive(Debug, Clone, Default)]
pub struct PendingMessage {
    pub event: MessageEvent,
    pub stt: PendingStt,
}

impl PendingMessage {
    /// Wrap a fresh inbound event with empty STT state.
    pub fn new(event: MessageEvent) -> Self {
        Self {
            event,
            stt: PendingStt::new(),
        }
    }
}

/// Merge a new caption into existing text, avoiding duplicates.
///
/// Faithful to `BasePlatformAdapter._merge_caption`: line-by-line exact match
/// (splitting on the blank-line separator `\n\n`), not substring, so a shorter
/// caption is never silently dropped because it is contained in a longer one.
/// Whitespace is normalised for comparison using Python's `str.strip` semantics
/// (which include U+001C..U+001F), and the final concatenation is trimmed the
/// same way.
pub fn merge_caption(existing_text: &str, new_text: &str) -> String {
    if existing_text.is_empty() {
        return new_text.to_string();
    }
    let new_trimmed = trim_python_whitespace(new_text);
    let already_present = existing_text
        .split("\n\n")
        .any(|caption| trim_python_whitespace(caption) == new_trimmed);
    if !already_present {
        return trim_python_whitespace(&format!("{existing_text}\n\n{new_text}")).to_string();
    }
    existing_text.to_string()
}

/// Store or merge a pending event for a session.
///
/// Faithful to `merge_pending_message_event`:
///   * PHOTO + PHOTO merges media, captions, and inline flags even when the
///     incoming event carries no media (an empty-media photo still merges its
///     caption).
///   * When either side carries media, the incoming media is appended: media
///     URLs, MIME types (concatenated as-is, never padded to repair a short MIME
///     array), and inline flags (existing flags padded to the existing URL count
///     first; overlong flag arrays are preserved, never truncated).
///   * Message type resolves as: PHOTO on either side dominates; otherwise a
///     TEXT existing upgrades to a non-TEXT incoming type.
///   * A media merge invalidates the derived STT cache but keeps the echo
///     ledger (see [`PendingStt::invalidate`]).
///   * With `merge_text`, two consecutive TEXT events are joined with a single
///     newline instead of the last replacing the queued turn.
///   * Anything else replaces the pending turn, dropping the old bundle's STT
///     state (the fresh [`PendingMessage`] starts with empty STT state).
///
/// The existing bundle is mutated in place, so the original event's identity and
/// reply fields are preserved across a merge.
pub fn merge_pending_message_event(
    pending: &mut HashMap<String, PendingMessage>,
    session_key: &str,
    event: MessageEvent,
    merge_text: bool,
) {
    if let Some(existing) = pending.get_mut(session_key) {
        let existing_is_photo = existing.event.message_type == MessageType::Photo;
        let incoming_is_photo = event.message_type == MessageType::Photo;
        let existing_has_media = !existing.event.media_urls.is_empty();
        let incoming_has_media = !event.media_urls.is_empty();

        let mut incoming_inline_flags: Vec<Option<bool>> = Vec::new();
        if incoming_has_media {
            let mut existing_inline_flags = existing.event.media_text_inlined.clone();
            let existing_pad = existing
                .event
                .media_urls
                .len()
                .saturating_sub(existing_inline_flags.len());
            existing_inline_flags.extend(std::iter::repeat_n(None, existing_pad));

            incoming_inline_flags = event.media_text_inlined.clone();
            let incoming_pad = event
                .media_urls
                .len()
                .saturating_sub(incoming_inline_flags.len());
            incoming_inline_flags.extend(std::iter::repeat_n(None, incoming_pad));

            existing.event.media_text_inlined = existing_inline_flags;
        }

        if existing_is_photo && incoming_is_photo {
            existing.event.media_urls.extend(event.media_urls.clone());
            existing.event.media_types.extend(event.media_types.clone());
            existing
                .event
                .media_text_inlined
                .extend(incoming_inline_flags);
            if !event.text.is_empty() {
                existing.event.text = merge_caption(&existing.event.text, &event.text);
            }
            existing.stt.invalidate();
            return;
        }

        if existing_has_media || incoming_has_media {
            if incoming_has_media {
                existing.event.media_urls.extend(event.media_urls.clone());
                existing.event.media_types.extend(event.media_types.clone());
                existing
                    .event
                    .media_text_inlined
                    .extend(incoming_inline_flags);
            }
            if !event.text.is_empty() {
                if !existing.event.text.is_empty() {
                    existing.event.text = merge_caption(&existing.event.text, &event.text);
                } else {
                    existing.event.text = event.text.clone();
                }
            }
            if existing_is_photo || incoming_is_photo {
                existing.event.message_type = MessageType::Photo;
            } else if existing.event.message_type == MessageType::Text
                && event.message_type != MessageType::Text
            {
                existing.event.message_type = event.message_type;
            }
            existing.stt.invalidate();
            return;
        }

        if merge_text
            && existing.event.message_type == MessageType::Text
            && event.message_type == MessageType::Text
        {
            if !event.text.is_empty() {
                existing.event.text = if !existing.event.text.is_empty() {
                    format!("{}\n{}", existing.event.text, event.text)
                } else {
                    event.text.clone()
                };
            }
            return;
        }
    }

    pending.insert(session_key.to_string(), PendingMessage::new(event));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pending_stt::{
        echo_pending_stt_transcripts_once, transcribe_pending_audio_event_once,
    };
    use std::future::Future;

    // --- fixtures ---------------------------------------------------------

    /// A PHOTO event carrying one attachment and the given caption.
    fn photo_event(caption: &str) -> MessageEvent {
        MessageEvent {
            message_type: MessageType::Photo,
            media_urls: vec!["p1.jpg".to_string()],
            media_types: vec!["image/jpeg".to_string()],
            ..MessageEvent::new(caption)
        }
    }

    /// A VOICE event carrying one STT-eligible clip under the given path.
    fn voice_event(path: &str) -> MessageEvent {
        MessageEvent {
            message_type: MessageType::Voice,
            media_urls: vec![path.to_string()],
            media_types: vec![String::new()],
            ..MessageEvent::new("")
        }
    }

    /// A plain text event with no media.
    fn text_event(text: &str) -> MessageEvent {
        MessageEvent::new(text)
    }

    /// Drive a future to completion on a single-threaded runtime.
    fn block_on<F: Future>(fut: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(fut)
    }

    // --- merge_caption ----------------------------------------------------

    #[test]
    fn merge_caption_appends_distinct_and_dedupes_exact() {
        // Empty existing returns the new text verbatim.
        assert_eq!(merge_caption("", "hello"), "hello");
        // A distinct caption is appended with a blank-line separator.
        assert_eq!(merge_caption("first", "second"), "first\n\nsecond");
        // An exact duplicate (after trimming) is not re-appended.
        assert_eq!(
            merge_caption("first\n\nsecond", "  second  "),
            "first\n\nsecond"
        );
        // Substring is NOT a duplicate: "Meeting" is kept even inside "Meeting agenda".
        assert_eq!(
            merge_caption("Meeting agenda", "Meeting"),
            "Meeting agenda\n\nMeeting"
        );
    }

    #[test]
    fn merge_caption_strips_python_information_separators() {
        // U+001C..U+001F are whitespace to Python str.strip, so the trimmed
        // "dupe" matches and is deduped.
        assert_eq!(merge_caption("dupe", "\u{1c}dupe\u{1f}"), "dupe");
        // The final concatenation is trimmed of trailing separators too.
        assert_eq!(merge_caption("keep", "tail\u{1f}"), "keep\n\ntail");
    }

    // --- merge behavior ---------------------------------------------------

    #[test]
    fn photo_photo_merges_caption_even_with_empty_media() {
        let mut pending = HashMap::new();
        merge_pending_message_event(&mut pending, "k", photo_event("cap1"), false);

        // Incoming photo has NO media but a caption; caption still merges.
        let incoming = MessageEvent {
            message_type: MessageType::Photo,
            ..MessageEvent::new("cap2")
        };
        merge_pending_message_event(&mut pending, "k", incoming, false);

        let m = &pending["k"];
        assert_eq!(m.event.text, "cap1\n\ncap2");
        // Media untouched: only the original attachment remains.
        assert_eq!(m.event.media_urls, vec!["p1.jpg".to_string()]);
        assert_eq!(m.event.message_type, MessageType::Photo);
    }

    #[test]
    fn media_merge_concatenates_mime_as_is_and_pads_inline_flags() {
        let mut pending = HashMap::new();
        merge_pending_message_event(
            &mut pending,
            "k",
            MessageEvent {
                message_type: MessageType::Photo,
                media_urls: vec!["a.jpg".to_string()],
                media_types: vec!["image/jpeg".to_string()],
                ..MessageEvent::new("")
            },
            false,
        );

        // Incoming: two URLs but only one MIME (short array) and an overlong
        // inline-flag array (three flags for two URLs).
        merge_pending_message_event(
            &mut pending,
            "k",
            MessageEvent {
                message_type: MessageType::Photo,
                media_urls: vec!["b.jpg".to_string(), "c.jpg".to_string()],
                media_types: vec!["image/png".to_string()],
                media_text_inlined: vec![Some(true), None, Some(false)],
                ..MessageEvent::new("")
            },
            false,
        );

        let m = &pending["k"];
        assert_eq!(
            m.event.media_urls,
            vec![
                "a.jpg".to_string(),
                "b.jpg".to_string(),
                "c.jpg".to_string()
            ]
        );
        // MIME concatenated as-is, never padded to match the URL count.
        assert_eq!(
            m.event.media_types,
            vec!["image/jpeg".to_string(), "image/png".to_string()]
        );
        // Existing flags padded to its one URL (None), then the overlong
        // incoming flags appended without truncation.
        assert_eq!(
            m.event.media_text_inlined,
            vec![None, Some(true), None, Some(false)]
        );
    }

    #[test]
    fn media_merge_resolves_message_type() {
        // PHOTO on the incoming side dominates a TEXT existing.
        let mut pending = HashMap::new();
        merge_pending_message_event(
            &mut pending,
            "k",
            MessageEvent {
                media_urls: vec!["doc.pdf".to_string()],
                media_types: vec!["application/pdf".to_string()],
                ..MessageEvent::new("")
            },
            false,
        );
        merge_pending_message_event(&mut pending, "k", photo_event("pic"), false);
        assert_eq!(pending["k"].event.message_type, MessageType::Photo);

        // A TEXT existing upgrades to a non-TEXT, non-PHOTO incoming type.
        let mut pending = HashMap::new();
        merge_pending_message_event(
            &mut pending,
            "k",
            MessageEvent {
                media_urls: vec!["f.bin".to_string()],
                media_types: vec![String::new()],
                ..MessageEvent::new("")
            },
            false,
        );
        merge_pending_message_event(
            &mut pending,
            "k",
            MessageEvent {
                message_type: MessageType::Video,
                media_urls: vec!["clip.mp4".to_string()],
                media_types: vec!["video/mp4".to_string()],
                ..MessageEvent::new("")
            },
            false,
        );
        assert_eq!(pending["k"].event.message_type, MessageType::Video);

        // A PHOTO existing stays PHOTO even when a VIDEO merges in.
        let mut pending = HashMap::new();
        merge_pending_message_event(&mut pending, "k", photo_event(""), false);
        merge_pending_message_event(
            &mut pending,
            "k",
            MessageEvent {
                message_type: MessageType::Video,
                media_urls: vec!["clip.mp4".to_string()],
                media_types: vec!["video/mp4".to_string()],
                ..MessageEvent::new("")
            },
            false,
        );
        assert_eq!(pending["k"].event.message_type, MessageType::Photo);
    }

    #[test]
    fn merge_preserves_original_identity_and_reply_fields() {
        let mut existing = photo_event("cap");
        existing.message_id = Some("orig-id".to_string());
        existing.reply_to_message_id = Some("reply-1".to_string());

        let mut pending = HashMap::new();
        merge_pending_message_event(&mut pending, "k", existing, false);

        let mut incoming = photo_event("more");
        incoming.message_id = Some("new-id".to_string());
        incoming.reply_to_message_id = Some("reply-2".to_string());
        merge_pending_message_event(&mut pending, "k", incoming, false);

        let m = &pending["k"];
        assert_eq!(m.event.message_id.as_deref(), Some("orig-id"));
        assert_eq!(m.event.reply_to_message_id.as_deref(), Some("reply-1"));
    }

    #[test]
    fn merge_invalidates_stt_cache_but_retains_echo_ledger() {
        // Build a transcribed + echoed pending voice message.
        let mut pm = PendingMessage::new(voice_event("vn.ogg"));
        block_on(transcribe_pending_audio_event_once(
            &mut pm.stt,
            &pm.event,
            None,
            |_text, _paths| async move {
                Ok((Some("enriched".to_string()), vec!["hello".to_string()]))
            },
        ))
        .unwrap();
        block_on(echo_pending_stt_transcripts_once(
            &mut pm.stt,
            &["hello".to_string()],
            true,
            true,
            |_line| async move { Ok(()) },
        ))
        .unwrap();
        assert!(pm.stt.is_prepared());
        assert_eq!(pm.stt.echoed_count(), 1);

        let mut pending = HashMap::new();
        pending.insert("k".to_string(), pm);

        // A second voice note merges its media in.
        merge_pending_message_event(&mut pending, "k", voice_event("second.ogg"), false);

        let m = &pending["k"];
        // Only the derived cache is dropped.
        assert!(!m.stt.is_prepared());
        // The echo ledger survives so the re-run transcription does not re-echo
        // what the user already saw.
        assert_eq!(m.stt.echoed_count(), 1);
        // Media concatenated across the two notes.
        assert_eq!(
            m.event.media_urls,
            vec!["vn.ogg".to_string(), "second.ogg".to_string()]
        );
    }

    #[test]
    fn merge_text_joins_consecutive_text_events() {
        let mut pending = HashMap::new();
        merge_pending_message_event(&mut pending, "k", text_event("one"), true);
        merge_pending_message_event(&mut pending, "k", text_event("two"), true);
        // A single newline (not the blank-line caption separator) joins bursts.
        assert_eq!(pending["k"].event.text, "one\ntwo");
    }

    #[test]
    fn replacement_resets_stt_state() {
        // Existing text-only bundle whose STT ledger already advanced.
        let mut pm = PendingMessage::new(text_event("old"));
        block_on(echo_pending_stt_transcripts_once(
            &mut pm.stt,
            &["x".to_string()],
            true,
            true,
            |_line| async move { Ok(()) },
        ))
        .unwrap();
        assert_eq!(pm.stt.echoed_count(), 1);

        let mut pending = HashMap::new();
        pending.insert("k".to_string(), pm);

        // TEXT over TEXT with merge_text off falls through to replacement.
        merge_pending_message_event(&mut pending, "k", text_event("new"), false);

        let m = &pending["k"];
        assert_eq!(m.event.text, "new");
        // The replacement bundle starts with fresh STT state.
        assert!(!m.stt.is_prepared());
        assert_eq!(m.stt.echoed_count(), 0);
    }

    #[test]
    fn first_event_is_stored_verbatim() {
        let mut pending = HashMap::new();
        merge_pending_message_event(&mut pending, "k", text_event("solo"), false);
        assert_eq!(pending["k"].event.text, "solo");
        assert!(!pending["k"].stt.is_prepared());
    }
}

// ---------------------------------------------------------------------------
// Python differential coverage
// ---------------------------------------------------------------------------
// Compare merged event payloads with the executable Python merge contract.
#[cfg(test)]
mod golden_corpus {
    use crate::pending_messages::merge_pending_message_event;
    use crate::platform_base_types::{MessageEvent, MessageType};
    use serde_json::{json, Value};
    use std::collections::HashMap;

    fn event(value: &Value) -> MessageEvent {
        MessageEvent {
            text: value["text"].as_str().unwrap().into(),
            message_type: MessageType::from_value(value["message_type"].as_str().unwrap()).unwrap(),
            message_id: value["message_id"].as_str().map(str::to_owned),
            reply_to_message_id: value["reply_to_message_id"].as_str().map(str::to_owned),
            media_urls: serde_json::from_value(value["media_urls"].clone()).unwrap(),
            media_types: serde_json::from_value(value["media_types"].clone()).unwrap(),
            media_text_inlined: serde_json::from_value(value["media_text_inlined"].clone())
                .unwrap(),
            ..Default::default()
        }
    }

    #[test]
    fn pending_event_merges_match_python() {
        let cases: Value =
            serde_json::from_str(include_str!("../../../tools/pending-message-goldens.json"))
                .unwrap();
        for (index, case) in cases.as_array().unwrap().iter().enumerate() {
            let mut pending = HashMap::new();
            merge_pending_message_event(&mut pending, "s", event(&case["before"]), false);
            merge_pending_message_event(
                &mut pending,
                "other",
                MessageEvent::new("unrelated"),
                false,
            );
            merge_pending_message_event(
                &mut pending,
                "s",
                event(&case["incoming"]),
                case["merge_text"].as_bool().unwrap(),
            );
            let merged = pending.remove("s").unwrap().event;
            let actual = json!({
                "text": merged.text,
                "message_type": merged.message_type.value(),
                "message_id": merged.message_id,
                "reply_to_message_id": merged.reply_to_message_id,
                "media_urls": merged.media_urls,
                "media_types": merged.media_types,
                "media_text_inlined": merged.media_text_inlined,
            });
            assert_eq!(actual, case["expected"], "Python merge case {index}");
            assert_eq!(pending.len(), 1);
            assert_eq!(pending["other"].event.text, "unrelated");
        }
    }
}
