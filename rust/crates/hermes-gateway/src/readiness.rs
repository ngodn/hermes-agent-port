//! Bounded, non-destructive readiness probes for health surfaces.
//!
//! Port of the portable probes in `gateway/readiness.py`. Each probe reports a
//! coarse status ("ok" / "degraded") and a short, non-sensitive detail, never
//! config values, credentials, paths, or exception messages. Probes must not
//! mutate runtime state or compete with normal writers.
//!
//! Ported: config (config.yaml exists + parses to a mapping), model (a model is
//! configured), state_db (state.db opens read-only and answers a schema query).
//! Not yet ported (need subsystems that don't exist in Rust yet): session_store,
//! gateway runtime state, background queue depths, disk usage.

use std::collections::BTreeMap;
use std::path::Path;

use serde::Serialize;
use serde_json::Value;

/// One probe result.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Probe {
    pub status: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

impl Probe {
    fn ok() -> Self {
        Self {
            status: "ok",
            detail: None,
        }
    }
    fn ok_detail(detail: &str) -> Self {
        Self {
            status: "ok",
            detail: Some(detail.to_string()),
        }
    }
    fn degraded(detail: &str) -> Self {
        Self {
            status: "degraded",
            detail: Some(detail.to_string()),
        }
    }
    fn is_ok(&self) -> bool {
        self.status == "ok"
    }
}

/// Aggregate readiness across all probes.
#[derive(Debug, Clone, Serialize)]
pub struct Readiness {
    pub status: &'static str,
    pub checks: BTreeMap<&'static str, Probe>,
}

/// Run the portable probes against `home` and the configured model.
pub fn collect_readiness(home: &Path, configured_model: Option<&str>) -> Readiness {
    let mut checks = BTreeMap::new();
    checks.insert("config", probe_config(home));
    checks.insert("model", probe_model(configured_model));
    checks.insert("state_db", probe_state_db(home));

    let status = if checks.values().all(Probe::is_ok) {
        "ok"
    } else {
        "degraded"
    };
    Readiness { status, checks }
}

fn probe_config(home: &Path) -> Probe {
    let path = home.join("config.yaml");
    if !path.exists() {
        return Probe::ok_detail("using defaults");
    }
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(_) => return Probe::degraded("unreadable config"),
    };
    if text.trim().is_empty() {
        return Probe::ok();
    }
    match serde_yaml_ng::from_str::<Value>(&text) {
        Ok(Value::Object(_)) | Ok(Value::Null) => Probe::ok(),
        Ok(_) => Probe::degraded("top level is not a mapping"),
        Err(_) => Probe::degraded("invalid config"),
    }
}

fn probe_model(configured_model: Option<&str>) -> Probe {
    match configured_model {
        Some(m) if !m.trim().is_empty() => Probe::ok(),
        _ => Probe::degraded("no model configured"),
    }
}

fn probe_state_db(home: &Path) -> Probe {
    let path = home.join("state.db");
    if !path.exists() {
        return Probe::ok_detail("not initialized");
    }
    // Read-only open so the probe never competes with state writers or takes a
    // write reservation on a health poll.
    let flags = rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY;
    let conn = match rusqlite::Connection::open_with_flags(&path, flags) {
        Ok(c) => c,
        Err(_) => return Probe::degraded("unopenable"),
    };
    // A read-only schema query catches unreadable/corrupt databases without
    // mutating anything.
    let probe = conn.execute_batch("PRAGMA query_only = ON;").and_then(|_| {
        conn.query_row("SELECT name FROM sqlite_master LIMIT 1", [], |_| Ok(()))
            .or_else(|e| match e {
                // An empty schema is still a healthy database.
                rusqlite::Error::QueryReturnedNoRows => Ok(()),
                other => Err(other),
            })
    });
    match probe {
        Ok(()) => Probe::ok(),
        Err(_) => Probe::degraded("unreadable"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn temp_dir(tag: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "hermes_ready_{}_{}_{}",
            tag,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn config_probe_variants() {
        let d = temp_dir("cfg");
        // Absent -> ok (defaults).
        assert_eq!(probe_config(&d).status, "ok");
        // Valid mapping -> ok.
        std::fs::write(d.join("config.yaml"), "model:\n  default: x\n").unwrap();
        assert_eq!(probe_config(&d).status, "ok");
        // Top-level scalar -> degraded.
        std::fs::write(d.join("config.yaml"), "42").unwrap();
        assert_eq!(probe_config(&d).status, "degraded");
        // Broken YAML -> degraded.
        std::fs::write(d.join("config.yaml"), "a: [1,\nb: {").unwrap();
        assert_eq!(probe_config(&d).status, "degraded");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn model_probe() {
        assert_eq!(probe_model(Some("gpt-x")).status, "ok");
        assert_eq!(probe_model(Some("  ")).status, "degraded");
        assert_eq!(probe_model(None).status, "degraded");
    }

    #[test]
    fn state_db_probe_absent_and_valid_and_corrupt() {
        let d = temp_dir("db");
        // Absent -> ok (not initialized).
        assert_eq!(probe_state_db(&d).status, "ok");

        // A real sqlite db -> ok.
        let dbp = d.join("state.db");
        {
            let conn = rusqlite::Connection::open(&dbp).unwrap();
            conn.execute_batch("CREATE TABLE t (a);").unwrap();
        }
        assert_eq!(probe_state_db(&d).status, "ok");

        // A garbage file with the db name -> degraded.
        std::fs::write(&dbp, b"not a database").unwrap();
        assert_eq!(probe_state_db(&d).status, "degraded");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn collect_is_degraded_if_any_probe_is() {
        let d = temp_dir("agg");
        // No model configured -> overall degraded even with ok config/db.
        let r = collect_readiness(&d, None);
        assert_eq!(r.status, "degraded");
        assert_eq!(r.checks["model"].status, "degraded");
        assert_eq!(r.checks["config"].status, "ok");
        // With a model and no files, everything is ok.
        let r2 = collect_readiness(&d, Some("m"));
        assert_eq!(r2.status, "ok");
        let _ = std::fs::remove_dir_all(&d);
    }
}
