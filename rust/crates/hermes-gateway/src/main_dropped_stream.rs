pub(crate) const NETWORK_CONTINUATION_PROMPT: &str =
    "[System: The previous response was cut off by a network error mid-stream. Continue exactly where you left off. Do not restart or repeat prior text. Finish the answer directly.]";
pub(crate) const OUTPUT_LIMIT_CONTINUATION_PROMPT: &str =
    "[System: Your previous response was truncated by the output length limit. Continue exactly where you left off. Do not restart or repeat prior text. Finish the answer directly.]";

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) enum StreamEnd {
    #[default]
    CleanEof,
    ProtocolDone,
    TransportError,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Candidate<'a> {
    pub(crate) end: StreamEnd,
    pub(crate) finish_reason: Option<&'a str>,
    pub(crate) visible_text: bool,
    pub(crate) observed_generation: bool,
    pub(crate) saw_usage_object: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Disposition {
    Normal,
    ContinuePartial,
    PropagateTransportError,
}

/// Decide whether an otherwise successful streaming response was cut off.
///
/// At a clean end, a provider finish reason or trailing usage object proves
/// completion. `[DONE]` alone does not: Python still treats visible text
/// without either signal as partial. A transport error wins over those clean
/// end signals and is recoverable only after the provider emitted generation
/// data, because unopened requests remain safe for ordinary retry and fallback.
pub(crate) fn classify(candidate: Candidate<'_>) -> Disposition {
    match candidate.end {
        StreamEnd::TransportError if candidate.observed_generation => Disposition::ContinuePartial,
        StreamEnd::TransportError => Disposition::PropagateTransportError,
        StreamEnd::CleanEof | StreamEnd::ProtocolDone
            if candidate.finish_reason.is_some() || candidate.saw_usage_object =>
        {
            Disposition::Normal
        }
        StreamEnd::CleanEof | StreamEnd::ProtocolDone if candidate.visible_text => {
            Disposition::ContinuePartial
        }
        StreamEnd::CleanEof | StreamEnd::ProtocolDone => Disposition::Normal,
    }
}

#[cfg(test)]
mod tests {
    use super::{
        classify, Candidate, Disposition, StreamEnd, NETWORK_CONTINUATION_PROMPT,
        OUTPUT_LIMIT_CONTINUATION_PROMPT,
    };

    fn corpus() -> serde_json::Value {
        serde_json::from_str(include_str!("../../../tools/dropped-stream-goldens.json")).unwrap()
    }

    fn golden_case<'a>(
        corpus: &'a serde_json::Value,
        section: &str,
        id: &str,
    ) -> &'a serde_json::Value {
        corpus[section]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["case_id"] == id)
            .unwrap_or_else(|| panic!("missing golden case {section}/{id}"))
    }

    #[test]
    fn completion_evidence_proves_clean_stream_end() {
        for end in [StreamEnd::CleanEof, StreamEnd::ProtocolDone] {
            assert_eq!(
                classify(Candidate {
                    end,
                    finish_reason: Some("stop"),
                    visible_text: true,
                    observed_generation: true,
                    saw_usage_object: false,
                }),
                Disposition::Normal
            );
            assert_eq!(
                classify(Candidate {
                    end,
                    finish_reason: None,
                    visible_text: true,
                    observed_generation: true,
                    saw_usage_object: true,
                }),
                Disposition::Normal
            );
        }
    }

    #[test]
    fn visible_unfinished_text_continues_after_eof_or_done() {
        for end in [StreamEnd::CleanEof, StreamEnd::ProtocolDone] {
            assert_eq!(
                classify(Candidate {
                    end,
                    finish_reason: None,
                    visible_text: true,
                    observed_generation: true,
                    saw_usage_object: false,
                }),
                Disposition::ContinuePartial
            );
        }
    }

    #[test]
    fn transport_errors_require_observed_generation_before_continuing() {
        let candidate = |observed_generation| Candidate {
            end: StreamEnd::TransportError,
            finish_reason: None,
            visible_text: observed_generation,
            observed_generation,
            saw_usage_object: false,
        };
        assert_eq!(
            classify(candidate(false)),
            Disposition::PropagateTransportError
        );
        assert_eq!(classify(candidate(true)), Disposition::ContinuePartial);
    }

    #[test]
    fn classifier_matches_source_executed_python_goldens() {
        let corpus = corpus();
        let clean_drop = golden_case(
            &corpus,
            "clean_eof_stream_recovery",
            "clean_eof_text_only_drop",
        );
        assert_eq!(clean_drop["is_stub"], true);
        assert_eq!(
            classify(Candidate {
                end: StreamEnd::CleanEof,
                finish_reason: None,
                visible_text: true,
                observed_generation: true,
                saw_usage_object: false,
            }),
            Disposition::ContinuePartial
        );

        for id in [
            "final_usage_chunk_standard_tokens_is_clean_stop",
            "final_usage_chunk_zero_tokens_is_clean_stop",
            "inline_usage_on_content_chunk_is_clean_stop",
        ] {
            let row = golden_case(&corpus, "usage_object_presence_semantics", id);
            assert_eq!(row["is_stub"], false, "{id}");
            assert_eq!(
                classify(Candidate {
                    end: StreamEnd::CleanEof,
                    finish_reason: None,
                    visible_text: true,
                    observed_generation: true,
                    saw_usage_object: true,
                }),
                Disposition::Normal,
                "{id}"
            );
        }

        for id in [
            "provider_reported_stop_exclusion",
            "provider_reported_length_exclusion",
            "provider_reported_tool_calls_exclusion",
            "provider_reported_content_filter_exclusion",
        ] {
            let row = golden_case(&corpus, "provider_reported_finish_reasons_exclusions", id);
            assert_eq!(row["is_stub"], false, "{id}");
            assert_eq!(
                classify(Candidate {
                    end: StreamEnd::CleanEof,
                    finish_reason: row["raw_finish_reason"].as_str(),
                    visible_text: true,
                    observed_generation: true,
                    saw_usage_object: false,
                }),
                Disposition::Normal,
                "{id}"
            );
        }

        let previsible = golden_case(
            &corpus,
            "transport_error_boundary",
            "transport_error_pre_visible_raises_directly",
        );
        assert_eq!(previsible["expected_stub"], false);
        assert_eq!(
            classify(Candidate {
                end: StreamEnd::TransportError,
                finish_reason: None,
                visible_text: false,
                observed_generation: false,
                saw_usage_object: false,
            }),
            Disposition::PropagateTransportError
        );
        let postvisible = golden_case(
            &corpus,
            "transport_error_boundary",
            "transport_error_post_visible_swallowed_into_stub",
        );
        assert_eq!(postvisible["is_stub"], true);
        assert_eq!(
            classify(Candidate {
                end: StreamEnd::TransportError,
                finish_reason: None,
                visible_text: true,
                observed_generation: true,
                saw_usage_object: false,
            }),
            Disposition::ContinuePartial
        );
    }

    #[test]
    fn prompt_bytes_match_source_executed_python_goldens() {
        let corpus = corpus();
        assert_eq!(
            golden_case(
                &corpus,
                "continuation_prompt_selection",
                "prompt_partial_stub_network_error",
            )["expected_prompt"],
            NETWORK_CONTINUATION_PROMPT
        );
        assert_eq!(
            golden_case(
                &corpus,
                "continuation_prompt_selection",
                "prompt_non_stub_output_limit",
            )["expected_prompt"],
            OUTPUT_LIMIT_CONTINUATION_PROMPT
        );
    }
}
