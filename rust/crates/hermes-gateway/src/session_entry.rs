//! Persisted SessionEntry records from gateway/session.py. Routing entry
//! serialization is separate from transcript storage and session-key creation.
#![allow(dead_code)]

use crate::session::{
    is_path_unsafe, is_session_key_unsafe, sanitize_model_override, SessionSource,
};
use chrono::{Datelike, Timelike};
use serde_json::{json, Map, Value};

#[derive(Clone, Debug)]
pub struct EntryTimestamp {
    pub local: chrono::NaiveDateTime,
    pub offset_micros: Option<i64>,
}

impl EntryTimestamp {
    pub fn parse(value: &Value) -> anyhow::Result<Self> {
        let (local, offset_micros) = value
            .as_str()
            .and_then(crate::message_timestamps::parse_iso_components)
            .ok_or_else(|| anyhow::anyhow!("invalid session timestamp"))?;
        Ok(Self {
            local,
            offset_micros,
        })
    }

    pub fn isoformat(&self) -> String {
        let mut text = self.local.format("%Y-%m-%dT%H:%M:%S").to_string();
        let micros = self.local.nanosecond() / 1000;
        if micros != 0 {
            text.push_str(&format!(".{micros:06}"));
        }
        if let Some(offset) = self.offset_micros {
            let abs = offset.unsigned_abs();
            let seconds = abs / 1_000_000;
            text.push_str(&format!(
                "{}{:02}:{:02}",
                if offset < 0 { '-' } else { '+' },
                seconds / 3600,
                (seconds / 60) % 60
            ));
            if seconds % 60 != 0 || abs % 1_000_000 != 0 {
                text.push_str(&format!(":{:02}", seconds % 60));
                if abs % 1_000_000 != 0 {
                    text.push_str(&format!(".{:06}", abs % 1_000_000));
                }
            }
        }
        text
    }
}

/// Structural fields are typed; counters and flags retain the source's
/// uncoerced JSON values, including explicit nulls in historical records.
#[derive(Clone, Debug)]
pub struct SessionEntry {
    // Snapshots retain object identity across unlocked checks. Reloaded or newly
    // constructed entries receive a new token, even when their IDs are equal.
    identity: std::sync::Arc<()>,
    pub session_key: String,
    pub session_id: String,
    pub created_at: EntryTimestamp,
    pub updated_at: EntryTimestamp,
    pub origin: Option<SessionSource>,
    pub fields: Map<String, Value>,
}

/// Reset lineage carried from the predecessor into a newly created route.
#[derive(Default)]
pub struct CreationContext {
    pub is_fresh_reset: bool,
    pub was_auto_reset: bool,
    pub auto_reset_reason: Option<String>,
    pub reset_had_activity: bool,
    pub prev_session_id: Option<String>,
}

impl SessionEntry {
    /// Advance the same stable route onto a newly published compression child.
    /// Usage counters and origin metadata belong to the conversation and stay
    /// attached; the prompt-token watermark resets at the cache boundary.
    pub fn compression_candidate(
        current: &Self,
        now: chrono::NaiveDateTime,
    ) -> anyhow::Result<Self> {
        let random = crate::install_identity::mint_id()
            .ok_or_else(|| anyhow::anyhow!("could not generate session identity"))?;
        let mut candidate = current.clone();
        candidate.identity = std::sync::Arc::new(());
        candidate.session_id = format!("{}_{}", now.format("%Y%m%d_%H%M%S"), &random[..8]);
        candidate.updated_at = EntryTimestamp {
            local: now,
            offset_micros: None,
        };
        candidate
            .fields
            .insert("last_prompt_tokens".into(), json!(0));
        Ok(candidate)
    }

    /// Rebind one stable route to an existing transcript. Conversation-scoped
    /// counters and overrides do not cross the boundary; only presentation and
    /// origin identity from the route are retained.
    pub fn resumed_candidate(
        current: &Self,
        target_session_id: &str,
        now: chrono::NaiveDateTime,
    ) -> anyhow::Result<Self> {
        let timestamp = EntryTimestamp {
            local: now,
            offset_micros: None,
        }
        .isoformat();
        let mut candidate = Self::from_dict(&json!({
            "session_key": current.session_key,
            "session_id": target_session_id,
            "created_at": timestamp,
            "updated_at": timestamp,
            "display_name": current.fields["display_name"],
            "platform": current.fields["platform"],
            "chat_type": current.fields["chat_type"],
        }))?;
        candidate.origin = current.origin.clone();
        Ok(candidate)
    }

    /// Apply an unlocked compression lookup only if this route still points
    /// at the session that was queried. Keep counters, lineage and identity.
    pub fn heal_compression_tip(&mut self, original: Option<&str>, tip: Option<&str>) -> bool {
        let (Some(original), Some(tip)) = (original, tip) else {
            return false;
        };
        if original.is_empty() || tip.is_empty() || original == tip || self.session_id != original {
            return false;
        }
        self.session_id = tip.to_owned();
        true
    }

    pub fn same_instance(&self, observed: &Self) -> bool {
        std::sync::Arc::ptr_eq(&self.identity, &observed.identity)
    }

    /// Build outside the routing lock. Publication decides whether this
    /// candidate wins before any caller creates its durable database row.
    pub fn new_candidate(
        key: &str,
        source: &SessionSource,
        now: chrono::NaiveDateTime,
        context: &CreationContext,
    ) -> anyhow::Result<Self> {
        let random = crate::install_identity::mint_id()
            .ok_or_else(|| anyhow::anyhow!("could not generate session identity"))?;
        let id = format!("{}_{}", now.format("%Y%m%d_%H%M%S"), &random[..8]);
        let timestamp = EntryTimestamp {
            local: now,
            offset_micros: None,
        }
        .isoformat();
        let mut candidate = Self::from_dict(&json!({
            "session_key": key, "session_id":id,
            "created_at":timestamp, "updated_at":timestamp,
            "display_name":source.chat_name, "platform":source.platform,
            "chat_type":source.chat_type,
            "is_fresh_reset":context.is_fresh_reset,
            "was_auto_reset":context.was_auto_reset,
            "auto_reset_reason":context.auto_reset_reason,
            "reset_had_activity":context.reset_had_activity,
            "prev_session_id":context.prev_session_id,
        }))?;
        candidate.origin = Some(source.clone());
        Ok(candidate)
    }

    /// Reconstruct routing state from durable recency, never the current clock.
    /// Fixed offsets are supplied by deterministic tests; production uses local
    /// time, matching datetime.fromtimestamp's naive local result.
    pub fn from_recovered_row(
        row: &Value,
        key: &str,
        source: &SessionSource,
        timezone: Option<chrono::FixedOffset>,
    ) -> anyhow::Result<Self> {
        let created = recovered_timestamp(&row["started_at"], timezone)
            .unwrap_or_else(|| recovered_timestamp(&json!(0), timezone).unwrap());
        let updated = recovered_timestamp(&row["last_activity_at"], timezone)
            .unwrap_or_else(|| created.clone());
        let had_activity = if row["_has_messages"].is_null() {
            crate::python_value::truthy(&row["message_count"]) || !row["last_activity_at"].is_null()
        } else {
            crate::python_value::truthy(&row["_has_messages"])
        };
        let id = row
            .get("id")
            .ok_or_else(|| anyhow::anyhow!("missing recovered session ID"))?;
        let id = match id {
            Value::String(s) => s.clone(),
            other => crate::python_value::python_repr(other),
        };
        let mut entry = Self::from_dict(&json!({"session_key":key,"session_id":id,
            "created_at":created.isoformat(),"updated_at":updated.isoformat(),
            "display_name":source.chat_name,"platform":source.platform,"chat_type":source.chat_type,
            "reset_had_activity":had_activity}))?;
        entry.origin = Some(source.clone());
        Ok(entry)
    }

    pub fn from_dict(data: &Value) -> anyhow::Result<Self> {
        let required = |key: &str| {
            data[key]
                .as_str()
                .map(str::to_owned)
                .ok_or_else(|| anyhow::anyhow!("missing or invalid {key}"))
        };
        let session_key = required("session_key")?;
        let session_id = required("session_id")?;
        anyhow::ensure!(!is_path_unsafe(&session_id), "unsafe session ID");
        anyhow::ensure!(!is_session_key_unsafe(&session_key), "unsafe session key");
        let defaults = json!({"display_name":null,"chat_type":"dm","input_tokens":0,"output_tokens":0,
            "cache_read_tokens":0,"cache_write_tokens":0,"total_tokens":0,"last_prompt_tokens":0,
            "estimated_cost_usd":0.0,"cost_status":"unknown","suspended":false,"resume_pending":false,
            "resume_reason":null,"is_fresh_reset":false,"was_auto_reset":false,"auto_reset_reason":null,
            "reset_had_activity":false,"prev_session_id":null});
        let mut fields = Map::new();
        for (key, default) in defaults.as_object().unwrap() {
            fields.insert(key.clone(), data.get(key).unwrap_or(default).clone());
        }
        fields.insert(
            "expiry_finalized".into(),
            data.get("expiry_finalized")
                .or_else(|| data.get("memory_flushed"))
                .cloned()
                .unwrap_or(json!(false)),
        );
        let platform = normalized_platform(&data["platform"]);
        fields.insert("platform".into(), json!(platform));
        let metadata = &data["metadata"];
        let metadata = if !crate::python_value::truthy(metadata) {
            Map::new()
        } else if let Some(object) = metadata.as_object() {
            object.clone()
        } else if let Some(pairs) = metadata.as_array() {
            let mut object = Map::new();
            for pair in pairs {
                // Python's dict constructor accepts any two-item sequence,
                // including a two-character string.
                let characters;
                let pair = if let Some(text) = pair.as_str() {
                    characters = Value::Array(text.chars().map(|c| json!(c.to_string())).collect());
                    &characters
                } else {
                    pair
                };
                let pair = pair
                    .as_array()
                    .filter(|p| p.len() == 2)
                    .ok_or_else(|| anyhow::anyhow!("invalid metadata pair"))?;
                let key = match &pair[0] {
                    Value::String(s) => s.clone(),
                    Value::Number(_) | Value::Bool(_) | Value::Null => pair[0].to_string(),
                    _ => anyhow::bail!("invalid metadata key"),
                };
                object.insert(key, pair[1].clone());
            }
            object
        } else {
            anyhow::bail!("invalid session metadata")
        };
        fields.insert("metadata".into(), Value::Object(metadata));
        for key in ["last_resume_marked_at", "active_turn_started_at"] {
            fields.insert(
                key.into(),
                json!(EntryTimestamp::parse(&data[key])
                    .ok()
                    .map(|t| t.isoformat())),
            );
        }
        let token = data["active_turn_token"].as_str().filter(|s| !s.is_empty());
        fields.insert("active_turn_token".into(), json!(token));
        if token.is_none() {
            fields.insert("active_turn_started_at".into(), Value::Null);
        }
        if let Some(overrides) = sanitize_model_override(data.get("model_override")) {
            fields.insert("model_override".into(), overrides);
        }
        let origin = if let Some(origin) = data.get("origin").filter(|v| v.is_object()) {
            let platform = normalized_platform(&origin["platform"])
                .ok_or_else(|| anyhow::anyhow!("invalid origin platform"))?;
            anyhow::ensure!(origin.get("chat_id").is_some(), "missing origin chat ID");
            let mut parsed = SessionSource::from_dict(origin);
            parsed.platform = platform.to_owned();
            Some(parsed)
        } else {
            None
        };
        Ok(Self {
            identity: std::sync::Arc::new(()),
            session_key,
            session_id,
            created_at: EntryTimestamp::parse(&data["created_at"])?,
            updated_at: EntryTimestamp::parse(&data["updated_at"])?,
            origin,
            fields,
        })
    }

    pub fn to_dict(&self) -> Value {
        let mut fields = self.fields.clone();
        // Sanitize again on write, including direct in-memory mutation.
        let overrides = fields.remove("model_override");
        if overrides.as_ref().is_some_and(crate::python_value::truthy) {
            fields.insert(
                "model_override".into(),
                sanitize_model_override(overrides.as_ref()).unwrap_or(Value::Null),
            );
        }
        fields.insert("session_key".into(), json!(self.session_key));
        fields.insert("session_id".into(), json!(self.session_id));
        fields.insert("created_at".into(), json!(self.created_at.isoformat()));
        fields.insert("updated_at".into(), json!(self.updated_at.isoformat()));
        if let Some(origin) = &self.origin {
            fields.insert("origin".into(), origin.to_dict());
        }
        Value::Object(fields)
    }
}

fn recovered_timestamp(
    value: &Value,
    timezone: Option<chrono::FixedOffset>,
) -> Option<EntryTimestamp> {
    let seconds = match value {
        Value::Bool(v) => f64::from(*v),
        Value::Number(n) => n.as_f64()?,
        Value::String(s) => crate::python_value::numeric_text(s)?.parse::<f64>().ok()?,
        _ => return None,
    };
    if !seconds.is_finite() {
        return None;
    }
    // Round the fractional part independently to avoid losing microseconds
    // when a large epoch value is multiplied by one million.
    let micros = (seconds.fract() * 1_000_000.0).round_ties_even() as i64;
    let utc = chrono::DateTime::from_timestamp(seconds.trunc() as i64, 0)?
        .checked_add_signed(chrono::Duration::microseconds(micros))?;
    // Bound before applying local offsets so chrono's wider date range cannot
    // overflow during conversion. Adjacent years can cross into Python's range.
    if !(0..=10000).contains(&utc.year()) {
        return None;
    }
    let local = match timezone {
        Some(tz) => utc.with_timezone(&tz).naive_local(),
        None => utc.with_timezone(&chrono::Local).naive_local(),
    };
    (1..=9999)
        .contains(&local.year())
        .then_some(EntryTimestamp {
            local,
            offset_micros: None,
        })
}

// Plugin pseudo-members need the platform registry, which is not ported yet.
fn normalized_platform(value: &Value) -> Option<&'static str> {
    let name = value.as_str()?;
    let name = name
        .trim_matches(crate::python_value::python_whitespace)
        .to_lowercase();
    crate::config_schema::Platform::from_value(&name).map(|p| p.value())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compression_healing_preserves_entry_state_and_rejects_stale_results() {
        let data = json!({"session_key":"key", "session_id":"parent", "created_at":"2026-09-06", "updated_at":"2026-09-06", "input_tokens":42, "metadata":{"queued":true}});
        let mut entry = SessionEntry::from_dict(&data).unwrap();
        let observed = entry.clone();
        assert!(!entry.heal_compression_tip(Some("parent"), None));
        assert!(!entry.heal_compression_tip(Some("parent"), Some("")));
        assert!(!entry.heal_compression_tip(Some("parent"), Some("parent")));
        assert!(entry.heal_compression_tip(Some("parent"), Some("child")));
        assert!(entry.same_instance(&observed));
        let mut expected = observed.to_dict();
        expected["session_id"] = json!("child");
        assert_eq!(entry.to_dict(), expected);
        assert!(!entry.heal_compression_tip(Some("parent"), Some("other")));
        assert_eq!(entry.session_id, "child");
    }

    #[test]
    fn recovered_entries_match_python_local_time_and_activity() {
        let cases: Vec<Value> = serde_json::from_str(include_str!(
            "../../../tools/session-recovered-entry-goldens.json"
        ))
        .unwrap();
        for case in cases {
            let source = SessionSource::from_dict(&case["source"]);
            let entry = SessionEntry::from_recovered_row(
                &case["row"],
                "agent:main:slack:group:C",
                &source,
                chrono::FixedOffset::east_opt(case["offset"].as_i64().unwrap() as i32),
            )
            .unwrap();
            assert_eq!(entry.to_dict(), case["expected"], "{case}");
        }
    }

    #[test]
    fn persisted_entries_match_python() {
        let cases: Vec<Value> =
            serde_json::from_str(include_str!("../../../tools/session-entry-goldens.json"))
                .unwrap();
        for (index, case) in cases.iter().enumerate() {
            let result = SessionEntry::from_dict(&case["input"]);
            assert_eq!(
                result.is_err(),
                case["error"].as_bool().unwrap(),
                "case {index}: {case}"
            );
            if let Ok(entry) = result {
                assert_eq!(
                    entry.to_dict(),
                    case["result"],
                    "case {index}: {}",
                    case["input"]
                );
            }
        }
    }

    #[test]
    fn serialization_strips_credentials_after_direct_mutation() {
        let mut entry = SessionEntry::from_dict(&json!({
            "session_key":"agent:main:slack:group:C", "session_id":"s1",
            "created_at":"2026-09-06", "updated_at":"2026-09-06"
        }))
        .unwrap();
        entry.fields.insert(
            "model_override".into(),
            json!({"model":"test", "api_key":"fixture-secret"}),
        );
        assert_eq!(entry.to_dict()["model_override"], json!({"model":"test"}));
        entry
            .fields
            .insert("model_override".into(), json!({"api_key":"fixture-secret"}));
        assert_eq!(entry.to_dict()["model_override"], Value::Null);
    }
}
