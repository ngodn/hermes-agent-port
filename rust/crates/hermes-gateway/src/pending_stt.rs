//! Port of the pending voice-preprocessing state from `gateway/run.py`.
//!
// Public API is ahead of its callers while the runner (GatewayRunner) is ported.
#![allow(dead_code)]
//!
//! A voice follow-up can be inspected first by the interrupt monitor and later
//! consumed by the pending-drain path. Both need the same transcript, but only
//! one STT call and one transcript echo should happen per platform message.
//! Python carries that transient state on the `MessageEvent` via `setattr`
//! (`_gateway_pending_stt_text`, `_gateway_pending_stt_transcripts`,
//! `_gateway_pending_stt_echoed`). We keep `MessageEvent` untouched and model
//! the state explicitly as [`PendingStt`], which the runner owns and threads
//! alongside the event.
//!
//! Ported pieces (`gateway/run.py`):
//!   * `_pending_event_audio_paths`            -> [`pending_event_audio_paths`]
//!   * `_transcribe_pending_audio_event_once`  -> [`transcribe_pending_audio_event_once`]
//!   * `_echo_pending_stt_transcripts_once`    -> [`echo_pending_stt_transcripts_once`]
//!   * `_prepare_clarify_reply_text`           -> [`prepare_clarify_reply_text`]
//!
//! Invalidation semantics come from `gateway/platforms/base.py`
//! `_invalidate_pending_stt_cache` (called by `merge_pending_message_event`):
//! when a second voice note is merged into a pending event, only the *derived*
//! transcription cache is dropped ([`PendingStt::invalidate`]); the echo ledger
//! survives so the re-run transcription (which returns the earlier notes as a
//! prefix) does not re-echo what the user already saw.
//!
//! The actual STT backend and chat sends stay with the runner: transcription
//! and echo are supplied as async callback closures so callers wire in their
//! existing `_enrich_message_with_transcription` and `adapter.send`.

use std::future::Future;

use anyhow::Result;

use crate::inbound_media::event_media_is_stt_input;
use crate::platform_base_types::MessageEvent;

// Python str.strip also treats the four ASCII information separators as
// whitespace. Rust str.trim alone would leave these in clarify replies.
fn trim_python_whitespace(text: &str) -> &str {
    text.trim_matches(|c: char| c.is_whitespace() || ('\u{1c}'..='\u{1f}').contains(&c))
}

/// A prepared/cached transcription result for one pending event.
///
/// Mirrors the pair Python caches on the event: `_gateway_pending_stt_text`
/// (the enriched text) and `_gateway_pending_stt_transcripts` (the raw
/// successful transcripts). `text` is `Option<String>` to keep Python's
/// `str | None` return faithful, including a prepared null result.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct PreparedStt {
    text: Option<String>,
    transcripts: Vec<String>,
}

/// Transient per-event STT preprocessing state, owned by the runner.
///
/// `prepared` being `None` is Python's "attribute absent" (never prepared);
/// `Some` is a prepared result, even when its cached text is empty. `echoed`
/// is the count of transcripts already delivered to chat, tracked as a COUNT
/// rather than a boolean so a merged second note re-echoes only its new tail
/// while two identically-transcribed notes still both echo (see
/// [`echo_pending_stt_transcripts_once`]).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PendingStt {
    prepared: Option<PreparedStt>,
    echoed: usize,
}

impl PendingStt {
    /// Fresh, unprepared state.
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether a transcription has been prepared/cached (Python's
    /// `hasattr(event, "_gateway_pending_stt_text")`).
    pub fn is_prepared(&self) -> bool {
        self.prepared.is_some()
    }

    /// Count of transcripts already echoed to chat.
    pub fn echoed_count(&self) -> usize {
        self.echoed
    }

    /// Drop only the derived transcription cache, keeping the echo ledger.
    ///
    /// Faithful to `_invalidate_pending_stt_cache`: after new media is merged
    /// into a pending event the stale transcript must be discarded so the next
    /// transcription picks up the merged attachments, but the echo count must
    /// survive so already-delivered transcripts are not echoed again.
    pub fn invalidate(&mut self) {
        self.prepared = None;
    }
}

/// STT-eligible local paths from a pending voice message, in attachment order.
///
/// Mirrors `_pending_event_audio_paths`: iterate `media_urls` and keep every
/// slot that passes [`event_media_is_stt_input`]. Note the filter is STT-input
/// only; it deliberately does NOT also require `event_media_is_audio`.
pub fn pending_event_audio_paths(event: &MessageEvent) -> Vec<String> {
    event
        .media_urls
        .iter()
        .enumerate()
        .filter(|(i, _)| event_media_is_stt_input(event, *i))
        .map(|(_, path)| path.clone())
        .collect()
}

/// Transcribe a pending audio event once and cache the result in `state`.
///
/// Faithful to `_transcribe_pending_audio_event_once`:
///   * A prepared cache short-circuits and is returned verbatim; later
///     `user_text` is ignored once a result is cached.
///   * No STT-eligible audio returns the caller's text without caching:
///     `Some(user_text)` (even empty) overrides the event text, otherwise the
///     event's own text (or `None` when that text is empty).
///   * Otherwise `transcribe(text, audio_paths)` runs once; `text` is the
///     caller-provided text when given (empty overriding the event), else the
///     event text. The `(enriched_text, transcripts)` it returns is cached and
///     returned. On a callback error nothing is cached, so a later call retries.
///
/// `transcribe` stands in for `_enrich_message_with_transcription`.
pub async fn transcribe_pending_audio_event_once<F, Fut>(
    state: &mut PendingStt,
    event: &MessageEvent,
    user_text: Option<&str>,
    transcribe: F,
) -> Result<(Option<String>, Vec<String>)>
where
    F: FnOnce(String, Vec<String>) -> Fut,
    Fut: Future<Output = Result<(Option<String>, Vec<String>)>>,
{
    if let Some(prepared) = &state.prepared {
        return Ok((prepared.text.clone(), prepared.transcripts.clone()));
    }

    let audio_paths = pending_event_audio_paths(event);
    if audio_paths.is_empty() {
        // No audio does not cache.
        let text = match user_text {
            Some(t) => Some(t.to_string()),
            None if event.text.is_empty() => None,
            None => Some(event.text.clone()),
        };
        return Ok((text, Vec::new()));
    }

    let text = match user_text {
        Some(t) => t.to_string(),
        None => event.text.clone(),
    };
    let (enriched_text, successful_transcripts) = transcribe(text, audio_paths).await?;
    state.prepared = Some(PreparedStt {
        text: enriched_text.clone(),
        transcripts: successful_transcripts.clone(),
    });
    Ok((enriched_text, successful_transcripts))
}

/// Echo pending-event STT transcripts to chat at most once each.
///
/// Faithful to `_echo_pending_stt_transcripts_once`. `echo_enabled` mirrors
/// `_should_echo_stt_transcripts()` and `adapter_present` mirrors the
/// `adapter is None` guard; both, along with an empty `transcripts`, short-
/// circuit before the echo ledger is touched (so a disabled echo and a missing
/// adapter are equivalent no-ops). Otherwise the already-echoed count selects
/// the unsent tail, the count advances by that tail's length BEFORE any send,
/// and each unsent transcript is sent as `🎙️ "<transcript>"`. A failed send is
/// non-fatal and does not roll back the count.
///
/// `send` stands in for `adapter.send(source.chat_id, ..., metadata=...)`; it
/// receives the fully formatted line.
pub async fn echo_pending_stt_transcripts_once<F, Fut>(
    state: &mut PendingStt,
    transcripts: &[String],
    echo_enabled: bool,
    adapter_present: bool,
    send: F,
) -> Result<()>
where
    F: Fn(String) -> Fut,
    Fut: Future<Output = Result<()>>,
{
    if transcripts.is_empty() || !echo_enabled || !adapter_present {
        return Ok(());
    }
    let already_echoed = state.echoed;
    let unsent = transcripts.get(already_echoed..).unwrap_or(&[]);
    // Count advances before the sends: a partway failure must not re-echo.
    state.echoed = already_echoed + unsent.len();
    for tx in unsent {
        // A failed echo is non-fatal (Python logs at debug and continues).
        if let Err(error) = send(format!("🎙️ \"{tx}\"")).await {
            tracing::debug!(%error, "Transcript echo failed (non-fatal)");
        }
    }
    Ok(())
}

/// Raw text or successful voice transcripts for a clarify reply.
///
/// Faithful to `_prepare_clarify_reply_text`: with no STT-eligible audio,
/// return the event's trimmed text. Otherwise transcribe once (with an empty
/// caller text, so the event text does not leak in) and join the successful
/// transcripts, each trimmed and dropped when empty, with blank lines.
pub async fn prepare_clarify_reply_text<F, Fut>(
    state: &mut PendingStt,
    event: &MessageEvent,
    transcribe: F,
) -> Result<String>
where
    F: FnOnce(String, Vec<String>) -> Fut,
    Fut: Future<Output = Result<(Option<String>, Vec<String>)>>,
{
    if pending_event_audio_paths(event).is_empty() {
        return Ok(trim_python_whitespace(&event.text).to_string());
    }
    let (_, successful_transcripts) =
        transcribe_pending_audio_event_once(state, event, Some(""), transcribe).await?;
    let joined = successful_transcripts
        .iter()
        .map(|t| trim_python_whitespace(t))
        .filter(|t| !t.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n");
    Ok(joined)
}

/// Compose pending transcription and echo for interrupt/queue-drain callers.
/// The send factory resolves routing/metadata after transcription, as Python's
/// _transcribe_and_echo_pending_voice does. Failures preserve the caller text;
/// a successfully prepared cache remains available even if routing fails.
pub async fn transcribe_and_echo_pending_voice<T, TF, P, S, SF>(
    state: &mut PendingStt,
    event: &MessageEvent,
    text: &str,
    echo_enabled: bool,
    adapter_present: bool,
    transcribe: T,
    prepare_send: P,
) -> (String, Vec<String>)
where
    T: FnOnce(String, Vec<String>) -> TF,
    TF: Future<Output = Result<(Option<String>, Vec<String>)>>,
    P: FnOnce() -> Result<S>,
    S: Fn(String) -> SF,
    SF: Future<Output = Result<()>>,
{
    if pending_event_audio_paths(event).is_empty() {
        return (text.to_owned(), Vec::new());
    }
    let result: Result<_> = async {
        let (enriched, transcripts) =
            transcribe_pending_audio_event_once(state, event, Some(text), transcribe).await?;
        let send = prepare_send()?;
        echo_pending_stt_transcripts_once(state, &transcripts, echo_enabled, adapter_present, send)
            .await?;
        Ok((
            enriched
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| text.to_owned()),
            transcripts,
        ))
    }
    .await;
    match result {
        Ok(output) => output,
        Err(error) => {
            tracing::warn!(%error, "Pending voice transcription failed");
            (text.to_owned(), Vec::new())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform_base_types::MessageType;
    use std::cell::Cell;

    /// A VOICE event carrying one STT-eligible clip and the given caption text.
    fn voice_event(text: &str) -> MessageEvent {
        MessageEvent {
            message_type: MessageType::Voice,
            media_urls: vec!["vn.ogg".to_string()],
            media_types: vec![String::new()],
            ..MessageEvent::new(text)
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

    #[test]
    fn pending_event_audio_paths_filters_stt_input_only() {
        // The pending path selects both: VOICE passes STT even with a PDF MIME.
        let e = MessageEvent {
            message_type: MessageType::Voice,
            media_urls: vec!["vn.ogg".to_string(), "doc.pdf".to_string()],
            media_types: vec![String::new(), "application/pdf".to_string()],
            ..MessageEvent::new("")
        };
        assert_eq!(
            pending_event_audio_paths(&e),
            vec!["vn.ogg".to_string(), "doc.pdf".to_string()]
        );
    }

    #[test]
    fn transcribe_reuses_cache_and_runs_once() {
        let calls = Cell::new(0);
        let mut state = PendingStt::new();
        let e = voice_event("");

        let first = block_on(transcribe_pending_audio_event_once(
            &mut state,
            &e,
            None,
            |text, paths| {
                calls.set(calls.get() + 1);
                async move {
                    assert_eq!(text, "");
                    assert_eq!(paths, vec!["vn.ogg".to_string()]);
                    Ok((Some("enriched".to_string()), vec!["hello".to_string()]))
                }
            },
        ))
        .unwrap();
        assert_eq!(
            first,
            (Some("enriched".to_string()), vec!["hello".to_string()])
        );
        assert!(state.is_prepared());
        assert_eq!(calls.get(), 1);

        // Second call: cache short-circuits, later caller text is ignored, no
        // new transcription runs.
        let second = block_on(transcribe_pending_audio_event_once(
            &mut state,
            &e,
            Some("ignored"),
            |_text, _paths| {
                calls.set(calls.get() + 1);
                async move { Ok((Some("SHOULD NOT RUN".to_string()), vec![])) }
            },
        ))
        .unwrap();
        assert_eq!(
            second,
            (Some("enriched".to_string()), vec!["hello".to_string()])
        );
        assert_eq!(calls.get(), 1);
    }

    #[test]
    fn transcribe_error_does_not_cache_and_retries() {
        let calls = Cell::new(0);
        let mut state = PendingStt::new();
        let e = voice_event("");

        let err = block_on(transcribe_pending_audio_event_once(
            &mut state,
            &e,
            None,
            |_text, _paths| {
                calls.set(calls.get() + 1);
                async move { Err(anyhow::anyhow!("stt down")) }
            },
        ));
        assert!(err.is_err());
        assert!(!state.is_prepared());
        assert_eq!(calls.get(), 1);

        // Retry succeeds and now caches.
        let ok = block_on(transcribe_pending_audio_event_once(
            &mut state,
            &e,
            None,
            |_text, _paths| {
                calls.set(calls.get() + 1);
                async move { Ok((Some("enriched".to_string()), vec!["hi".to_string()])) }
            },
        ))
        .unwrap();
        assert_eq!(ok, (Some("enriched".to_string()), vec!["hi".to_string()]));
        assert!(state.is_prepared());
        assert_eq!(calls.get(), 2);
    }

    #[test]
    fn no_audio_returns_text_without_caching() {
        let mut state = PendingStt::new();
        let e = text_event("just text");

        // user_text None -> event text (non-empty) returned, nothing cached.
        let out = block_on(transcribe_pending_audio_event_once(
            &mut state,
            &e,
            None,
            |_t, _p| async move { Ok((Some("x".to_string()), vec![])) },
        ))
        .unwrap();
        assert_eq!(out, (Some("just text".to_string()), Vec::new()));
        assert!(!state.is_prepared());
    }

    #[test]
    fn no_audio_empty_text_returns_none() {
        let mut state = PendingStt::new();
        let e = text_event("");

        // user_text None + empty event text -> None (Python `event.text or None`).
        let out = block_on(transcribe_pending_audio_event_once(
            &mut state,
            &e,
            None,
            |_t, _p| async move { Ok((Some("x".to_string()), vec![])) },
        ))
        .unwrap();
        assert_eq!(out, (None, Vec::new()));

        // user_text Some("") overrides: empty string returned, not None.
        let out2 = block_on(transcribe_pending_audio_event_once(
            &mut state,
            &e,
            Some(""),
            |_t, _p| async move { Ok((Some("x".to_string()), vec![])) },
        ))
        .unwrap();
        assert_eq!(out2, (Some(String::new()), Vec::new()));
    }

    #[test]
    fn empty_caller_text_overrides_event_text_for_transcription() {
        let seen_text = Cell::new(String::from("unset"));
        let mut state = PendingStt::new();
        let e = voice_event("caption from event");

        block_on(transcribe_pending_audio_event_once(
            &mut state,
            &e,
            Some(""),
            |text, _paths| {
                seen_text.set(text);
                async move { Ok((Some("enriched".to_string()), vec![])) }
            },
        ))
        .unwrap();
        // Caller "" overrides the event's caption when building STT input text.
        assert_eq!(seen_text.take(), "");
    }

    #[test]
    fn echo_sends_unsent_and_advances_count() {
        let sent: std::cell::RefCell<Vec<String>> = std::cell::RefCell::new(Vec::new());
        let mut state = PendingStt::new();
        let transcripts = vec!["hello".to_string()];

        block_on(echo_pending_stt_transcripts_once(
            &mut state,
            &transcripts,
            true,
            true,
            |line| {
                sent.borrow_mut().push(line);
                async move { Ok(()) }
            },
        ))
        .unwrap();
        assert_eq!(state.echoed_count(), 1);
        assert_eq!(*sent.borrow(), vec!["🎙️ \"hello\"".to_string()]);
    }

    #[test]
    fn echo_duplicate_transcripts_are_both_sent() {
        // Two separate notes transcribing identically are two distinct
        // deliveries and both must echo.
        let count = Cell::new(0);
        let mut state = PendingStt::new();
        let transcripts = vec!["same".to_string(), "same".to_string()];

        block_on(echo_pending_stt_transcripts_once(
            &mut state,
            &transcripts,
            true,
            true,
            |_line| {
                count.set(count.get() + 1);
                async move { Ok(()) }
            },
        ))
        .unwrap();
        assert_eq!(count.get(), 2);
        assert_eq!(state.echoed_count(), 2);
    }

    #[test]
    fn echo_after_invalidate_only_sends_new_tail() {
        let count = Cell::new(0);
        let mut state = PendingStt::new();

        // First note echoed.
        block_on(echo_pending_stt_transcripts_once(
            &mut state,
            &["first".to_string()],
            true,
            true,
            |_l| {
                count.set(count.get() + 1);
                async move { Ok(()) }
            },
        ))
        .unwrap();
        assert_eq!(state.echoed_count(), 1);

        // A merge invalidates the derived cache but keeps the echo ledger.
        state.prepared = Some(PreparedStt::default());
        state.invalidate();
        assert!(!state.is_prepared());
        assert_eq!(state.echoed_count(), 1);

        // Re-run transcription returns the earlier note as a prefix of the new
        // list; only the new tail echoes.
        block_on(echo_pending_stt_transcripts_once(
            &mut state,
            &["first".to_string(), "second".to_string()],
            true,
            true,
            |_l| {
                count.set(count.get() + 1);
                async move { Ok(()) }
            },
        ))
        .unwrap();
        assert_eq!(count.get(), 2);
        assert_eq!(state.echoed_count(), 2);
    }

    #[test]
    fn echo_failure_is_nonfatal_and_count_still_advances() {
        let mut state = PendingStt::new();
        let transcripts = vec!["a".to_string(), "b".to_string()];

        let res = block_on(echo_pending_stt_transcripts_once(
            &mut state,
            &transcripts,
            true,
            true,
            |_line| async move { Err(anyhow::anyhow!("send failed")) },
        ));
        assert!(res.is_ok());
        // Count advanced before (and despite) the failed sends.
        assert_eq!(state.echoed_count(), 2);
    }

    #[test]
    fn echo_disabled_and_missing_adapter_are_equivalent_noops() {
        let transcripts = vec!["x".to_string()];

        // Echo disabled.
        let disabled_count = Cell::new(0);
        let mut disabled = PendingStt::new();
        block_on(echo_pending_stt_transcripts_once(
            &mut disabled,
            &transcripts,
            false,
            true,
            |_l| {
                disabled_count.set(disabled_count.get() + 1);
                async move { Ok(()) }
            },
        ))
        .unwrap();

        // Missing adapter.
        let missing_count = Cell::new(0);
        let mut missing = PendingStt::new();
        block_on(echo_pending_stt_transcripts_once(
            &mut missing,
            &transcripts,
            true,
            false,
            |_l| {
                missing_count.set(missing_count.get() + 1);
                async move { Ok(()) }
            },
        ))
        .unwrap();

        assert_eq!(disabled_count.get(), 0);
        assert_eq!(missing_count.get(), 0);
        assert_eq!(disabled, missing);
        assert_eq!(disabled.echoed_count(), 0);
    }

    #[test]
    fn clarify_no_audio_returns_trimmed_text() {
        let mut state = PendingStt::new();
        let e = text_event("  hello there  ");
        let out = block_on(prepare_clarify_reply_text(
            &mut state,
            &e,
            |_t, _p| async move { Ok((Some("unused".to_string()), vec![])) },
        ))
        .unwrap();
        assert_eq!(out, "hello there");
    }

    #[test]
    fn clarify_audio_joins_trimmed_nonempty_transcripts() {
        let mut state = PendingStt::new();
        let e = voice_event("event caption ignored");
        let out = block_on(prepare_clarify_reply_text(&mut state, &e, |text, _p| {
            // Clarify passes an empty caller text.
            assert_eq!(text, "");
            async move {
                Ok((
                    Some("enriched".to_string()),
                    vec![
                        "  one  ".to_string(),
                        "   ".to_string(), // trims to empty -> dropped
                        "two".to_string(),
                    ],
                ))
            }
        }))
        .unwrap();
        assert_eq!(out, "one\n\ntwo");
    }
}

// ---------------------------------------------------------------------------
// Python differential coverage
// ---------------------------------------------------------------------------
// Replay Python's cache/echo transition traces through the real Rust APIs.
#[cfg(test)]
mod golden_corpus {
    use super::*;
    use crate::platform_base_types::{MessageEvent, MessageType};
    use serde_json::{json, Value};
    use std::cell::RefCell;

    #[tokio::test]
    async fn pending_voice_transitions_match_python() {
        let cases: Value =
            serde_json::from_str(include_str!("../../../tools/pending-stt-goldens.json")).unwrap();
        for case in cases.as_array().unwrap() {
            let mut state = PendingStt::new();
            let mut event = MessageEvent {
                text: case["text"].as_str().unwrap().into(),
                message_type: MessageType::from_value(case["kind"].as_str().unwrap()).unwrap(),
                media_urls: serde_json::from_value(case["paths"].clone()).unwrap(),
                media_types: serde_json::from_value(case["mimes"].clone()).unwrap(),
                ..Default::default()
            };
            let calls = RefCell::new(Vec::<Value>::new());
            let sends = RefCell::new(Vec::<String>::new());
            for (index, step) in case["steps"].as_array().unwrap().iter().enumerate() {
                let transcribe = |text: String, paths: Vec<String>| {
                    calls
                        .borrow_mut()
                        .push(json!({"text": text, "paths": paths}));
                    async move {
                        if step["fail"].as_bool().unwrap_or(false) {
                            anyhow::bail!("transcription failed");
                        }
                        let transcripts: Vec<String> =
                            serde_json::from_value(step["transcripts"].clone()).unwrap();
                        Ok((step["text"].as_str().map(str::to_owned), transcripts))
                    }
                };
                let result = match step["op"].as_str().unwrap() {
                    "combined" => json!(
                        transcribe_and_echo_pending_voice(
                            &mut state,
                            &event,
                            step["user_text"].as_str().unwrap(),
                            step["enabled"].as_bool().unwrap_or(true),
                            step["available"].as_bool().unwrap_or(true),
                            transcribe,
                            || {
                                if step["routing_fail"].as_bool().unwrap_or(false) {
                                    anyhow::bail!("routing failed");
                                }
                                Ok(|text| {
                                    sends.borrow_mut().push(text);
                                    async move {
                                        if step["fail"].as_bool().unwrap_or(false) {
                                            anyhow::bail!("send failed");
                                        }
                                        Ok(())
                                    }
                                })
                            },
                        )
                        .await
                    ),
                    "transcribe" => match transcribe_pending_audio_event_once(
                        &mut state,
                        &event,
                        step["user_text"].as_str(),
                        transcribe,
                    )
                    .await
                    {
                        Ok(output) => json!(output),
                        Err(_) => json!({"error": true}),
                    },
                    "clarify" => {
                        match prepare_clarify_reply_text(&mut state, &event, transcribe).await {
                            Ok(output) => json!(output),
                            Err(_) => json!({"error": true}),
                        }
                    }
                    "append" => {
                        event.media_urls.push(step["path"].as_str().unwrap().into());
                        event
                            .media_types
                            .push(step["mime"].as_str().unwrap().into());
                        state.invalidate();
                        Value::Null
                    }
                    "echo" => {
                        let transcripts: Vec<String> =
                            serde_json::from_value(step["transcripts"].clone()).unwrap();
                        echo_pending_stt_transcripts_once(
                            &mut state,
                            &transcripts,
                            step["enabled"].as_bool().unwrap_or(true),
                            step["available"].as_bool().unwrap_or(true),
                            |text| {
                                sends.borrow_mut().push(text);
                                async move {
                                    if step["fail"].as_bool().unwrap_or(false) {
                                        anyhow::bail!("send failed");
                                    }
                                    Ok(())
                                }
                            },
                        )
                        .await
                        .unwrap();
                        Value::Null
                    }
                    op => panic!("unknown operation {op}"),
                };
                let actual =
                    json!({"result": result, "calls": *calls.borrow(), "sends": *sends.borrow()});
                assert_eq!(
                    actual, step["expected"],
                    "scenario {}, step {index}",
                    case["name"]
                );
            }
        }
    }
}
