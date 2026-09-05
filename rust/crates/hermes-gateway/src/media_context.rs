//! Port of pure media text builders from `gateway/run.py`.
//!
// Public API is ahead of its callers while GatewayRunner is ported.
#![allow(dead_code)]
//!
//! This module ports the four pure text formatting functions used when
//! handling inbound attachments:
//! - `build_media_placeholder`: text placeholder for media-only events so queued
//!   attachments without captions are not dropped.
//! - `build_document_context_note`: context note prepended when a user attaches
//!   a document (distinguishing inlined text, cached non-inlined text, and binary).
//! - `build_audio_context_note`: context note prepended when a user attaches an
//!   audio file.
//! - `build_video_context_note`: context note prepended when a user attaches a
//!   video file.
//!
//! Scope boundary: these builders are pure string generators. Path translation
//! (e.g. host-to-container cache mapping via `to_agent_visible_cache_path`) and
//! path resolution are caller concerns. [`attachment_display_name`] provides
//! the filename normalization shared by the audio, video and document branches.

use crate::inbound_media::{event_media_is_audio, event_media_is_image, event_media_is_video};
use crate::platform_base_types::MessageEvent;
use unicode_general_category::{get_general_category, GeneralCategory};

/// Remove the cache's two underscore-separated prefixes and scrub the display
/// name exactly as the Python inbound pipeline does. This is a label, not a
/// filesystem path or a file-access authorization check.
pub fn attachment_display_name(path: &str) -> String {
    // POSIX basename preserves an empty basename after a trailing slash. Using
    // Path::file_name would instead return the preceding directory component.
    let basename = path.rsplit('/').next().unwrap_or("");
    let display = basename.splitn(3, '_').nth(2).unwrap_or(basename);
    display
        .chars()
        .map(|c| {
            // Python regex \w includes Unicode letters and numbers plus underscore,
            // but excludes combining marks (Rust's Unicode Alphabetic includes some).
            let word = matches!(
                get_general_category(c),
                GeneralCategory::UppercaseLetter
                    | GeneralCategory::LowercaseLetter
                    | GeneralCategory::TitlecaseLetter
                    | GeneralCategory::ModifierLetter
                    | GeneralCategory::OtherLetter
                    | GeneralCategory::DecimalNumber
                    | GeneralCategory::LetterNumber
                    | GeneralCategory::OtherNumber
            );
            if word || matches!(c, '_' | '.' | '-' | ' ') {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// Resolve unknown document MIME types before building the attachment note.
/// The gateway deliberately treats common text/config extensions as plain text
/// even if the host MIME database assigns them an application/* type.
pub fn document_mime(path: &str, supplied: &str) -> String {
    if !matches!(supplied, "" | "application/octet-stream") {
        return supplied.into();
    }
    let extension = crate::mime_types::split_extension(path).1.to_lowercase();
    if matches!(
        extension.as_str(),
        ".txt"
            | ".md"
            | ".csv"
            | ".log"
            | ".json"
            | ".xml"
            | ".yaml"
            | ".yml"
            | ".toml"
            | ".ini"
            | ".cfg"
    ) {
        return "text/plain".into();
    }
    crate::mime_types::guess_path_type(std::path::Path::new(path))
        .unwrap_or_else(|| "application/octet-stream".into())
}

/// Build a text placeholder for media-only events so they are not dropped.
///
/// When a photo/document is queued during active processing and later
/// dequeued, only `.text` is extracted. If the event has no caption,
/// the media would be silently lost. This builds a placeholder that
/// the vision enrichment pipeline will replace with a real description.
///
/// Matches Python `_build_media_placeholder(event)`.
pub fn build_media_placeholder(event: &MessageEvent) -> String {
    let mut parts = Vec::with_capacity(event.media_urls.len());
    for (i, url) in event.media_urls.iter().enumerate() {
        if event_media_is_image(event, i) {
            parts.push(format!("[User sent an image: {url}]"));
        } else if event_media_is_audio(event, i) {
            parts.push(format!("[User sent audio: {url}]"));
        } else if event_media_is_video(event, i) {
            parts.push(format!("[User sent a video: {url}]"));
        } else {
            parts.push(format!("[User sent a file: {url}]"));
        }
    }
    parts.join("\n")
}

/// Context note prepended to a user turn when they attach a document.
///
/// Text documents (`text/*`) are usually inlined upstream by the platform
/// adapter. `content_inlined = false` records adapters that cache the file
/// without injecting its content, so the note tells the agent to read it.
///
/// Binary documents (PDF, DOCX, XLSX, ...) cannot be inlined as text. The note
/// tells the agent to extract the text itself before answering.
///
/// Matches Python `_build_document_context_note(display_name, agent_path, mtype, content_inlined=True)`.
pub fn build_document_context_note(
    display_name: &str,
    agent_path: &str,
    mtype: &str,
    content_inlined: bool,
) -> String {
    if mtype.starts_with("text/") && content_inlined {
        format!(
            "[The user sent a text document: '{display_name}'. Its content has been included below. The file is also saved at: {agent_path}]"
        )
    } else if mtype.starts_with("text/") {
        format!(
            "[The user sent a text document: '{display_name}'. It is saved at: {agent_path}. Its content is not inlined here. Read the cached file yourself before answering when the user's request involves its contents.]"
        )
    } else {
        format!(
            "[The user sent a document: '{display_name}'. It is saved at: {agent_path}. Its text is not inlined here (it's a binary format such as PDF or DOCX). To read it, extract the document's text yourself \u{2014} for example with the terminal tool or the ocr-and-documents skill \u{2014} before answering, instead of asking the user to paste the contents.]"
        )
    }
}

/// Context note prepended to a user turn when an audio file attachment is preserved as a path.
///
/// Parameters `display_name` and `agent_path` must already be resolved and sanitized by the caller.
/// Matches the audio note literal in Python `GatewayRunner._prepare_inbound_message_text`.
pub fn build_audio_context_note(display_name: &str, agent_path: &str) -> String {
    format!(
        "[The user sent an audio file attachment: '{display_name}'. It is saved at: {agent_path}. Its content is not inlined here. If the user's request involves what the audio contains, transcribe or process it yourself \u{2014} for example by passing the path to a transcription or media tool \u{2014} instead of asking the user to describe it. Only ask what to do with it if their intent is genuinely unclear.]"
    )
}

/// Context note prepended to a user turn when a video attachment is preserved as a path.
///
/// Parameters `display_name` and `agent_path` must already be resolved and sanitized by the caller.
/// Matches the video note literal in Python `GatewayRunner._prepare_inbound_message_text`.
pub fn build_video_context_note(display_name: &str, agent_path: &str) -> String {
    format!(
        "[The user sent a video attachment: '{display_name}'. It is saved at: {agent_path}. Its content is not inlined here. If the user's request involves what the video contains, inspect or process it yourself \u{2014} for example by passing the path to a video analysis or media tool \u{2014} instead of asking the user to describe it. Only ask what to do with it if their intent is genuinely unclear.]"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform_base_types::MessageType;

    fn make_event(message_type: MessageType, attachments: &[(&str, &str)]) -> MessageEvent {
        MessageEvent {
            message_type,
            media_urls: attachments.iter().map(|(p, _)| p.to_string()).collect(),
            media_types: attachments.iter().map(|(_, m)| m.to_string()).collect(),
            ..Default::default()
        }
    }

    // --- build_media_placeholder ------------------------------------------

    #[test]
    fn test_media_placeholder_empty_media_urls() {
        let event = make_event(MessageType::Text, &[]);
        assert_eq!(build_media_placeholder(&event), "");
    }

    #[test]
    fn test_media_placeholder_mixed_attachment_ordering() {
        let event = make_event(
            MessageType::Text,
            &[
                ("/tmp/photo.png", "image/png"),
                ("/tmp/voice.ogg", "audio/ogg"),
                ("/tmp/clip.mp4", "video/mp4"),
                ("/tmp/manual.pdf", "application/pdf"),
            ],
        );
        let expected = "\
[User sent an image: /tmp/photo.png]\n\
[User sent audio: /tmp/voice.ogg]\n\
[User sent a video: /tmp/clip.mp4]\n\
[User sent a file: /tmp/manual.pdf]";
        assert_eq!(build_media_placeholder(&event), expected);
    }

    #[test]
    fn test_media_placeholder_missing_mime_precedence() {
        let photo_event = make_event(MessageType::Photo, &[("/tmp/unknown_img", "")]);
        assert_eq!(
            build_media_placeholder(&photo_event),
            "[User sent an image: /tmp/unknown_img]"
        );

        let voice_event = make_event(MessageType::Voice, &[("/tmp/unknown_voice", "")]);
        assert_eq!(
            build_media_placeholder(&voice_event),
            "[User sent audio: /tmp/unknown_voice]"
        );

        let audio_event = make_event(MessageType::Audio, &[("/tmp/unknown_audio", "")]);
        assert_eq!(
            build_media_placeholder(&audio_event),
            "[User sent audio: /tmp/unknown_audio]"
        );

        let video_event = make_event(MessageType::Video, &[("/tmp/unknown_video", "")]);
        assert_eq!(
            build_media_placeholder(&video_event),
            "[User sent a video: /tmp/unknown_video]"
        );

        let doc_event = make_event(MessageType::Document, &[("/tmp/unknown_doc", "")]);
        assert_eq!(
            build_media_placeholder(&doc_event),
            "[User sent a file: /tmp/unknown_doc]"
        );
    }

    #[test]
    fn test_media_placeholder_explicit_mime_overrides_message_type() {
        let event = make_event(
            MessageType::Photo,
            &[
                ("/c/product.png", "image/png"),
                ("/c/brief.md", "text/markdown"),
            ],
        );
        let placeholder = build_media_placeholder(&event);
        assert_eq!(
            placeholder,
            "[User sent an image: /c/product.png]\n[User sent a file: /c/brief.md]"
        );
    }

    #[test]
    fn test_media_placeholder_case_and_whitespace_mime_sensitivity() {
        let event = make_event(MessageType::Photo, &[("/c/bad.png", "Image/png")]);
        assert_eq!(
            build_media_placeholder(&event),
            "[User sent a file: /c/bad.png]"
        );
    }

    // --- build_document_context_note --------------------------------------

    #[test]
    fn test_document_context_note_text_inlined() {
        let note =
            build_document_context_note("notes.txt", "/cache/doc_notes.txt", "text/plain", true);
        assert_eq!(
            note,
            "[The user sent a text document: 'notes.txt'. Its content has been included below. The file is also saved at: /cache/doc_notes.txt]"
        );
    }

    #[test]
    fn test_document_context_note_text_not_inlined() {
        let note =
            build_document_context_note("notes.txt", "/cache/doc_notes.txt", "text/plain", false);
        assert_eq!(
            note,
            "[The user sent a text document: 'notes.txt'. It is saved at: /cache/doc_notes.txt. Its content is not inlined here. Read the cached file yourself before answering when the user's request involves its contents.]"
        );
    }

    #[test]
    fn test_document_context_note_binary() {
        let note = build_document_context_note(
            "contract.pdf",
            "/cache/doc_contract.pdf",
            "application/pdf",
            false,
        );
        assert_eq!(
            note,
            "[The user sent a document: 'contract.pdf'. It is saved at: /cache/doc_contract.pdf. Its text is not inlined here (it's a binary format such as PDF or DOCX). To read it, extract the document's text yourself \u{2014} for example with the terminal tool or the ocr-and-documents skill \u{2014} before answering, instead of asking the user to paste the contents.]"
        );
    }

    #[test]
    fn test_document_context_note_binary_ignores_inlined_flag() {
        let note = build_document_context_note(
            "sheet.xlsx",
            "/cache/sheet.xlsx",
            "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
            true,
        );
        assert_eq!(
            note,
            "[The user sent a document: 'sheet.xlsx'. It is saved at: /cache/sheet.xlsx. Its text is not inlined here (it's a binary format such as PDF or DOCX). To read it, extract the document's text yourself \u{2014} for example with the terminal tool or the ocr-and-documents skill \u{2014} before answering, instead of asking the user to paste the contents.]"
        );
    }

    #[test]
    fn test_document_context_note_em_dash_byte_parity() {
        let note = build_document_context_note(
            "doc.bin",
            "/path/doc.bin",
            "application/octet-stream",
            false,
        );
        let em_dash_bytes = [0xe2, 0x80, 0x94];
        let occurrences = note
            .as_bytes()
            .windows(3)
            .filter(|w| *w == em_dash_bytes)
            .count();
        assert_eq!(occurrences, 2);
    }

    // --- build_audio_context_note -----------------------------------------

    #[test]
    fn test_audio_context_note() {
        let note = build_audio_context_note("track.mp3", "/cache/track.mp3");
        assert_eq!(
            note,
            "[The user sent an audio file attachment: 'track.mp3'. It is saved at: /cache/track.mp3. Its content is not inlined here. If the user's request involves what the audio contains, transcribe or process it yourself \u{2014} for example by passing the path to a transcription or media tool \u{2014} instead of asking the user to describe it. Only ask what to do with it if their intent is genuinely unclear.]"
        );
        let em_dash_bytes = [0xe2, 0x80, 0x94];
        let occurrences = note
            .as_bytes()
            .windows(3)
            .filter(|w| *w == em_dash_bytes)
            .count();
        assert_eq!(occurrences, 2);
    }

    // --- build_video_context_note -----------------------------------------

    #[test]
    fn test_video_context_note() {
        let note = build_video_context_note("clip.mp4", "/cache/clip.mp4");
        assert_eq!(
            note,
            "[The user sent a video attachment: 'clip.mp4'. It is saved at: /cache/clip.mp4. Its content is not inlined here. If the user's request involves what the video contains, inspect or process it yourself \u{2014} for example by passing the path to a video analysis or media tool \u{2014} instead of asking the user to describe it. Only ask what to do with it if their intent is genuinely unclear.]"
        );
        let em_dash_bytes = [0xe2, 0x80, 0x94];
        let occurrences = note
            .as_bytes()
            .windows(3)
            .filter(|w| *w == em_dash_bytes)
            .count();
        assert_eq!(occurrences, 2);
    }
}

// ---------------------------------------------------------------------------
// Python differential coverage
// ---------------------------------------------------------------------------
// Compare real Rust builder outputs with executed Python expressions.
#[cfg(test)]
mod golden_corpus {
    use super::*;
    use crate::platform_base_types::{MessageEvent, MessageType};
    use serde_json::Value;

    #[test]
    fn context_notes_match_python() {
        let fixture: Value =
            serde_json::from_str(include_str!("../../../tools/media-context-goldens.json"))
                .unwrap();
        for case in fixture["names"].as_array().unwrap() {
            assert_eq!(
                attachment_display_name(case["path"].as_str().unwrap()),
                case["expected"].as_str().unwrap(),
                "{case}"
            );
        }
        for case in fixture["mime_cases"].as_array().unwrap() {
            assert_eq!(
                document_mime(
                    case["path"].as_str().unwrap(),
                    case["supplied"].as_str().unwrap()
                ),
                case["expected"].as_str().unwrap(),
                "{case}"
            );
        }
        for case in fixture["placeholders"].as_array().unwrap() {
            let event = MessageEvent {
                message_type: MessageType::from_value(case["message_type"].as_str().unwrap())
                    .unwrap(),
                media_urls: serde_json::from_value(case["media_urls"].clone()).unwrap(),
                media_types: serde_json::from_value(case["media_types"].clone()).unwrap(),
                ..Default::default()
            };
            assert_eq!(
                build_media_placeholder(&event),
                case["expected"].as_str().unwrap()
            );
        }
        for case in fixture["documents"].as_array().unwrap() {
            assert_eq!(
                build_document_context_note(
                    case["display_name"].as_str().unwrap(),
                    case["agent_path"].as_str().unwrap(),
                    case["mtype"].as_str().unwrap(),
                    case["content_inlined"].as_bool().unwrap(),
                ),
                case["expected"].as_str().unwrap()
            );
        }
        for case in fixture["attachments"].as_array().unwrap() {
            let name = case["display_name"].as_str().unwrap();
            let path = case["agent_path"].as_str().unwrap();
            assert_eq!(
                build_audio_context_note(name, path),
                case["audio"].as_str().unwrap()
            );
            assert_eq!(
                build_video_context_note(name, path),
                case["video"].as_str().unwrap()
            );
        }
    }
}
