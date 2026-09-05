//! Port of the inbound-media classification helpers from `gateway/run.py`.
//!
// Public API is ahead of its callers while the runner (GatewayRunner) is ported.
#![allow(dead_code)]
//!
//! This is a bounded Tier 2 slice: the five per-attachment classifiers
//! (`_event_media_type_at`, `_event_media_is_image`, `_event_media_is_audio`,
//! `_event_media_is_stt_input`, `_event_media_is_video`) plus the `classify_media`
//! helper that reproduces the attachment-bucketing loop inside
//! `GatewayRunner._prepare_inbound_message_text`.
//!
//! Scope boundary: this file only *classifies* attachments into buckets. The
//! network enrichment that consumes those buckets (vision routing / image mode
//! decision, transcription, the audio-file and video context notes, the
//! document path-note fallthrough) and model/runtime resolution stay with the
//! runner and are ported separately. Nothing here does IO.
//!
//! The Python source is the specification; its quirks are preserved verbatim:
//!
//!   * Per-attachment MIME wins when present, and the message-level
//!     `MessageType` fallback is consulted ONLY when this slot's MIME is the
//!     empty string. A document uploaded alongside an image (whole-message type
//!     `PHOTO`) must NOT be mis-routed as an image, or the provider 400s on a
//!     base64 vision part it can't decode.
//!   * MIME matching is `str.startswith`, i.e. case-sensitive with no trimming.
//!     `"Image/png"` and `" image/png"` do NOT match `"image/"`, and because
//!     those strings are non-empty they also never fall through to the
//!     message-level type. This is faithful, not a bug.
//!   * The STT gate excludes `AUDIO` and `DOCUMENT` message types up front:
//!     `AUDIO` is a real audio-file attachment (`.mp3`, `.m4a`) that is saved as
//!     a path rather than transcribed, and a `DOCUMENT` that merely happens to
//!     carry an `audio/*` MIME is likewise preserved as a file, never fed to STT.

use crate::platform_base_types::{MessageEvent, MessageType};

/// Return the per-attachment MIME for the attachment at `index`.
///
/// Empty string when the platform didn't populate a per-file MIME for that slot
/// (some adapters only set a message-level type). Mirrors Python
/// `media_types[index] if index < len(media_types) else ""`.
pub fn event_media_type_at(event: &MessageEvent, index: usize) -> &str {
    match event.media_types.get(index) {
        Some(m) => m.as_str(),
        None => "",
    }
}

/// True if the attachment at `index` is an image.
///
/// Trust the per-attachment MIME when present. Only fall back to the
/// message-level `PHOTO` type when this attachment's MIME is unknown --
/// otherwise a document (or any non-image) uploaded alongside an image in the
/// same message gets mis-routed as an image, base64'd into a vision content
/// part, and the provider 400s ("Could not process image").
pub fn event_media_is_image(event: &MessageEvent, index: usize) -> bool {
    let mtype = event_media_type_at(event, index);
    if !mtype.is_empty() {
        return mtype.starts_with("image/");
    }
    event.message_type == MessageType::Photo
}

/// True if the attachment at `index` is audio (per-attachment MIME first).
pub fn event_media_is_audio(event: &MessageEvent, index: usize) -> bool {
    let mtype = event_media_type_at(event, index);
    if !mtype.is_empty() {
        return mtype.starts_with("audio/");
    }
    matches!(event.message_type, MessageType::Voice | MessageType::Audio)
}

/// True when an audio attachment should enter the automatic STT pipeline.
///
/// `AUDIO` and `DOCUMENT` message types are excluded up front: an `AUDIO`
/// attachment is a real audio file preserved as a path, and a `DOCUMENT`
/// carrying an `audio/*` MIME is likewise kept as a file rather than
/// transcribed. Everything else is STT input when it's a `VOICE` note or its
/// per-attachment MIME starts with `audio/`.
pub fn event_media_is_stt_input(event: &MessageEvent, index: usize) -> bool {
    if matches!(
        event.message_type,
        MessageType::Audio | MessageType::Document
    ) {
        return false;
    }
    event.message_type == MessageType::Voice
        || event_media_type_at(event, index).starts_with("audio/")
}

/// True if the attachment at `index` is video (per-attachment MIME first).
pub fn event_media_is_video(event: &MessageEvent, index: usize) -> bool {
    let mtype = event_media_type_at(event, index);
    if !mtype.is_empty() {
        return mtype.starts_with("video/");
    }
    event.message_type == MessageType::Video
}

/// The four attachment buckets produced by the classification loop in
/// `GatewayRunner._prepare_inbound_message_text`.
///
/// Each bucket holds the local file paths (from `event.media_urls`) that landed
/// in it, in original attachment order. The current MIME prefixes and fallback
/// message types make these buckets disjoint. Independent conditions preserve
/// the Python loop's structure.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MediaClassification {
    /// Images to route through the vision path (native attach vs. text pre-run).
    pub image_paths: Vec<String>,
    /// Audio to feed the automatic STT / transcription enrichment. Named
    /// `audio_paths` in the Python loop.
    pub transcription_paths: Vec<String>,
    /// Audio-file attachments preserved as a path (never transcribed): the
    /// message type was `AUDIO` or `DOCUMENT`.
    pub audio_file_paths: Vec<String>,
    /// Video attachments preserved as a path.
    pub video_paths: Vec<String>,
}

/// Bucket every attachment in `event.media_urls`, faithfully reproducing the
/// classification loop in `GatewayRunner._prepare_inbound_message_text`.
///
/// `pending_stt_prepared` mirrors the Python `_pending_stt_prepared` flag: it is
/// true when the event already carries a prepared/cached transcription (Python
/// sets it from `hasattr(event, "_gateway_pending_stt_text")`). When true, the
/// automatic STT branch is skipped so audio is not transcribed a second time;
/// the attachment simply lands in no bucket here (its cached text is applied by
/// the runner, outside this slice).
///
/// The loop's structure, preserved exactly:
///
/// ```text
/// for (i, path) in media_urls:
///     if is_image(i):                          image_paths += path
///     if is_audio(i):
///         if message_type in {AUDIO, DOCUMENT}: audio_file_paths += path
///         elif !pending_stt_prepared && is_stt_input(i): transcription_paths += path
///     if is_video(i):                          video_paths += path
/// ```
pub fn classify_media(event: &MessageEvent, pending_stt_prepared: bool) -> MediaClassification {
    let mut out = MediaClassification::default();

    for (i, path) in event.media_urls.iter().enumerate() {
        if event_media_is_image(event, i) {
            out.image_paths.push(path.clone());
        }

        // AUDIO = audio-file attachment (e.g. .mp3, .m4a) -- never STT. Mixed
        // DOCUMENT events also preserve audio as a file path instead of
        // dropping it or treating it as a voice note.
        if event_media_is_audio(event, i) {
            if matches!(
                event.message_type,
                MessageType::Audio | MessageType::Document
            ) {
                out.audio_file_paths.push(path.clone());
            } else if !pending_stt_prepared && event_media_is_stt_input(event, i) {
                out.transcription_paths.push(path.clone());
            }
        }

        if event_media_is_video(event, i) {
            out.video_paths.push(path.clone());
        }
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build an event with the given message type and per-attachment
    /// (path, mime) pairs. A mime of "" leaves that slot's MIME unset.
    fn ev(message_type: MessageType, attachments: &[(&str, &str)]) -> MessageEvent {
        MessageEvent {
            message_type,
            media_urls: attachments.iter().map(|(p, _)| p.to_string()).collect(),
            media_types: attachments.iter().map(|(_, m)| m.to_string()).collect(),
            ..MessageEvent::new("")
        }
    }

    // --- event_media_type_at: out-of-range fallback -----------------------

    #[test]
    fn media_type_at_out_of_range_is_empty() {
        // One MIME populated, but the loop asks about a later slot: "".
        let e = ev(MessageType::Photo, &[("a.png", "image/png")]);
        assert_eq!(event_media_type_at(&e, 0), "image/png");
        assert_eq!(event_media_type_at(&e, 1), "");
        assert_eq!(event_media_type_at(&e, 99), "");
    }

    #[test]
    fn media_type_at_shorter_types_than_urls() {
        // media_urls longer than media_types: the tail slots read "" and fall
        // back to the message-level type.
        let mut e = ev(MessageType::Photo, &[("a.png", "image/png")]);
        e.media_urls.push("b.bin".to_string()); // no matching media_types entry
        assert_eq!(event_media_type_at(&e, 1), "");
        // Slot 1 has empty MIME -> message_type PHOTO makes it an image.
        assert!(event_media_is_image(&e, 1));
    }

    // --- explicit MIME precedence -----------------------------------------

    #[test]
    fn explicit_mime_wins_over_message_type() {
        // Whole message is a PHOTO, but attachment 1 carries a document MIME.
        // Per-attachment MIME must win: slot 0 is the image, slot 1 is NOT.
        let e = ev(
            MessageType::Photo,
            &[("a.png", "image/png"), ("b.pdf", "application/pdf")],
        );
        assert!(event_media_is_image(&e, 0));
        assert!(!event_media_is_image(&e, 1));
        assert!(!event_media_is_audio(&e, 1));
        assert!(!event_media_is_video(&e, 1));

        // The document alongside the image must not be bucketed anywhere here.
        let c = classify_media(&e, false);
        assert_eq!(c.image_paths, vec!["a.png".to_string()]);
        assert!(c.transcription_paths.is_empty());
        assert!(c.audio_file_paths.is_empty());
        assert!(c.video_paths.is_empty());
    }

    #[test]
    fn explicit_audio_and_video_mime() {
        // A TEXT message (no helpful message-level type) whose attachments are
        // classified purely by MIME.
        let e = ev(
            MessageType::Text,
            &[("a.mp3", "audio/mpeg"), ("b.mp4", "video/mp4")],
        );
        assert!(event_media_is_audio(&e, 0));
        assert!(event_media_is_video(&e, 1));
        assert!(!event_media_is_image(&e, 0));
    }

    // --- unknown MIME fallback --------------------------------------------

    #[test]
    fn unknown_mime_falls_back_to_message_type() {
        // Empty per-attachment MIME -> message-level type decides.
        let photo = ev(MessageType::Photo, &[("a", "")]);
        assert!(event_media_is_image(&photo, 0));

        let voice = ev(MessageType::Voice, &[("a", "")]);
        assert!(event_media_is_audio(&voice, 0));

        let audio = ev(MessageType::Audio, &[("a", "")]);
        assert!(event_media_is_audio(&audio, 0));

        let video = ev(MessageType::Video, &[("a", "")]);
        assert!(event_media_is_video(&video, 0));

        // A DOCUMENT with no MIME is none of image/audio/video.
        let doc = ev(MessageType::Document, &[("a", "")]);
        assert!(!event_media_is_image(&doc, 0));
        assert!(!event_media_is_audio(&doc, 0));
        assert!(!event_media_is_video(&doc, 0));
    }

    // --- VOICE vs AUDIO vs DOCUMENT STT gating ----------------------------

    #[test]
    fn stt_input_voice_note() {
        // A VOICE note with no MIME is STT input and routes to transcription.
        let e = ev(MessageType::Voice, &[("vn.ogg", "")]);
        assert!(event_media_is_stt_input(&e, 0));
        let c = classify_media(&e, false);
        assert_eq!(c.transcription_paths, vec!["vn.ogg".to_string()]);
        assert!(c.audio_file_paths.is_empty());
    }

    #[test]
    fn stt_input_audio_file_never_transcribed() {
        // AUDIO = a real audio file (.mp3). Excluded from STT; preserved as a
        // file path instead.
        let e = ev(MessageType::Audio, &[("song.mp3", "audio/mpeg")]);
        assert!(!event_media_is_stt_input(&e, 0));
        assert!(event_media_is_audio(&e, 0));
        let c = classify_media(&e, false);
        assert_eq!(c.audio_file_paths, vec!["song.mp3".to_string()]);
        assert!(c.transcription_paths.is_empty());
    }

    #[test]
    fn stt_input_document_with_audio_mime_kept_as_file() {
        // A DOCUMENT that happens to carry an audio/* MIME is audio (by MIME)
        // but excluded from STT, and bucketed as an audio file.
        let e = ev(MessageType::Document, &[("clip.wav", "audio/wav")]);
        assert!(event_media_is_audio(&e, 0));
        assert!(!event_media_is_stt_input(&e, 0));
        let c = classify_media(&e, false);
        assert_eq!(c.audio_file_paths, vec!["clip.wav".to_string()]);
        assert!(c.transcription_paths.is_empty());
    }

    #[test]
    fn stt_input_audio_mime_on_neutral_type() {
        // audio/* MIME on a non-AUDIO/DOCUMENT message (e.g. TEXT) is STT input.
        let e = ev(MessageType::Text, &[("a.oga", "audio/ogg")]);
        assert!(event_media_is_stt_input(&e, 0));
        let c = classify_media(&e, false);
        assert_eq!(c.transcription_paths, vec!["a.oga".to_string()]);
    }

    // --- mixed attachments ------------------------------------------------

    #[test]
    fn mixed_attachments_bucket_independently() {
        // One message carrying an image, a voice note, a plain document, and a
        // video, each classified by its own MIME (message type VOICE here is
        // overridden per-slot by MIME where present).
        let e = ev(
            MessageType::Voice,
            &[
                ("pic.jpg", "image/jpeg"),
                ("note.ogg", "audio/ogg"),
                ("doc.pdf", "application/pdf"),
                ("clip.mov", "video/quicktime"),
            ],
        );
        let c = classify_media(&e, false);
        assert_eq!(c.image_paths, vec!["pic.jpg".to_string()]);
        // note.ogg: audio + not AUDIO/DOCUMENT type + stt input -> transcription.
        assert_eq!(c.transcription_paths, vec!["note.ogg".to_string()]);
        assert!(c.audio_file_paths.is_empty());
        assert_eq!(c.video_paths, vec!["clip.mov".to_string()]);
        // doc.pdf is deliberately in no bucket here (runner handles documents).
    }

    #[test]
    fn mixed_document_message_with_audio_becomes_audio_file() {
        // Whole message is a DOCUMENT (a batch upload). The audio in it is
        // preserved as an audio file, the image still routes to vision.
        let e = ev(
            MessageType::Document,
            &[("a.png", "image/png"), ("b.mp3", "audio/mpeg")],
        );
        let c = classify_media(&e, false);
        assert_eq!(c.image_paths, vec!["a.png".to_string()]);
        assert_eq!(c.audio_file_paths, vec!["b.mp3".to_string()]);
        assert!(c.transcription_paths.is_empty());
    }

    // --- pending STT caching ----------------------------------------------

    #[test]
    fn pending_stt_prepared_suppresses_transcription() {
        // Same voice note, but a cached transcription is already prepared: the
        // automatic STT branch is skipped, so it lands in no bucket here.
        let e = ev(MessageType::Voice, &[("vn.ogg", "")]);
        let c = classify_media(&e, true);
        assert!(c.transcription_paths.is_empty());
        assert!(c.audio_file_paths.is_empty());

        // Without the flag it would transcribe (guards against a false pass).
        let c_off = classify_media(&e, false);
        assert_eq!(c_off.transcription_paths, vec!["vn.ogg".to_string()]);
    }

    #[test]
    fn pending_stt_does_not_affect_audio_files() {
        // The pending flag only gates the transcription branch; an AUDIO file
        // is still preserved as a file path regardless.
        let e = ev(MessageType::Audio, &[("song.mp3", "audio/mpeg")]);
        let c = classify_media(&e, true);
        assert_eq!(c.audio_file_paths, vec!["song.mp3".to_string()]);
    }

    // --- case / whitespace handling (startswith is exact) -----------------

    #[test]
    fn mime_matching_is_case_and_whitespace_sensitive() {
        // "Image/png" is non-empty, so it never falls back to message_type, and
        // startswith("image/") is case-sensitive -> NOT an image.
        let upper = ev(MessageType::Photo, &[("a.png", "Image/png")]);
        assert!(!event_media_is_image(&upper, 0));

        // Leading whitespace likewise defeats the prefix match, and the
        // non-empty MIME still blocks the message-type fallback.
        let spaced = ev(MessageType::Photo, &[("a.png", " image/png")]);
        assert!(!event_media_is_image(&spaced, 0));

        // Same for audio and video prefixes.
        let up_audio = ev(MessageType::Voice, &[("a.ogg", "Audio/ogg")]);
        assert!(!event_media_is_audio(&up_audio, 0));
        let up_video = ev(MessageType::Video, &[("a.mp4", "VIDEO/mp4")]);
        assert!(!event_media_is_video(&up_video, 0));

        // And an exact lowercase prefix still matches (control).
        let ok = ev(MessageType::Text, &[("a.png", "image/png")]);
        assert!(event_media_is_image(&ok, 0));
    }

    #[test]
    fn empty_media_urls_yields_empty_buckets() {
        let e = ev(MessageType::Text, &[]);
        let c = classify_media(&e, false);
        assert_eq!(c, MediaClassification::default());
    }
}

// ---------------------------------------------------------------------------
// Python differential coverage
// ---------------------------------------------------------------------------
// Differential coverage from the executable Python source, including its
// classification loop. Regenerate with tools/gen_inbound_media_goldens.py.
#[cfg(test)]
mod golden_corpus {
    use super::*;
    use crate::platform_base_types::{MessageEvent, MessageType};
    use serde::Deserialize;

    #[derive(Deserialize)]
    struct Case {
        message_type: String,
        media_types: Vec<String>,
        media_urls: Vec<String>,
        pending_stt_prepared: bool,
        predicates: Vec<(String, bool, bool, bool, bool)>,
        classified: Expected,
    }

    #[derive(Deserialize)]
    struct Expected {
        image_paths: Vec<String>,
        transcription_paths: Vec<String>,
        audio_file_paths: Vec<String>,
        video_paths: Vec<String>,
    }

    #[test]
    fn attachment_rules_match_python_source() {
        let cases: Vec<Case> =
            serde_json::from_str(include_str!("../../../tools/inbound-media-goldens.json"))
                .unwrap();
        assert!(!cases.is_empty());
        for (number, case) in cases.into_iter().enumerate() {
            let event = MessageEvent {
                message_type: MessageType::from_value(&case.message_type).unwrap(),
                media_types: case.media_types,
                media_urls: case.media_urls,
                ..Default::default()
            };
            for (index, expected) in case.predicates.into_iter().enumerate() {
                let actual = (
                    event_media_type_at(&event, index).to_owned(),
                    event_media_is_image(&event, index),
                    event_media_is_audio(&event, index),
                    event_media_is_stt_input(&event, index),
                    event_media_is_video(&event, index),
                );
                assert_eq!(actual, expected, "Python case {number}, slot {index}");
            }
            let actual = classify_media(&event, case.pending_stt_prepared);
            let expected = MediaClassification {
                image_paths: case.classified.image_paths,
                transcription_paths: case.classified.transcription_paths,
                audio_file_paths: case.classified.audio_file_paths,
                video_paths: case.classified.video_paths,
            };
            assert_eq!(actual, expected, "Python classification case {number}");
        }
    }
}
