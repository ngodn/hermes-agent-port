//! Mandatory secret scrubbing at the durable compression boundary.

use std::sync::LazyLock;

use fancy_regex::Regex;
use serde_json::Value;

static PATTERNS: LazyLock<Vec<(Regex, &'static str)>> = LazyLock::new(|| {
    [
        (
            r"(?s)-----BEGIN [A-Z ]*PRIVATE KEY-----.*?-----END [A-Z ]*PRIVATE KEY-----",
            "[REDACTED PRIVATE KEY]",
        ),
        (
            r"(?i)\b(?:sk-(?:ant-)?|ghp_|github_pat_|gho_|ghu_|ghs_|ghr_|xapp-\d+-|xox[baprs]-|AIza|pplx-|fal_|fc-|bb_live_|gAAAA|sk_live_|sk_test_|rk_live_|SG\.|hf_|r8_|npm_|pypi-|dop_v1_|doo_v1_|am_|sk_|tvly-|exa_|gsk_|syt_|retaindb_|hsk-|mem0_|brv_|xai-|ntn_|fw[-_]|fpk_|glpat-|gloas-|gldt-|glrt-|glrtr-|glcbt-|glptt-|glft-|glimt-|glagent-|glsoat-|glffct-|glwt-|GR1348941)[A-Za-z0-9_.=-]{8,}",
            "[REDACTED]",
        ),
        (
            r"(?i)\b(Bearer|Basic)\s+[A-Za-z0-9._~+/=-]{8,}",
            "$1 [REDACTED]",
        ),
        (
            r"(?i)\b([a-z][a-z0-9+.-]*://)[^\s/@:]+:[^\s/@]+@",
            "$1[REDACTED]@",
        ),
        (
            r#"(?i)([\"'](?:access_token|refresh_token|id_token|token|api_key|apikey|client_secret|password|auth|jwt|secret|private_key|authorization|key)[\"']\s*:\s*)[\"'][^\"'\r\n]*[\"']"#,
            "$1\"[REDACTED]\"",
        ),
        (
            r"(?i)([?&](?:access_token|refresh_token|id_token|token|api_key|apikey|client_secret|password|auth|jwt|session|secret|key|code|signature|x-amz-signature)=)[^&#\s]+",
            "$1[REDACTED]",
        ),
        (
            r"(?im)\b((?:api[_-]?key|access[_-]?token|refresh[_-]?token|id[_-]?token|client[_-]?secret|password|passwd|secret|private[_-]?key|authorization|auth|jwt|[A-Z0-9_]+_(?:KEY|TOKEN|SECRET|PASSWORD|PASS|PW))\s*[:=]\s*)([^\s,;&]+)",
            "$1[REDACTED]",
        ),
    ]
    .into_iter()
    .map(|(pattern, replacement)| (Regex::new(pattern).expect("valid redaction regex"), replacement))
    .collect()
});

pub fn redact(text: &str) -> String {
    PATTERNS
        .iter()
        .fold(text.to_owned(), |value, (pattern, replacement)| {
            pattern.replace_all(&value, *replacement).into_owned()
        })
}

pub fn redact_value(value: &Value) -> Value {
    match value {
        Value::String(text) => Value::String(redact(text)),
        Value::Array(items) => Value::Array(items.iter().map(redact_value).collect()),
        Value::Object(items) => Value::Object(
            items
                .iter()
                .map(|(key, value)| (key.clone(), redact_value(value)))
                .collect(),
        ),
        other => other.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_prefixes_assignments_urls_headers_and_private_keys() {
        let input = "OPENAI_API_KEY=sk-secretvalue123456\nAuthorization: Bearer abcdefghijklmnop\nhttps://alice:hunter2@example.test/cb?access_token=opaque-value-123\n-----BEGIN PRIVATE KEY-----\nabc123\n-----END PRIVATE KEY-----";
        let redacted = redact(input);
        assert!(!redacted.contains("secretvalue"));
        assert!(!redacted.contains("abcdefghijklmnop"));
        assert!(!redacted.contains("hunter2"));
        assert!(!redacted.contains("opaque-value"));
        assert!(!redacted.contains("abc123"));
        assert!(redacted.matches("[REDACTED]").count() >= 4);
    }

    #[test]
    fn leaves_counts_paths_and_sha_values_intact() {
        let input = "/tmp/token_count.rs 1e0f6c51f1 version=42 token_count=900";
        assert_eq!(redact(input), input);
    }

    #[test]
    fn recursively_redacts_structured_content() {
        let value = serde_json::json!({"parts":[{"text":"password=hunter2"}],"count":4});
        let redacted = redact_value(&value);
        assert_eq!(redacted["parts"][0]["text"], "password=[REDACTED]");
        assert_eq!(redacted["count"], 4);
    }
}
