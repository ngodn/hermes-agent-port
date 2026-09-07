//! Profile-scoped clock inputs from hermes_time.py. Cache identity follows the
//! configured source, including invalid values, until an explicit reset.
#![allow(dead_code)]

use chrono::{DateTime, FixedOffset, Local, Utc};
use chrono_tz::Tz;
use std::{collections::HashMap, ffi::OsString, path::Path, sync::Mutex};

#[derive(Hash, PartialEq, Eq)]
enum Identity {
    Environment(String),
    Config(OsString),
}

#[derive(Default)]
pub struct TimezoneCache {
    entries: Mutex<HashMap<Identity, Option<Tz>>>,
}

impl TimezoneCache {
    /// Config is the raw profile snapshot after the managed overlay. Captured
    /// env input wins even when invalid; do not retry the config timezone then.
    pub fn resolve(
        &self,
        environment: &str,
        config_path: &Path,
        config: &serde_json::Value,
    ) -> Option<Tz> {
        let environment = environment.trim_matches(crate::python_value::python_whitespace);
        let identity = if environment.is_empty() {
            Identity::Config(config_path.as_os_str().to_owned())
        } else {
            Identity::Environment(environment.into())
        };
        let mut entries = self
            .entries
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        *entries.entry(identity).or_insert_with(|| {
            let name = if environment.is_empty() {
                config.get("timezone").and_then(serde_json::Value::as_str).unwrap_or("").trim_matches(crate::python_value::python_whitespace)
            } else { environment };
            if name.is_empty() { return None; }
            match name.parse::<Tz>() {
                Ok(zone) => Some(zone),
                Err(error) => { tracing::warn!(timezone = name, %error, "Invalid timezone; using server local time"); None }
            }
        })
    }

    pub fn reset(&self) {
        self.entries
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clear();
    }
}

pub struct ClockSnapshot {
    utc: DateTime<Utc>,
    zone: Option<Tz>,
    machine_offset: FixedOffset,
    local_abbreviation: String,
}

#[cfg(unix)]
fn local_abbreviation(timestamp: i64) -> Option<String> {
    // time_t is narrower on some supported Unix targets.
    #[allow(clippy::useless_conversion)]
    let timestamp: libc::time_t = timestamp.try_into().ok()?;
    let mut local = std::mem::MaybeUninit::<libc::tm>::uninit();
    let mut buffer = [0u8; 256];
    // SAFETY: localtime_r initializes our tm on success; strftime writes at
    // most buffer.len() bytes with a constant, NUL-terminated format string.
    unsafe {
        if libc::localtime_r(&timestamp, local.as_mut_ptr()).is_null() {
            return None;
        }
        let length = libc::strftime(
            buffer.as_mut_ptr().cast(),
            buffer.len(),
            c"%Z".as_ptr(),
            local.as_ptr(),
        );
        Some(String::from_utf8_lossy(&buffer[..length]).into_owned())
    }
}

#[cfg(not(unix))]
fn local_abbreviation(_timestamp: i64) -> Option<String> {
    None
}

impl ClockSnapshot {
    pub fn capture(zone: Option<Tz>) -> Self {
        let utc = Utc::now();
        let local = utc.with_timezone(&Local);
        Self {
            utc,
            zone,
            machine_offset: *local.offset(),
            local_abbreviation: local_abbreviation(utc.timestamp())
                .unwrap_or_else(|| local.format("%Z").to_string()),
        }
    }

    /// Use the same instant and zone for footer dates, zone labels and lineage
    /// conversion. An IANA zone retains historical DST rules for the start date.
    pub fn apply(
        &self,
        footer: &mut crate::prompt_footer::Footer,
        db: Option<&crate::session_db::SessionDb>,
        creation: Option<crate::prompt_footer::SessionStart>,
    ) {
        let root = db
            .filter(|_| !footer.session_id.is_empty())
            .and_then(|db| db.get_conversation_root(&footer.session_id).ok());
        self.apply_with_root(footer, root.as_deref(), creation);
    }

    /// Apply a lineage root captured before prompt construction.
    pub fn apply_with_root(
        &self,
        footer: &mut crate::prompt_footer::Footer,
        root: Option<&str>,
        creation: Option<crate::prompt_footer::SessionStart>,
    ) {
        if let Some(zone) = self.zone {
            let now = self.utc.with_timezone(&zone);
            footer.iana = Some(zone.name().into());
            footer.abbreviation = now.format("%Z").to_string();
            footer.offset = now.format("%z").to_string();
            footer.resolve_start_date_from_root(root, creation, &now, Some(self.machine_offset));
        } else {
            let now = self.utc.with_timezone(&self.machine_offset);
            footer.iana = None;
            footer.abbreviation = self.local_abbreviation.clone();
            footer.offset = now.format("%z").to_string();
            footer.resolve_start_date_from_root(root, creation, &now, Some(self.machine_offset));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_is_scoped_and_invalid_environment_does_not_fall_through() {
        let cache = TimezoneCache::default();
        let red = Path::new("/profiles/red/config.yaml");
        let blue = Path::new("/profiles/blue/config.yaml");
        let ny = serde_json::json!({"timezone":" America/New_York "});
        let kl = serde_json::json!({"timezone":"Asia/Kuala_Lumpur"});
        assert_eq!(
            cache.resolve("", red, &ny).unwrap().name(),
            "America/New_York"
        );
        assert_eq!(
            cache.resolve("", blue, &kl).unwrap().name(),
            "Asia/Kuala_Lumpur"
        );
        assert_eq!(
            cache.resolve("", red, &kl).unwrap().name(),
            "America/New_York"
        );
        assert!(cache.resolve(" invalid/zone ", red, &kl).is_none());
        assert_eq!(cache.resolve(" UTC ", red, &kl).unwrap().name(), "UTC");
        cache.reset();
        assert_eq!(
            cache.resolve("", red, &kl).unwrap().name(),
            "Asia/Kuala_Lumpur"
        );
    }

    #[test]
    fn iana_clock_keeps_historical_start_date_and_current_offset() {
        let mut footer: crate::prompt_footer::Footer = serde_json::from_value(serde_json::json!({
            "now_date":"2026-01-01","start_date":"2026-01-01","iana":null,"abbreviation":"","offset":"",
            "timeless":false,"pass_session_id":false,"session_id":"20260101_043000_origin","model":"","provider":"","platform":""
        })).unwrap();
        let clock = ClockSnapshot {
            utc: DateTime::parse_from_rfc3339("2026-07-01T12:00:00Z")
                .unwrap()
                .with_timezone(&Utc),
            zone: Some(chrono_tz::America::New_York),
            machine_offset: FixedOffset::east_opt(0).unwrap(),
            local_abbreviation: "UTC".into(),
        };
        clock.apply(&mut footer, None, None);
        assert_eq!(footer.start_date.to_string(), "2025-12-31");
        assert_eq!(footer.now_date.to_string(), "2026-07-01");
        assert_eq!(footer.abbreviation, "EDT");
        assert_eq!(footer.offset, "-0400");
        assert!(footer.render().contains("America/New_York, EDT, UTC-04:00"));
    }
}
