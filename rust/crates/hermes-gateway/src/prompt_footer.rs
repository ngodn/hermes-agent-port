//! Session metadata footer from agent/system_prompt.py. Capture display-zone
//! dates and zone labels once at prompt construction, never on each API call.
#![allow(dead_code)]

#[derive(Clone, Copy)]
pub enum SessionStart {
    Naive(chrono::NaiveDateTime),
    Aware(chrono::DateTime<chrono::FixedOffset>),
}

/// Apply Python's fixed-width ID match and strptime field restrictions. Some
/// positions accept Unicode decimal digits while month digits remain ASCII.
fn embedded_start(id: &str) -> Option<chrono::NaiveDateTime> {
    let chars: Vec<_> = id.chars().take(15).collect();
    if chars.len() != 15 || chars[8] != '_' {
        return None;
    }
    static PATTERN: std::sync::OnceLock<fancy_regex::Regex> = std::sync::OnceLock::new();
    let pattern = PATTERN.get_or_init(|| fancy_regex::Regex::new(r"^\d{4}(?:1[0-2]|0[1-9])(?:3[0-1]|[1-2]\d|0[1-9])_(?:2[0-3]|[0-1]\d)[0-5]\d(?:6[0-1]|[0-5]\d)$").unwrap());
    if !pattern.is_match(&chars.iter().collect::<String>()).ok()? {
        return None;
    }
    let mut year = 0;
    for ch in &chars[..4] {
        year = year * 10 + crate::python_value::decimal_digit(*ch)? as i32;
    }
    if year == 0 {
        return None;
    }
    let pair = |start: usize| -> Option<u32> {
        let a = chars[start];
        let b = chars[start + 1];
        Some(
            (crate::python_value::decimal_digit(a)? * 10 + crate::python_value::decimal_digit(b)?)
                as u32,
        )
    };
    chrono::NaiveDate::from_ymd_opt(year, pair(4)?, pair(6)?)?.and_hms_opt(
        pair(9)?,
        pair(11)?,
        pair(13)?,
    )
}

/// Lineage root precedes the current segment, then the runner's creation
/// stamp. Python attaches the machine's captured current fixed offset to
/// naive stamps before converting, rather than looking up historical DST.
pub fn session_start_date<Tz: chrono::TimeZone>(
    root_id: Option<&str>,
    session_id: &str,
    creation: Option<SessionStart>,
    now: &chrono::DateTime<Tz>,
    machine_offset: Option<chrono::FixedOffset>,
) -> chrono::NaiveDate {
    use chrono::TimeZone;
    let selected = root_id
        .filter(|_| !session_id.is_empty())
        .and_then(embedded_start)
        .or_else(|| embedded_start(session_id))
        .map(SessionStart::Naive)
        .or(creation);
    match selected {
        Some(SessionStart::Aware(stamp)) => stamp.with_timezone(&now.timezone()).date_naive(),
        Some(SessionStart::Naive(stamp)) => machine_offset
            .and_then(|offset| offset.from_local_datetime(&stamp).single())
            .map(|stamp| stamp.with_timezone(&now.timezone()).date_naive())
            .unwrap_or_else(|| stamp.date()),
        None => now.date_naive(),
    }
}

#[derive(serde::Deserialize)]
pub struct Footer {
    #[serde(deserialize_with = "date")]
    pub now_date: chrono::NaiveDate,
    #[serde(deserialize_with = "date")]
    pub start_date: chrono::NaiveDate,
    pub iana: Option<String>,
    pub abbreviation: String,
    /// Python strftime("%z") output, normally +HHMM or -HHMM.
    pub offset: String,
    pub timeless: bool,
    pub pass_session_id: bool,
    pub session_id: String,
    pub model: String,
    pub provider: String,
    pub platform: String,
}

fn date<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<chrono::NaiveDate, D::Error> {
    let value = <String as serde::Deserialize>::deserialize(deserializer)?;
    chrono::NaiveDate::parse_from_str(&value, "%Y-%m-%d").map_err(serde::de::Error::custom)
}

impl Footer {
    /// Database failures fall through to the segment ID instead of preventing
    /// prompt construction. Call when building/rebuilding the cached prompt.
    pub fn resolve_start_date<Tz: chrono::TimeZone>(
        &mut self,
        db: Option<&crate::session_db::SessionDb>,
        creation: Option<SessionStart>,
        now: &chrono::DateTime<Tz>,
        machine_offset: Option<chrono::FixedOffset>,
    ) {
        let root = db
            .filter(|_| !self.session_id.is_empty())
            .and_then(|db| db.get_conversation_root(&self.session_id).ok());
        self.resolve_start_date_from_root(root.as_deref(), creation, now, machine_offset);
    }

    /// Resolve from metadata captured before prompt construction. This keeps
    /// SQLite access out of the asynchronous prompt assembly phase.
    pub fn resolve_start_date_from_root<Tz: chrono::TimeZone>(
        &mut self,
        root: Option<&str>,
        creation: Option<SessionStart>,
        now: &chrono::DateTime<Tz>,
        machine_offset: Option<chrono::FixedOffset>,
    ) {
        self.start_date = session_start_date(root, &self.session_id, creation, now, machine_offset);
        self.now_date = now.date_naive();
    }

    pub fn render(&self) -> String {
        let mut zones = Vec::new();
        if let Some(iana) = self.iana.as_ref().filter(|value| !value.is_empty()) {
            zones.push(iana.clone());
        }
        if !self.abbreviation.is_empty() && self.iana.as_deref() != Some(&self.abbreviation) {
            zones.push(self.abbreviation.clone());
        }
        if !self.offset.is_empty() {
            zones.push(format!(
                "UTC{}:{}",
                self.offset.chars().take(3).collect::<String>(),
                self.offset.chars().skip(3).collect::<String>()
            ));
        }
        let mut text = if self.timeless {
            if zones.is_empty() {
                String::new()
            } else {
                format!("Timezone: {}", zones.join(", "))
            }
        } else {
            let suffix = if zones.is_empty() {
                String::new()
            } else {
                format!(" ({})", zones.join(", "))
            };
            let mut text = format!(
                "Conversation started: {}{suffix}",
                self.start_date.format("%A, %B %d, %Y")
            );
            if self.now_date != self.start_date {
                text.push_str(&format!("\nToday's date (as of the last context rebuild): {} \u{2014} trust this over the start date for what day it is now; query tools for exact time.", self.now_date.format("%A, %B %d, %Y")));
            }
            text
        };
        for (label, value) in [
            (
                "Session ID",
                if self.pass_session_id {
                    self.session_id.as_str()
                } else {
                    ""
                },
            ),
            ("Model", &self.model),
            ("Provider", &self.provider),
            ("Platform", &self.platform),
        ] {
            if !value.is_empty() {
                text.push_str(&format!("\n{label}: {value}"));
            }
        }
        text
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_start_matches_python_precedence_and_zone_conversion() {
        let cases: Vec<serde_json::Value> = serde_json::from_str(include_str!(
            "../../../tools/session-start-date-goldens.json"
        ))
        .unwrap();
        for case in cases {
            let now = chrono::DateTime::parse_from_rfc3339(case["now"].as_str().unwrap()).unwrap();
            let creation = case["creation"].as_str().map(|text| {
                chrono::DateTime::parse_from_rfc3339(text)
                    .map(SessionStart::Aware)
                    .unwrap_or_else(|_| {
                        SessionStart::Naive(
                            chrono::NaiveDateTime::parse_from_str(text, "%Y-%m-%dT%H:%M:%S")
                                .unwrap(),
                        )
                    })
            });
            let machine = case["machine_offset"]
                .as_i64()
                .map(|seconds| chrono::FixedOffset::east_opt(seconds as i32).unwrap());
            assert_eq!(
                session_start_date(
                    case["root"].as_str(),
                    case["session"].as_str().unwrap(),
                    creation,
                    &now,
                    machine
                )
                .to_string(),
                case["expected"].as_str().unwrap(),
                "{case}"
            );
        }
    }

    #[test]
    fn footer_matches_python_date_zone_and_identity_cases() {
        let cases: Vec<serde_json::Value> =
            serde_json::from_str(include_str!("../../../tools/prompt-footer-goldens.json"))
                .unwrap();
        for case in cases {
            let input: Footer = serde_json::from_value(case["input"].clone()).unwrap();
            let text = input.render();
            assert_eq!(text, case["expected"].as_str().unwrap(), "{case}");
            let mut sections = crate::system_prompt::ResolvedPromptSections::default();
            sections.set_footer(&input);
            assert_eq!(
                sections.assemble().volatile,
                text.trim_matches(crate::python_value::python_whitespace)
            );
        }
    }
}
