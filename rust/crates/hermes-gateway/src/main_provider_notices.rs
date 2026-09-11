//! Turn-local presentation notices for ordinary main-provider recovery.

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Notice {
    pub kind: &'static str,
    pub text: String,
}

#[derive(Debug, Default)]
pub struct TurnNotices {
    buffered: Vec<Notice>,
    pending_durable: Vec<Notice>,
}

impl TurnNotices {
    pub fn record_fallback(
        &mut self,
        old_model: &str,
        old_provider: &str,
        new_model: &str,
        new_provider: &str,
        reason: &str,
    ) {
        let notice = Notice {
            kind: "fallback_switch",
            text: format!(
                "⚠️ Model fallback: {old_model} via {old_provider} unavailable ({reason}); using {new_model} via {new_provider}."
            ),
        };
        self.buffered.push(notice.clone());
        self.pending_durable.push(notice);
    }

    pub fn record_primary_restore(
        &mut self,
        primary_model: &str,
        primary_provider: &str,
        fallback_model: &str,
        fallback_provider: &str,
    ) {
        let notice = Notice {
            kind: "primary_restore",
            text: format!(
                "✅ Primary model restored: {primary_model} via {primary_provider}; fallback {fallback_model} via {fallback_provider} is no longer active."
            ),
        };
        self.buffered.push(notice.clone());
        self.pending_durable.push(notice);
    }

    /// Recovery drops transient chatter but preserves each fallback switch.
    /// Terminal failure emits the buffered switch trace without duplicating
    /// the pending fallback copy. Both vectors contain the same durable
    /// notices until transient retry trace records are added to `buffered`.
    pub fn drain(&mut self, recovered: bool) -> Vec<Notice> {
        if recovered {
            self.buffered.clear();
            std::mem::take(&mut self.pending_durable)
        } else {
            self.pending_durable.clear();
            std::mem::take(&mut self.buffered)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recovery_keeps_ordered_switches_and_drops_the_terminal_copies() {
        let mut notices = TurnNotices::default();
        notices.record_fallback("m1", "p1", "m2", "p2", "rate limit");
        notices.record_fallback("m2", "p2", "m3", "p3", "request timeout");

        let emitted = notices.drain(true);

        assert_eq!(emitted.len(), 2);
        assert!(emitted[0].text.contains("m1 via p1"));
        assert!(emitted[1].text.contains("m2 via p2"));
        assert!(notices.drain(true).is_empty());
        assert!(notices.drain(false).is_empty());
    }

    #[test]
    fn terminal_failure_flushes_each_switch_once() {
        let mut notices = TurnNotices::default();
        notices.record_fallback("m1", "p1", "m2", "p2", "provider overloaded");

        assert_eq!(notices.drain(false).len(), 1);
        assert!(notices.drain(false).is_empty());
        assert!(notices.drain(true).is_empty());
    }

    #[test]
    fn primary_restore_is_durable_on_recovery_or_terminal_failure() {
        for recovered in [true, false] {
            let mut notices = TurnNotices::default();
            notices.record_primary_restore("m1", "p1", "m2", "p2");

            let emitted = notices.drain(recovered);

            assert_eq!(emitted.len(), 1);
            assert_eq!(emitted[0].kind, "primary_restore");
            assert_eq!(
                emitted[0].text,
                "✅ Primary model restored: m1 via p1; fallback m2 via p2 is no longer active."
            );
            assert!(notices.drain(recovered).is_empty());
        }
    }

    #[test]
    fn fallback_text_and_recovered_order_match_the_python_corpus() {
        let corpus: serde_json::Value = serde_json::from_str(include_str!(
            "../../../tools/main-provider-operator-notice-goldens.json"
        ))
        .unwrap();
        assert_eq!(corpus["contract_metadata"]["total_cases"], 37);
        let rows = corpus["multiple_switches"].as_array().unwrap();
        let expected = rows
            .iter()
            .find(|row| row["case_name"] == "two_switches_recovered_on_second_fallback")
            .unwrap()["pending_notices_before_emit"]
            .as_array()
            .unwrap();
        let mut notices = TurnNotices::default();
        notices.record_fallback("primary-m", "primary-p", "fb1-m", "fb1-p", "rate limit");
        notices.record_fallback("fb1-m", "fb1-p", "fb2-m", "fb2-p", "authentication failed");

        let emitted = notices.drain(true);

        assert_eq!(emitted.len(), expected.len());
        for (notice, text) in emitted.iter().zip(expected) {
            assert_eq!(notice.kind, "fallback_switch");
            assert_eq!(notice.text, text.as_str().unwrap());
        }
    }
}
