//! Port of `GatewayRunner._enrich_message_with_transcription` from
//! `gateway/run.py`.
//!
// Public API is ahead of its callers while the runner (GatewayRunner) is ported.
#![allow(dead_code)]
//!
//! This is the STT orchestration that auto-transcribes a user's voice/audio
//! clips and prepends the transcript (or a fallback note) to the message text.
//! The Python method is the specification; every user-facing string and every
//! branch is preserved verbatim.
//!
//! ## What stays out of this file
//!
//! The source reaches four effects the runner owns: `os.path.abspath`, the
//! best-effort `_probe_audio_duration`, the configured/local STT providers
//! (`transcribe_audio` / `transcribe_audio_local_fallback`), and
//! `to_agent_visible_cache_path`. Those are the *only* things this port cannot
//! do without live services, so they are abstracted behind one
//! [`TranscriptionBackend`] trait. The runner will implement it against its real
//! providers; the tests implement it with a deterministic recorder. Nothing here
//! performs IO, spawns threads, or reaches a network.
//!
//! ## Signature note (why not `Option<&dyn ...>` + a lone `stt_enabled`)
//!
//! The disabled path in the source still calls `os.path.abspath` and
//! `_probe_audio_duration` on every clip, so a backend is required *even when
//! STT is disabled* -- an `Option` that goes `None` in the disabled case could
//! not produce the duration notes. And the "transcription module unavailable"
//! branch is a *separate* condition from "STT disabled": Python models it as an
//! import failure (`ModuleNotFoundError`) inside the enabled path. So the faithful
//! surface is an always-present `backend`, plus two independent booleans:
//! `stt_enabled` and `module_available`. See
//! [`enrich_message_with_transcription`].
//!
//! The trait is `Send + Sync` (like [`crate::platform::PlatformAdapter`]) and
//! uses `#[async_trait]`, so the production provider calls yield `Send` futures
//! and the whole orchestration future is `Send`.

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use serde_json::Value;

/// The Discord adapter's empty-content placeholder. When the caption is exactly
/// this (after a Python-style strip) it is dropped once we have real notes,
/// since the notes already say a voice message arrived.
const EMPTY_CONTENT_PLACEHOLDER: &str = "(The user sent a message with no text content)";

/// Sentinel emitted when STT returns success with an empty/whitespace-only
/// transcript (silence, cut-off, inaudible). Emitting bare `""` makes the agent
/// reply to nothing and can loop (#41603), so we say so plainly instead.
///
/// The em dash is written as `\u{2014}` to match the Python source byte-for-byte.
const SILENCE_SENTINEL: &str = "[The user sent a voice message but it came through empty or inaudible \u{2014} speech-to-text returned no words. Do not guess at the content; ask the user to resend or type it out.]";

/// Note used when the transcription module itself is unavailable.
const MODULE_UNAVAILABLE_NOTE: &str = "[voice message could not be transcribed]";

/// Python `str.strip` also treats the four ASCII information separators
/// (U+001C..U+001F) as whitespace, which Rust's `str::trim` does not. Match the
/// broader set so caption dedup/placeholder comparisons line up with Python.
fn trim_python_whitespace(text: &str) -> &str {
    text.trim_matches(|c: char| c.is_whitespace() || ('\u{1c}'..='\u{1f}').contains(&c))
}

/// Python truthiness for a JSON value: `None`/`False`/`0`/`""`/`[]`/`{}` are
/// falsy, everything else truthy. Provider response flags use Python truthiness,
/// not configuration-string coercion (a nonempty string such as "false" is true).
fn python_bool(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().map(|f| f != 0.0).unwrap_or(true),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

/// Truthiness of an optional value; a missing key is falsy (Python
/// `dict.get(key)` returns `None`).
fn python_bool_opt(value: Option<&Value>) -> bool {
    value.map(python_bool).unwrap_or(false)
}

/// Faithful port of Python `mapping.get(key)`: a real dict returns the value or
/// `None`; anything that is *not* a dict raises `AttributeError` (no `.get`),
/// which the source's `try`/`except` turns into the "could not be transcribed"
/// note. We surface that as `Err` so callers route it the same way.
fn py_dict_get<'a>(value: &'a Value, key: &str) -> Result<Option<&'a Value>> {
    match value {
        Value::Object(map) => Ok(map.get(key)),
        _ => Err(anyhow!("provider result is not a mapping (no .get)")),
    }
}

/// Faithful port of Python `mapping[key]`: `KeyError` when the key is absent,
/// `TypeError` when the value is not subscriptable. Both propagate as `Err` and
/// become the exception-path note, exactly as the source's bare `result["..."]`
/// accesses do inside the `try`.
fn py_dict_getitem<'a>(value: &'a Value, key: &str) -> Result<&'a Value> {
    match value {
        Value::Object(map) => map.get(key).ok_or_else(|| anyhow!("KeyError: {key:?}")),
        _ => Err(anyhow!("provider result is not subscriptable")),
    }
}

/// Effects `_enrich_message_with_transcription` invokes on the runner. The
/// concrete implementation lives with the runner (real providers) or, in tests,
/// a deterministic recorder. All provider responses are `serde_json::Value` so
/// missing `success`/`transcript` keys and Python truthiness are modelled
/// exactly.
#[async_trait]
pub trait TranscriptionBackend: Send + Sync {
    /// `os.path.abspath(path)`.
    fn absolute_path(&self, path: &str) -> String;

    /// `await _probe_audio_duration(abs_path)` -- best-effort, never raising;
    /// `None` maps to Python's falsy `None`/`""` (no duration suffix).
    async fn probe_duration(&self, abs_path: &str) -> Option<String>;

    /// `await asyncio.to_thread(transcribe_audio, path, None, "gateway")`. The
    /// returned dict is a `Value`; a raised exception maps to `Err`.
    async fn transcribe(&self, path: &str) -> Result<Value>;

    /// `await asyncio.to_thread(transcribe_audio_local_fallback, path)`. Same
    /// `Value`/`Err` contract as [`Self::transcribe`]; a raised fallback maps to
    /// `Err` (the "fallback failure" boundary).
    async fn local_fallback(&self, path: &str) -> Result<Value>;

    /// `to_agent_visible_cache_path(abs_path)` -- translate a host cache path to
    /// its agent-visible (sandbox-mounted) form.
    fn agent_visible_path(&self, abs_path: &str) -> String;
}

/// Append the caption after a non-empty `prefix`, dropping the empty-content
/// placeholder. Shared by the disabled, module-unavailable, and success paths;
/// all three end with the identical three-way branch in the source.
fn compose_with_caption(prefix: &str, user_text: &str) -> String {
    if !user_text.is_empty() && trim_python_whitespace(user_text) == EMPTY_CONTENT_PLACEHOLDER {
        prefix.to_string()
    } else if !user_text.is_empty() {
        format!("{prefix}\n\n{user_text}")
    } else {
        prefix.to_string()
    }
}

/// What one clip resolved to inside the enabled+available loop.
enum ClipOutcome {
    /// Success with an empty/whitespace transcript -> the silence sentinel.
    Silence,
    /// A real, non-empty transcript string.
    Transcript(String),
    /// `success` was present-but-falsy (the source's `else` branch). Produces
    /// the same agent-path note as any exception.
    Failed,
}

/// Port of `GatewayRunner._enrich_message_with_transcription`.
///
/// Returns `(enriched_text, successful_transcripts)`:
///   * `enriched_text` -- the message with transcription wrappers/notes
///     prepended.
///   * `successful_transcripts` -- raw transcript strings for clips that were
///     transcribed successfully, in input order; empty when every clip failed,
///     was silent, or STT was disabled/unavailable.
///
/// Parameters:
///   * `user_text` -- the caption / message text.
///   * `audio_paths` -- local cached audio paths (deduplicated stably here).
///   * `stt_enabled` -- `getattr(self.config, "stt_enabled", True)`.
///   * `module_available` -- whether `tools.transcription_tools` imported; the
///     source's `ModuleNotFoundError` branch. Ignored when `stt_enabled` is
///     false (the disabled path never imports the module).
///   * `backend` -- the runner's effects (always required; the disabled path
///     still needs `absolute_path` + `probe_duration`).
pub async fn enrich_message_with_transcription(
    user_text: &str,
    audio_paths: &[String],
    stt_enabled: bool,
    module_available: bool,
    backend: &dyn TranscriptionBackend,
) -> Result<(String, Vec<String>)> {
    // seen = set(); [p for p in audio_paths if p not in seen and not seen.add(p)]
    // Stable order-preserving dedup.
    let mut seen = std::collections::HashSet::new();
    let audio_paths: Vec<&String> = audio_paths
        .iter()
        .filter(|p| seen.insert(p.as_str()))
        .collect();

    // --- STT disabled: duration notes only, no transcription. ---------------
    if !stt_enabled {
        let mut notes = Vec::new();
        for &path in &audio_paths {
            let abs_path = backend.absolute_path(path);
            let duration = backend.probe_duration(&abs_path).await;
            // `if duration_str:` -- truthy means Some(non-empty).
            match duration {
                Some(d) if !d.is_empty() => notes.push(format!(
                    "[The user sent a voice message: {abs_path} (duration: {d})]"
                )),
                _ => notes.push(format!("[The user sent a voice message: {abs_path}]")),
            }
        }
        if notes.is_empty() {
            return Ok((user_text.to_string(), Vec::new()));
        }
        let prefix = notes.join("\n\n");
        return Ok((compose_with_caption(&prefix, user_text), Vec::new()));
    }

    // --- STT enabled but the transcription module is unavailable. -----------
    if !module_available {
        return Ok((
            compose_with_caption(MODULE_UNAVAILABLE_NOTE, user_text),
            Vec::new(),
        ));
    }

    // --- STT enabled and available: transcribe each clip. -------------------
    let mut enriched_parts: Vec<String> = Vec::new();
    let mut successful_transcripts: Vec<String> = Vec::new();

    for &path in &audio_paths {
        // The whole per-clip body is the source's `try`; any raised error
        // (provider raise, missing `success`/`transcript` key, `.strip()` on a
        // non-string) becomes the agent-path note, identical to the `else`
        // branch's note.
        let outcome: Result<ClipOutcome> = async {
            let mut result = backend.transcribe(path).await?;

            // if not result.get("success"): try the local fallback.
            let need_fallback = !python_bool_opt(py_dict_get(&result, "success")?);
            if need_fallback {
                let fallback = backend.local_fallback(path).await?;
                // if fallback.get("success"): result = fallback
                if python_bool_opt(py_dict_get(&fallback, "success")?) {
                    result = fallback;
                }
            }

            // if result["success"]: -- getitem, so a missing key raises.
            if python_bool(py_dict_getitem(&result, "success")?) {
                let transcript = py_dict_getitem(&result, "transcript")?;
                // if not (transcript or "").strip():
                // `transcript or ""`: falsy transcript collapses to "" (empty
                // after strip). A truthy non-string would AttributeError on
                // `.strip()` -> route to the failure note.
                let stripped_empty = if python_bool(transcript) {
                    match transcript.as_str() {
                        Some(s) => trim_python_whitespace(s).is_empty(),
                        None => return Err(anyhow!("AttributeError: 'strip' on non-str")),
                    }
                } else {
                    true
                };
                if stripped_empty {
                    return Ok(ClipOutcome::Silence);
                }
                // Truthy and non-empty => a real string transcript.
                let t = transcript
                    .as_str()
                    .expect("checked truthy string above")
                    .to_string();
                Ok(ClipOutcome::Transcript(t))
            } else {
                // else: error = result.get("error", "unknown error"); logged
                // for operators, kept out of the LLM-visible prompt.
                let error = py_dict_get(&result, "error")?
                    .and_then(Value::as_str)
                    .unwrap_or("unknown error");
                tracing::info!(%path, error, "Voice transcription failed");
                Ok(ClipOutcome::Failed)
            }
        }
        .await;

        match outcome {
            Ok(ClipOutcome::Silence) => enriched_parts.push(SILENCE_SENTINEL.to_string()),
            Ok(ClipOutcome::Transcript(t)) => {
                successful_transcripts.push(t.clone());
                // Plain quoted line -- the earlier meta-wording made the model
                // comment on "voice mode" instead of replying to the content.
                enriched_parts.push(format!("\"{t}\""));
            }
            other => {
                // Either the `else` branch (present-but-falsy success) or any
                // raised exception; both produce the identical agent-path note.
                if let Err(error) = &other {
                    tracing::error!(%path, %error, "Transcription error");
                }
                let abs_path = backend.absolute_path(path);
                let agent_path = backend.agent_visible_path(&abs_path);
                enriched_parts.push(format!(
                    "[voice message could not be transcribed automatically; the audio is available at: {agent_path}]"
                ));
            }
        }
    }

    if !enriched_parts.is_empty() {
        let prefix = enriched_parts.join("\n\n");
        return Ok((
            compose_with_caption(&prefix, user_text),
            successful_transcripts,
        ));
    }
    // No audio clips at all -> caption passes through unchanged; transcripts empty.
    Ok((user_text.to_string(), successful_transcripts))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::HashMap;
    use std::future::Future;
    use std::sync::Mutex;

    /// Drive a future to completion on a single-threaded runtime (repo idiom).
    fn block_on<F: Future>(fut: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(fut)
    }

    /// A clonable stand-in for a provider dict result, plus a "raised" variant.
    #[derive(Clone)]
    enum Resp {
        Dict(Value),
        Raise,
    }

    /// Deterministic, `Send + Sync` recorder. Records every effect call in order
    /// and returns scripted responses keyed by path. Uses `Mutex` (not
    /// `RefCell`) so it satisfies the `Send + Sync` trait bound and the provider
    /// futures are `Send`.
    #[derive(Default)]
    struct Recorder {
        calls: Mutex<Vec<String>>,
        durations: Mutex<HashMap<String, Option<String>>>,
        transcribe: Mutex<HashMap<String, Resp>>,
        fallback: Mutex<HashMap<String, Resp>>,
    }

    impl Recorder {
        fn new() -> Self {
            Self::default()
        }

        fn record(&self, entry: impl Into<String>) {
            self.calls.lock().unwrap().push(entry.into());
        }

        fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }

        fn with_duration(self, path: &str, dur: Option<&str>) -> Self {
            self.durations
                .lock()
                .unwrap()
                .insert(path.to_string(), dur.map(str::to_string));
            self
        }

        fn with_transcribe(self, path: &str, resp: Resp) -> Self {
            self.transcribe
                .lock()
                .unwrap()
                .insert(path.to_string(), resp);
            self
        }

        fn with_fallback(self, path: &str, resp: Resp) -> Self {
            self.fallback.lock().unwrap().insert(path.to_string(), resp);
            self
        }
    }

    #[async_trait]
    impl TranscriptionBackend for Recorder {
        fn absolute_path(&self, path: &str) -> String {
            self.record(format!("abspath:{path}"));
            format!("/abs/{path}")
        }

        async fn probe_duration(&self, abs_path: &str) -> Option<String> {
            self.record(format!("probe:{abs_path}"));
            self.durations
                .lock()
                .unwrap()
                .get(abs_path)
                .cloned()
                .flatten()
        }

        async fn transcribe(&self, path: &str) -> Result<Value> {
            self.record(format!("transcribe:{path}"));
            match self.transcribe.lock().unwrap().get(path).cloned() {
                Some(Resp::Dict(v)) => Ok(v),
                Some(Resp::Raise) => Err(anyhow!("transcribe raised for {path}")),
                None => Ok(json!({"success": false, "error": "no provider"})),
            }
        }

        async fn local_fallback(&self, path: &str) -> Result<Value> {
            self.record(format!("fallback:{path}"));
            match self.fallback.lock().unwrap().get(path).cloned() {
                Some(Resp::Dict(v)) => Ok(v),
                Some(Resp::Raise) => Err(anyhow!("fallback raised for {path}")),
                None => Ok(json!({"success": false})),
            }
        }

        fn agent_visible_path(&self, abs_path: &str) -> String {
            self.record(format!("agentvis:{abs_path}"));
            abs_path.replace("/abs/", "/visible/")
        }
    }

    fn paths(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    // ---- disabled path -----------------------------------------------------

    #[test]
    fn disabled_emits_duration_notes_and_dedups_stably() {
        // Duplicate path collapses to one; order preserved; duration suffix only
        // when the probe returns a non-empty string.
        let backend = Recorder::new()
            .with_duration("/abs/a.ogg", Some("00:03"))
            .with_duration("/abs/b.ogg", None);
        let (text, transcripts) = block_on(enrich_message_with_transcription(
            "",
            &paths(&["a.ogg", "b.ogg", "a.ogg"]),
            false,
            true,
            &backend,
        ))
        .unwrap();
        assert_eq!(
            text,
            "[The user sent a voice message: /abs/a.ogg (duration: 00:03)]\n\n\
             [The user sent a voice message: /abs/b.ogg]"
        );
        assert!(transcripts.is_empty());
        // Effect order: two unique clips, each abspath then probe. No transcribe.
        assert_eq!(
            backend.calls(),
            vec![
                "abspath:a.ogg",
                "probe:/abs/a.ogg",
                "abspath:b.ogg",
                "probe:/abs/b.ogg",
            ]
        );
    }

    #[test]
    fn disabled_drops_placeholder_but_keeps_real_caption() {
        let backend = Recorder::new().with_duration("/abs/a.ogg", None);

        // Placeholder caption (with U+001C..U+001F padding) is stripped.
        let (only_note, _) = block_on(enrich_message_with_transcription(
            "\u{1c}(The user sent a message with no text content)\u{1f}",
            &paths(&["a.ogg"]),
            false,
            true,
            &backend,
        ))
        .unwrap();
        assert_eq!(only_note, "[The user sent a voice message: /abs/a.ogg]");

        // Real caption is appended after a blank line.
        let (with_caption, _) = block_on(enrich_message_with_transcription(
            "look at this",
            &paths(&["a.ogg"]),
            false,
            true,
            &backend,
        ))
        .unwrap();
        assert_eq!(
            with_caption,
            "[The user sent a voice message: /abs/a.ogg]\n\nlook at this"
        );
    }

    #[test]
    fn disabled_with_no_audio_returns_caption_untouched() {
        let backend = Recorder::new();
        let (text, transcripts) = block_on(enrich_message_with_transcription(
            "hello",
            &[],
            false,
            true,
            &backend,
        ))
        .unwrap();
        assert_eq!(text, "hello");
        assert!(transcripts.is_empty());
        assert!(backend.calls().is_empty());
    }

    // ---- module unavailable ------------------------------------------------

    #[test]
    fn module_unavailable_emits_single_note_and_placeholder_rules() {
        let backend = Recorder::new();

        let (bare, t1) = block_on(enrich_message_with_transcription(
            "",
            &paths(&["a.ogg"]),
            true,
            false,
            &backend,
        ))
        .unwrap();
        assert_eq!(bare, "[voice message could not be transcribed]");
        assert!(t1.is_empty());

        let (with_caption, _) = block_on(enrich_message_with_transcription(
            "caption",
            &paths(&["a.ogg"]),
            true,
            false,
            &backend,
        ))
        .unwrap();
        assert_eq!(
            with_caption,
            "[voice message could not be transcribed]\n\ncaption"
        );

        let (placeholder, _) = block_on(enrich_message_with_transcription(
            "(The user sent a message with no text content)",
            &paths(&["a.ogg"]),
            true,
            false,
            &backend,
        ))
        .unwrap();
        assert_eq!(placeholder, "[voice message could not be transcribed]");

        // The module-unavailable branch touches no provider effects.
        assert!(backend.calls().is_empty());
    }

    // ---- enabled + available: success, silence, fallback, errors -----------

    #[test]
    fn success_quotes_transcripts_in_order_and_appends_caption() {
        let backend = Recorder::new()
            .with_transcribe(
                "a.ogg",
                Resp::Dict(json!({"success": true, "transcript": "hello"})),
            )
            .with_transcribe(
                "b.ogg",
                Resp::Dict(json!({"success": true, "transcript": "world"})),
            );
        let (text, transcripts) = block_on(enrich_message_with_transcription(
            "cap",
            &paths(&["a.ogg", "b.ogg"]),
            true,
            true,
            &backend,
        ))
        .unwrap();
        assert_eq!(text, "\"hello\"\n\n\"world\"\n\ncap");
        assert_eq!(transcripts, vec!["hello".to_string(), "world".to_string()]);
        // Configured success on both => no fallback calls.
        assert_eq!(
            backend.calls(),
            vec!["transcribe:a.ogg", "transcribe:b.ogg"]
        );
    }

    #[test]
    fn success_placeholder_caption_is_dropped() {
        let backend = Recorder::new().with_transcribe(
            "a.ogg",
            Resp::Dict(json!({"success": true, "transcript": "hi"})),
        );
        let (text, transcripts) = block_on(enrich_message_with_transcription(
            "(The user sent a message with no text content)",
            &paths(&["a.ogg"]),
            true,
            true,
            &backend,
        ))
        .unwrap();
        assert_eq!(text, "\"hi\"");
        assert_eq!(transcripts, vec!["hi".to_string()]);
    }

    #[test]
    fn empty_and_whitespace_transcripts_hit_the_silence_sentinel() {
        // success=True with "" and with whitespace-only both -> sentinel, and
        // neither counts as a successful transcript.
        for transcript in ["", "   \u{1d}\t"] {
            let backend = Recorder::new().with_transcribe(
                "a.ogg",
                Resp::Dict(json!({"success": true, "transcript": transcript})),
            );
            let (text, transcripts) = block_on(enrich_message_with_transcription(
                "",
                &paths(&["a.ogg"]),
                true,
                true,
                &backend,
            ))
            .unwrap();
            assert_eq!(text, SILENCE_SENTINEL);
            assert!(transcripts.is_empty());
        }
    }

    #[test]
    fn configured_failure_recovers_via_local_fallback() {
        let backend = Recorder::new()
            .with_transcribe(
                "a.ogg",
                Resp::Dict(json!({"success": false, "error": "nope"})),
            )
            .with_fallback(
                "a.ogg",
                Resp::Dict(json!({"success": true, "transcript": "local"})),
            );
        let (text, transcripts) = block_on(enrich_message_with_transcription(
            "",
            &paths(&["a.ogg"]),
            true,
            true,
            &backend,
        ))
        .unwrap();
        assert_eq!(text, "\"local\"");
        assert_eq!(transcripts, vec!["local".to_string()]);
        // Configured failed -> fallback consulted, order preserved.
        assert_eq!(backend.calls(), vec!["transcribe:a.ogg", "fallback:a.ogg"]);
    }

    #[test]
    fn both_providers_failing_emits_agent_visible_note() {
        // Configured falsy + fallback falsy: result["success"] is present-and-
        // false -> else branch -> agent-visible path note.
        let backend = Recorder::new()
            .with_transcribe("a.ogg", Resp::Dict(json!({"success": false, "error": "x"})))
            .with_fallback("a.ogg", Resp::Dict(json!({"success": false})));
        let (text, transcripts) = block_on(enrich_message_with_transcription(
            "",
            &paths(&["a.ogg"]),
            true,
            true,
            &backend,
        ))
        .unwrap();
        assert_eq!(
            text,
            "[voice message could not be transcribed automatically; the audio is available at: /visible/a.ogg]"
        );
        assert!(transcripts.is_empty());
        assert_eq!(
            backend.calls(),
            vec![
                "transcribe:a.ogg",
                "fallback:a.ogg",
                "abspath:a.ogg",
                "agentvis:/abs/a.ogg",
            ]
        );
    }

    #[test]
    fn provider_raise_and_missing_keys_all_route_to_the_note() {
        // Three distinct exception boundaries, one identical note each:
        //  1. transcribe raises,
        //  2. success key missing + fallback also missing key (getitem KeyError),
        //  3. transcript key missing on a success result (getitem KeyError).
        let note = |p: &str| {
            format!(
                "[voice message could not be transcribed automatically; the audio is available at: /visible/{p}]"
            )
        };

        // 1. transcribe raises.
        let b1 = Recorder::new().with_transcribe("a.ogg", Resp::Raise);
        let (t1, tr1) = block_on(enrich_message_with_transcription(
            "",
            &paths(&["a.ogg"]),
            true,
            true,
            &b1,
        ))
        .unwrap();
        assert_eq!(t1, note("a.ogg"));
        assert!(tr1.is_empty());

        // 2. success key absent everywhere: `.get` is falsy -> fallback; then
        //    result["success"] getitem raises KeyError -> note.
        let b2 = Recorder::new()
            .with_transcribe("a.ogg", Resp::Dict(json!({"nope": 1})))
            .with_fallback("a.ogg", Resp::Dict(json!({"still": "no"})));
        let (t2, _) = block_on(enrich_message_with_transcription(
            "",
            &paths(&["a.ogg"]),
            true,
            true,
            &b2,
        ))
        .unwrap();
        assert_eq!(t2, note("a.ogg"));

        // 3. success true but transcript key missing -> getitem KeyError -> note.
        let b3 = Recorder::new().with_transcribe("a.ogg", Resp::Dict(json!({"success": true})));
        let (t3, _) = block_on(enrich_message_with_transcription(
            "",
            &paths(&["a.ogg"]),
            true,
            true,
            &b3,
        ))
        .unwrap();
        assert_eq!(t3, note("a.ogg"));
    }

    #[test]
    fn mixed_clips_preserve_order_of_notes_and_successful_transcripts() {
        // silence, success, failure interleaved: enriched parts keep clip order,
        // but successful_transcripts holds only the real transcript.
        let backend = Recorder::new()
            .with_transcribe(
                "s.ogg",
                Resp::Dict(json!({"success": true, "transcript": "  "})),
            )
            .with_transcribe(
                "ok.ogg",
                Resp::Dict(json!({"success": true, "transcript": "yes"})),
            )
            .with_transcribe("bad.ogg", Resp::Dict(json!({"success": false})))
            .with_fallback("bad.ogg", Resp::Dict(json!({"success": false})));
        let (text, transcripts) = block_on(enrich_message_with_transcription(
            "cap",
            &paths(&["s.ogg", "ok.ogg", "bad.ogg"]),
            true,
            true,
            &backend,
        ))
        .unwrap();
        assert_eq!(
            text,
            format!(
                "{SILENCE_SENTINEL}\n\n\"yes\"\n\n[voice message could not be transcribed automatically; the audio is available at: /visible/bad.ogg]\n\ncap"
            )
        );
        assert_eq!(transcripts, vec!["yes".to_string()]);
    }

    #[test]
    fn fallback_raising_is_caught_and_noted() {
        // The "fallback failure" boundary: configured falsy, fallback raises.
        let backend = Recorder::new()
            .with_transcribe("a.ogg", Resp::Dict(json!({"success": false})))
            .with_fallback("a.ogg", Resp::Raise);
        let (text, transcripts) = block_on(enrich_message_with_transcription(
            "",
            &paths(&["a.ogg"]),
            true,
            true,
            &backend,
        ))
        .unwrap();
        assert_eq!(
            text,
            "[voice message could not be transcribed automatically; the audio is available at: /visible/a.ogg]"
        );
        assert!(transcripts.is_empty());
    }

    #[test]
    fn truthy_non_string_transcript_routes_to_note() {
        // success true, transcript is a number: `(x or "").strip()` would
        // AttributeError in Python -> exception note.
        let backend = Recorder::new().with_transcribe(
            "a.ogg",
            Resp::Dict(json!({"success": true, "transcript": 5})),
        );
        let (text, transcripts) = block_on(enrich_message_with_transcription(
            "",
            &paths(&["a.ogg"]),
            true,
            true,
            &backend,
        ))
        .unwrap();
        assert_eq!(
            text,
            "[voice message could not be transcribed automatically; the audio is available at: /visible/a.ogg]"
        );
        assert!(transcripts.is_empty());
    }
}

// ---------------------------------------------------------------------------
// Python differential coverage
// ---------------------------------------------------------------------------
// Replay Python's orchestration against a recording provider boundary.
#[cfg(test)]
mod golden_corpus {
    use crate::transcription_enrichment::{
        enrich_message_with_transcription, TranscriptionBackend,
    };
    use anyhow::Result;
    use async_trait::async_trait;
    use serde_json::{json, Value};
    use std::sync::Mutex;

    struct Backend<'a> {
        case: &'a Value,
        calls: Mutex<Vec<Value>>,
    }
    impl Backend<'_> {
        fn record(&self, kind: &str, path: &str) {
            self.calls.lock().unwrap().push(json!([kind, path]));
        }
        fn response(&self, kind: &str, path: &str) -> Result<Value> {
            self.record(kind, path);
            if path == "b" {
                return Ok(json!({"success": true, "transcript": "second"}));
            }
            let value = &self.case[kind];
            if value.get("raise").and_then(Value::as_bool).unwrap_or(false) {
                anyhow::bail!("provider failed");
            }
            Ok(value.clone())
        }
    }
    #[async_trait]
    impl TranscriptionBackend for Backend<'_> {
        fn absolute_path(&self, path: &str) -> String {
            self.record("absolute", path);
            format!("/fixture/{path}")
        }
        async fn probe_duration(&self, path: &str) -> Option<String> {
            self.record("duration", path);
            Some(if path.ends_with('a') { "0:12" } else { "" }.into())
        }
        async fn transcribe(&self, path: &str) -> Result<Value> {
            self.response("primary", path)
        }
        async fn local_fallback(&self, path: &str) -> Result<Value> {
            self.response("fallback", path)
        }
        fn agent_visible_path(&self, path: &str) -> String {
            self.record("visible", path);
            format!("/agent{path}")
        }
    }

    #[tokio::test]
    async fn transcription_call_order_and_outputs_match_python() {
        let cases: Value =
            serde_json::from_str(include_str!("../../../tools/transcription-goldens.json"))
                .unwrap();
        for (index, case) in cases.as_array().unwrap().iter().enumerate() {
            let backend = Backend {
                case,
                calls: Mutex::new(Vec::new()),
            };
            let paths: Vec<String> = serde_json::from_value(case["paths"].clone()).unwrap();
            let output = enrich_message_with_transcription(
                case["caption"].as_str().unwrap(),
                &paths,
                case["enabled"].as_bool().unwrap(),
                case["available"].as_bool().unwrap(),
                &backend,
            )
            .await
            .unwrap();
            assert_eq!(
                json!({"output": output, "calls": *backend.calls.lock().unwrap()}),
                case["expected"],
                "Python transcription case {index}"
            );
        }
    }
}
