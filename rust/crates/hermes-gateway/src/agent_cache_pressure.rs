//! Port of gateway/agent_cache_pressure.py.
//!
// Public API is ahead of its callers (the gateway agent-cache sweep wires it).
#![allow(dead_code)]
//!
//! Memory-pressure bounds for the gateway's per-session agent cache. The gateway
//! caches one agent per session so a long-lived conversation reuses its prompt
//! prefix, but each cached agent pins the full live transcript (tens of MB on a
//! tool-heavy session). The entry-count LRU cap and the idle TTL are both blind
//! to how much memory the cache actually holds, so a busy gateway can hoard
//! every transcript until it hits the OOM killer (#80764).
//!
//! This supplies the missing signal: the process's own anonymous RSS compared
//! against a budget derived from the cgroup limit the gateway runs under. The
//! sweep sheds LRU transcripts through the soft-eviction path (rebuilt from the
//! persisted session next turn).
//!
//! Scope: this port covers the pure/OS-level pieces (bounds resolution, the
//! cgroup/total-memory limits, anon RSS, eviction planning). The
//! `transcript_persistence_caught_up` guard and the actual cache sweep depend on
//! the `AIAgent` type + `GatewayRunner`, so they land with the agent core loop
//! (Phase 4). Everything here is pure or read-only and testable without a
//! gateway. Config lives under `agent.agent_cache` in config.yaml.

use serde_json::Value;

// Fraction of the resolved memory limit at which we start shedding transcripts.
// Well under the limit: eviction has to happen while the process still has room
// to breathe (past cgroup memory.high throttling a SIGTERM flush cannot finish
// inside systemd's stop timeout).
const AUTO_BUDGET_FRACTION: f64 = 0.65;
// Below this a budget is noise: small containers would evict every pass and
// never keep a warm prefix.
const AUTO_BUDGET_FLOOR_MB: i64 = 512;

const DEFAULT_MAX_EVICTIONS_PER_PASS: i64 = 16;
// Never let a pressure pass touch the hottest sessions.
const DEFAULT_PROTECT_RECENT: i64 = 8;

const BYTES_PER_MB: i64 = 1024 * 1024;

/// Operator-facing bounds for the per-session agent cache. `max_size` and
/// `idle_ttl_secs` are `None` when the operator did not set them (so the
/// gateway keeps its own defaults); `memory_high_mb` is `None` when pressure
/// eviction is off.
#[derive(Debug, Clone, PartialEq)]
pub struct AgentCacheBounds {
    pub max_size: Option<i64>,
    pub idle_ttl_secs: Option<f64>,
    pub memory_high_mb: Option<i64>,
    pub max_evictions_per_pass: i64,
    pub protect_recent: i64,
}

impl Default for AgentCacheBounds {
    fn default() -> Self {
        Self {
            max_size: None,
            idle_ttl_secs: None,
            memory_high_mb: None,
            max_evictions_per_pass: DEFAULT_MAX_EVICTIONS_PER_PASS,
            protect_recent: DEFAULT_PROTECT_RECENT,
        }
    }
}

/// Python `int(value)` if positive, else `None` (rejects bool/None; truncates a
/// float toward zero like `int(3.7) == 3`).
fn positive_int(value: Option<&Value>) -> Option<i64> {
    let parsed = match value? {
        Value::Bool(_) | Value::Null => return None,
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                i
            } else {
                n.as_f64()? as i64 // truncates toward zero
            }
        }
        // int("123") works; int("12.5") raises -> None.
        Value::String(s) => s.trim().parse::<i64>().ok()?,
        _ => return None,
    };
    if parsed > 0 {
        Some(parsed)
    } else {
        None
    }
}

/// Like [`positive_int`] but for a bare string (the "auto"/numeric setting path).
fn positive_int_str(s: &str) -> Option<i64> {
    let parsed = s.trim().parse::<i64>().ok()?;
    if parsed > 0 {
        Some(parsed)
    } else {
        None
    }
}

fn positive_float(value: Option<&Value>) -> Option<f64> {
    let parsed = match value? {
        Value::Bool(_) | Value::Null => return None,
        Value::Number(n) => n.as_f64()?,
        Value::String(s) => s.trim().parse::<f64>().ok()?,
        _ => return None,
    };
    if parsed.is_finite() && parsed > 0.0 {
        Some(parsed)
    } else {
        None
    }
}

/// The memory limit this process runs under if cgroup-capped, else `None`.
/// Prefers cgroup v2 `memory.high` (the throttling point) over `memory.max`,
/// checking the process's own cgroup before the root, then falls back to v1.
/// `max` / absurd sentinels mean unlimited.
#[cfg(target_os = "linux")]
fn cgroup_limit_bytes() -> Option<i64> {
    let mut candidates: Vec<String> = Vec::new();
    if let Some(own) = crate::cgroup_cleanup::own_cgroup_path() {
        if own != "/" {
            candidates.push(format!("/sys/fs/cgroup{own}/memory.high"));
            candidates.push(format!("/sys/fs/cgroup{own}/memory.max"));
        }
    }
    candidates.push("/sys/fs/cgroup/memory.high".to_string());
    candidates.push("/sys/fs/cgroup/memory.max".to_string());
    candidates.push("/sys/fs/cgroup/memory/memory.limit_in_bytes".to_string());

    for candidate in candidates {
        let Ok(raw) = std::fs::read_to_string(&candidate) else {
            continue;
        };
        let raw = raw.trim();
        if raw.is_empty() || raw == "max" {
            continue;
        }
        let Ok(limit) = raw.parse::<i64>() else {
            continue;
        };
        // cgroup v1 reports "unlimited" as a near-2^63 sentinel.
        if limit <= 0 || limit >= (1i64 << 62) {
            continue;
        }
        return Some(limit);
    }
    None
}

#[cfg(not(target_os = "linux"))]
fn cgroup_limit_bytes() -> Option<i64> {
    None
}

/// Total physical memory in bytes (`SC_PAGE_SIZE * SC_PHYS_PAGES`), or `None`.
#[cfg(unix)]
fn total_memory_bytes() -> Option<i64> {
    // SAFETY: sysconf reads a system constant; no memory is touched.
    let page = unsafe { libc::sysconf(libc::_SC_PAGE_SIZE) };
    let pages = unsafe { libc::sysconf(libc::_SC_PHYS_PAGES) };
    if page > 0 && pages > 0 {
        page.checked_mul(pages)
    } else {
        None
    }
}

#[cfg(not(unix))]
fn total_memory_bytes() -> Option<i64> {
    None
}

/// Resolve the `memory_high_mb` setting into an absolute MB budget. `"auto"`
/// (or `true`) derives a budget from the cgroup limit (or total RAM when
/// uncapped); a positive number is taken literally; anything falsy disables the
/// pass.
pub fn resolve_memory_high_mb(setting: &Value) -> Option<i64> {
    // Decide whether to fall through to the auto-budget computation. Only the
    // "auto" string and the boolean `true` do; every other case returns here.
    match setting {
        Value::String(s) => {
            let normalized = s.trim().to_lowercase();
            if normalized != "auto" {
                return if matches!(
                    normalized.as_str(),
                    "" | "off" | "none" | "false" | "disabled"
                ) {
                    None
                } else {
                    positive_int_str(&normalized)
                };
            }
            // "auto" -> fall through.
        }
        Value::Bool(b) => {
            if !*b {
                return None;
            }
            // true -> fall through to auto.
        }
        other => return positive_int(Some(other)),
    }

    let limit = cgroup_limit_bytes().or_else(total_memory_bytes)?;
    let budget = (limit as f64 * AUTO_BUDGET_FRACTION / BYTES_PER_MB as f64) as i64;
    if budget >= AUTO_BUDGET_FLOOR_MB {
        Some(budget)
    } else {
        None
    }
}

/// Read `agent.agent_cache` out of a raw config value. Reads the raw user
/// config (no deep-merge), so an absent key stays absent and the caller can
/// tell "operator chose 128" from "operator said nothing".
pub fn resolve_agent_cache_bounds(config: &Value) -> AgentCacheBounds {
    let section = config
        .get("agent")
        .and_then(|a| a.get("agent_cache"))
        .filter(|s| s.is_object());

    let get = |key: &str| section.and_then(|s| s.get(key));

    // protect_recent: an explicit integer 0 means "shed anything" (distinct from
    // unset); a YAML-typo bool stays on the default.
    let protect_raw = get("protect_recent");
    let mut protect_parsed = positive_int(protect_raw);
    if protect_parsed.is_none() {
        if let Some(Value::Number(n)) = protect_raw {
            if n.as_i64() == Some(0) {
                protect_parsed = Some(0);
            }
        }
    }

    let max_evictions = positive_int(get("max_evictions_per_pass"));
    let memory_setting = get("memory_high_mb")
        .cloned()
        .unwrap_or(Value::String("auto".into()));

    AgentCacheBounds {
        max_size: positive_int(get("max_size")),
        idle_ttl_secs: positive_float(get("idle_ttl_secs")),
        memory_high_mb: resolve_memory_high_mb(&memory_setting),
        max_evictions_per_pass: max_evictions.unwrap_or(DEFAULT_MAX_EVICTIONS_PER_PASS),
        protect_recent: protect_parsed.unwrap_or(DEFAULT_PROTECT_RECENT),
    }
}

/// The process's anonymous resident memory in MB, or `None`. Anonymous pages are
/// where cached transcripts live, so file-backed pages are noise here. Reads
/// `/proc/self/status` (RssAnon, falling back to VmRSS).
#[cfg(target_os = "linux")]
pub fn read_anon_rss_mb() -> Option<i64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    let mut rss_anon_kib: Option<i64> = None;
    let mut rss_kib: Option<i64> = None;
    for line in status.lines() {
        if let Some(rest) = line.strip_prefix("RssAnon:") {
            rss_anon_kib = parse_status_kib(rest);
        } else if let Some(rest) = line.strip_prefix("VmRSS:") {
            rss_kib = parse_status_kib(rest);
        }
    }
    if let Some(anon) = rss_anon_kib.filter(|&v| v > 0) {
        return Some(anon / 1024);
    }
    rss_kib.filter(|&v| v > 0).map(|v| v / 1024)
}

#[cfg(not(target_os = "linux"))]
pub fn read_anon_rss_mb() -> Option<i64> {
    None
}

/// Parse the leading integer (KiB) out of a `/proc/self/status` value like
/// `   1234 kB`.
#[cfg(target_os = "linux")]
fn parse_status_kib(rest: &str) -> Option<i64> {
    rest.split_whitespace().next()?.parse::<i64>().ok()
}

/// Choose which cached sessions to shed, least-recently-used first.
///
/// `ordered_entries` must be in LRU->MRU order. The batch is capped so one pass
/// cannot stall the gateway tearing down clients. `protect_recent` is an upper
/// bound, clamped to half the cache: a handful of sessions can exhaust the
/// budget on their own, and a fixed guard would then protect the whole cache and
/// leave the gateway climbing toward the OOM killer with nothing to shed.
pub fn plan_pressure_evictions<A, F>(
    ordered_entries: Vec<(String, A)>,
    is_evictable: F,
    max_evictions: i64,
    protect_recent: i64,
) -> Vec<(String, A)>
where
    F: Fn(&str, &A) -> bool,
{
    if max_evictions <= 0 || ordered_entries.is_empty() {
        return Vec::new();
    }
    let len = ordered_entries.len();
    let protect = protect_recent.max(0).min((len / 2) as i64) as usize;
    let keep_until = len - protect;

    let mut plan: Vec<(String, A)> = Vec::new();
    for (i, (key, agent)) in ordered_entries.into_iter().enumerate() {
        if i >= keep_until {
            break; // protected MRU tail
        }
        if plan.len() as i64 >= max_evictions {
            break;
        }
        if is_evictable(&key, &agent) {
            plan.push((key, agent));
        }
    }
    plan
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_bounds_reads_section() {
        let cfg = serde_json::json!({
            "agent": {"agent_cache": {
                "max_size": 64,
                "idle_ttl_secs": 1800,
                "memory_high_mb": 2048,
                "max_evictions_per_pass": 4,
                "protect_recent": 2
            }}
        });
        let b = resolve_agent_cache_bounds(&cfg);
        assert_eq!(b.max_size, Some(64));
        assert_eq!(b.idle_ttl_secs, Some(1800.0));
        assert_eq!(b.memory_high_mb, Some(2048));
        assert_eq!(b.max_evictions_per_pass, 4);
        assert_eq!(b.protect_recent, 2);
    }

    #[test]
    fn absent_section_keeps_defaults_and_disabled_literals() {
        // No agent_cache at all: memory_high defaults to "auto" (may be Some or
        // None depending on host), the rest stay at defaults.
        let cfg = serde_json::json!({});
        let b = resolve_agent_cache_bounds(&cfg);
        assert_eq!(b.max_size, None);
        assert_eq!(b.idle_ttl_secs, None);
        assert_eq!(b.max_evictions_per_pass, DEFAULT_MAX_EVICTIONS_PER_PASS);
        assert_eq!(b.protect_recent, DEFAULT_PROTECT_RECENT);
    }

    #[test]
    fn protect_recent_zero_is_distinct_from_unset() {
        let cfg = serde_json::json!({"agent": {"agent_cache": {"protect_recent": 0}}});
        assert_eq!(resolve_agent_cache_bounds(&cfg).protect_recent, 0);
        // A bool typo stays on the default (not treated as 0).
        let cfg2 = serde_json::json!({"agent": {"agent_cache": {"protect_recent": false}}});
        assert_eq!(
            resolve_agent_cache_bounds(&cfg2).protect_recent,
            DEFAULT_PROTECT_RECENT
        );
    }

    #[test]
    fn memory_high_disable_words() {
        for w in ["off", "none", "false", "disabled", ""] {
            assert_eq!(resolve_memory_high_mb(&Value::String(w.into())), None);
        }
        assert_eq!(resolve_memory_high_mb(&Value::Bool(false)), None);
        // Literal positive number taken as-is.
        assert_eq!(resolve_memory_high_mb(&serde_json::json!(1500)), Some(1500));
        // Numeric string.
        assert_eq!(
            resolve_memory_high_mb(&Value::String("1500".into())),
            Some(1500)
        );
    }

    #[test]
    fn positive_int_truncates_float_and_rejects_nonpositive() {
        assert_eq!(positive_int(Some(&serde_json::json!(3.7))), Some(3));
        assert_eq!(positive_int(Some(&serde_json::json!(0))), None);
        assert_eq!(positive_int(Some(&serde_json::json!(-4))), None);
        assert_eq!(positive_int(Some(&serde_json::json!(true))), None);
    }

    #[test]
    fn plan_respects_cap_protect_and_evictable() {
        // 6 entries LRU->MRU: a b c d e f. protect_recent=2 -> protects e,f.
        // Candidates a,b,c,d. is_evictable rejects "b". cap=2 -> [a, c].
        let entries: Vec<(String, i32)> = ["a", "b", "c", "d", "e", "f"]
            .iter()
            .enumerate()
            .map(|(i, k)| (k.to_string(), i as i32))
            .collect();
        let plan = plan_pressure_evictions(entries, |k, _| k != "b", 2, 2);
        let keys: Vec<&str> = plan.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(keys, vec!["a", "c"]);
    }

    #[test]
    fn plan_protect_clamped_to_half() {
        // 4 entries, protect_recent=10 -> clamped to len/2 = 2, protecting c,d.
        let entries: Vec<(String, i32)> = ["a", "b", "c", "d"]
            .iter()
            .enumerate()
            .map(|(i, k)| (k.to_string(), i as i32))
            .collect();
        let plan = plan_pressure_evictions(entries, |_, _| true, 100, 10);
        let keys: Vec<&str> = plan.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(keys, vec!["a", "b"]);
    }

    #[test]
    fn plan_empty_or_zero_cap() {
        assert!(plan_pressure_evictions::<i32, _>(vec![], |_, _| true, 5, 0).is_empty());
        let entries = vec![("a".to_string(), 1)];
        assert!(plan_pressure_evictions(entries, |_, _| true, 0, 0).is_empty());
    }
}
