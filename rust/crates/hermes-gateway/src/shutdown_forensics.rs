//! Port of gateway/shutdown_forensics.py.
//!
// Public API is ahead of its callers (the shutdown signal path wires it).
#![allow(dead_code)]
//!
//! Capture context when the gateway receives SIGTERM/SIGINT so "the gateway
//! keeps dying" incidents can be diagnosed after the fact.
//!
//!  * `snapshot_shutdown_context` — a fast (<10ms), non-blocking `/proc` probe:
//!    the signal, our PID/ppid + parent summary, whether systemd is the parent,
//!    load average, a tracer (debugger) check, and takeover/planned-stop
//!    markers. Returns a structured object the signal handler logs immediately.
//!  * `spawn_async_diagnostic` — a fire-and-forget detached `ps`/`pstree`/`dmesg`
//!    walk that can't block teardown even if `/proc` is wedged.
//!  * `check_systemd_timing_alignment` — startup sanity check that systemd's
//!    `TimeoutStopSec` covers the full stop budget (a stale unit file otherwise
//!    lets systemd SIGKILL the cgroup mid-drain, logged only as `status=9`).
//!
//! Best-effort throughout; never raises, never blocks on a subprocess.

use std::path::Path;

use serde_json::{json, Map, Value};

fn signal_name(sig: Option<i32>) -> String {
    let Some(n) = sig else {
        return "UNKNOWN".to_string();
    };
    #[cfg(unix)]
    {
        let name = match n {
            libc::SIGTERM => Some("SIGTERM"),
            libc::SIGINT => Some("SIGINT"),
            libc::SIGHUP => Some("SIGHUP"),
            libc::SIGQUIT => Some("SIGQUIT"),
            libc::SIGUSR1 => Some("SIGUSR1"),
            libc::SIGUSR2 => Some("SIGUSR2"),
            _ => None,
        };
        if let Some(name) = name {
            return name.to_string();
        }
    }
    format!("signal#{n}")
}

fn read_proc_field(pid: i64, key: &str) -> Option<String> {
    let text = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    let prefix = format!("{key}:");
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix(&prefix) {
            return Some(rest.trim().to_string());
        }
    }
    None
}

fn read_proc_cmdline(pid: i64) -> Option<String> {
    let data = std::fs::read(format!("/proc/{pid}/cmdline")).ok()?;
    if data.is_empty() {
        return None;
    }
    // cmdline uses NUL separators.
    let s: Vec<u8> = data
        .iter()
        .map(|&b| if b == 0 { b' ' } else { b })
        .collect();
    Some(String::from_utf8_lossy(&s).trim().to_string())
}

/// Compact `/proc/<pid>` snapshot: pid, name, state, ppid, uid, cmdline.
fn proc_summary(pid: i64) -> Map<String, Value> {
    let mut summary = Map::new();
    summary.insert("pid".into(), json!(pid));
    if pid <= 0 {
        return summary;
    }
    if let Some(name) = read_proc_field(pid, "Name") {
        summary.insert("name".into(), json!(name));
    }
    if let Some(state) = read_proc_field(pid, "State") {
        summary.insert("state".into(), json!(state));
    }
    if let Some(ppid) = read_proc_field(pid, "PPid").and_then(|v| v.parse::<i64>().ok()) {
        summary.insert("ppid".into(), json!(ppid));
    }
    if let Some(uid) = read_proc_field(pid, "Uid") {
        // "real effective saved fs" -> the real uid.
        let real = uid.split_whitespace().next().unwrap_or(&uid).to_string();
        summary.insert("uid".into(), json!(real));
    }
    if let Some(cmdline) = read_proc_cmdline(pid) {
        if !cmdline.is_empty() {
            let truncated: String = cmdline.chars().take(300).collect();
            summary.insert("cmdline".into(), json!(truncated));
        }
    }
    summary
}

fn getppid() -> i64 {
    #[cfg(unix)]
    {
        // SAFETY: getppid is always safe.
        unsafe { libc::getppid() as i64 }
    }
    #[cfg(not(unix))]
    {
        0
    }
}

fn loadavg_1m() -> Option<f64> {
    let text = std::fs::read_to_string("/proc/loadavg").ok()?;
    text.split_whitespace().next()?.parse::<f64>().ok()
}

fn now_epoch() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

fn monotonic() -> f64 {
    use std::sync::OnceLock;
    static BASE: OnceLock<std::time::Instant> = OnceLock::new();
    BASE.get_or_init(std::time::Instant::now)
        .elapsed()
        .as_secs_f64()
}

/// Fast snapshot of who/what is asking us to shut down. Pure, never raises,
/// never blocks on subprocesses.
pub fn snapshot_shutdown_context(received_signal: Option<i32>) -> Map<String, Value> {
    let pid = std::process::id() as i64;
    let ppid = getppid();

    let mut ctx = Map::new();
    ctx.insert("ts".into(), json!(now_epoch()));
    ctx.insert("ts_monotonic".into(), json!(monotonic()));
    ctx.insert("signal".into(), json!(signal_name(received_signal)));
    ctx.insert(
        "signal_num".into(),
        received_signal.map(|n| json!(n)).unwrap_or(Value::Null),
    );
    ctx.insert("pid".into(), json!(pid));
    ctx.insert("ppid".into(), json!(ppid));
    ctx.insert("parent".into(), Value::Object(proc_summary(ppid)));
    ctx.insert("self".into(), Value::Object(proc_summary(pid)));

    let invocation_id = std::env::var("INVOCATION_ID")
        .ok()
        .filter(|v| !v.is_empty());
    if let Some(id) = &invocation_id {
        ctx.insert("systemd_invocation_id".into(), json!(id));
    }
    if let Ok(js) = std::env::var("JOURNAL_STREAM") {
        if !js.is_empty() {
            ctx.insert("systemd_journal_stream".into(), json!(js));
        }
    }
    ctx.insert(
        "under_systemd".into(),
        json!(invocation_id.is_some() || ppid == 1),
    );

    if let Some(load) = loadavg_1m() {
        ctx.insert("loadavg_1m".into(), json!(load));
    }

    // TracerPid != 0 means a debugger / strace is attached.
    if let Some(tracer) = read_proc_field(pid, "TracerPid") {
        if tracer != "0" {
            if let Ok(tp) = tracer.parse::<i64>() {
                ctx.insert("tracer_pid".into(), json!(tp));
                ctx.insert("tracer".into(), Value::Object(proc_summary(tp)));
            } else {
                ctx.insert("tracer_pid".into(), json!(tracer));
                ctx.insert("tracer".into(), Value::Null);
            }
        }
    }

    // Takeover / planned-stop markers (a marker not naming us is a smoking gun
    // for another --replace instance killing us).
    if let Ok(home) = std::env::var("HERMES_HOME") {
        if !home.is_empty() {
            let takeover = Path::new(&home).join(".gateway-takeover.json");
            if let Ok(raw) = std::fs::read_to_string(&takeover) {
                let head: String = raw.chars().take(300).collect();
                let for_self = raw.contains(&format!("\"target_pid\": {pid}"))
                    || raw.contains(&format!("'target_pid': {pid}"));
                ctx.insert("takeover_marker".into(), json!(head));
                ctx.insert("takeover_marker_for_self".into(), json!(for_self));
            }
            let planned = Path::new(&home).join(".gateway-planned-stop.json");
            if let Ok(raw) = std::fs::read_to_string(&planned) {
                let head: String = raw.chars().take(300).collect();
                ctx.insert("planned_stop_marker".into(), json!(head));
            }
        }
    }

    ctx
}

/// Fire-and-forget detached `ps`/`pstree`/`dmesg` snapshot appended to
/// `log_path`. Returns the child PID, or `None`. Never raises.
#[cfg(unix)]
pub fn spawn_async_diagnostic(
    log_path: &Path,
    signal_name: &str,
    timeout_seconds: f64,
) -> Option<u32> {
    use std::os::unix::process::CommandExt;
    if let Some(parent) = log_path.parent() {
        if std::fs::create_dir_all(parent).is_err() {
            return None;
        }
    }
    let self_pid = std::process::id();
    let script = format!(
        "echo '=== shutdown diagnostic @ {sig} ==='; \
         echo '--- date ---'; date -u +%Y-%m-%dT%H:%M:%SZ; \
         echo '--- ps auxf (top 60 by cpu) ---'; \
         ps auxf --sort=-pcpu 2>/dev/null | head -60; \
         echo '--- pstree of self ---'; \
         pstree -plau {self_pid} 2>/dev/null | head -40 || true; \
         echo '--- /proc/loadavg ---'; \
         cat /proc/loadavg 2>/dev/null || true; \
         echo '--- recent dmesg (oom/killed) ---'; \
         dmesg -T 2>/dev/null | tail -20 || journalctl --user -n 20 --no-pager 2>/dev/null | tail -20 || true; \
         echo '=== end ==='",
        sig = signal_name,
        self_pid = self_pid,
    );
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path)
        .ok()?;
    let stderr = file.try_clone().ok()?;

    let child = std::process::Command::new("timeout")
        .arg(format!("{:.0}", timeout_seconds))
        .arg("bash")
        .arg("-c")
        .arg(&script)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::from(file))
        .stderr(std::process::Stdio::from(stderr))
        .process_group(0) // detach from our process group (defense in depth)
        .spawn()
        .ok()?;
    Some(child.id())
}

#[cfg(not(unix))]
pub fn spawn_async_diagnostic(
    _log_path: &Path,
    _signal_name: &str,
    _timeout_seconds: f64,
) -> Option<u32> {
    None
}

/// Render a shutdown-context object as a single scannable log line.
pub fn format_context_for_log(ctx: &Map<String, Value>) -> String {
    let sig = ctx.get("signal").and_then(Value::as_str).unwrap_or("?");
    let parent = ctx.get("parent").and_then(Value::as_object);
    let parent_cmd = parent
        .and_then(|p| p.get("cmdline"))
        .and_then(Value::as_str)
        .unwrap_or("(unknown)");
    let parent_name = parent
        .and_then(|p| p.get("name"))
        .and_then(Value::as_str)
        .unwrap_or("?");
    let parent_pid = parent
        .and_then(|p| p.get("pid"))
        .map(|v| v.to_string())
        .unwrap_or_else(|| "?".to_string());
    let under_systemd = if ctx
        .get("under_systemd")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        "yes"
    } else {
        "no"
    };
    let load_str = ctx
        .get("loadavg_1m")
        .and_then(Value::as_f64)
        .map(|l| format!("{l:.2}"))
        .unwrap_or_else(|| "?".to_string());

    let mut extras: Vec<String> = Vec::new();
    if ctx.get("takeover_marker").is_some() {
        let for_self = ctx
            .get("takeover_marker_for_self")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        extras.push(format!(
            "takeover_marker_present={}",
            if for_self { "self" } else { "other" }
        ));
    }
    if ctx.get("planned_stop_marker").is_some() {
        extras.push("planned_stop_marker_present=yes".to_string());
    }
    if let Some(tp) = ctx.get("tracer_pid") {
        if !tp.is_null() {
            extras.push(format!("tracer_pid={tp}"));
        }
    }
    let extras_str = if extras.is_empty() {
        String::new()
    } else {
        format!(" {}", extras.join(" "))
    };
    format!(
        "signal={sig} under_systemd={under_systemd} parent_pid={parent_pid} \
         parent_name={parent_name} loadavg_1m={load_str}{extras_str} parent_cmdline={parent_cmd:?}"
    )
}

/// JSON-serialize a context object for structured ingestion. Never raises.
pub fn context_as_json(ctx: &Map<String, Value>) -> String {
    serde_json::to_string(&Value::Object(ctx.clone())).unwrap_or_else(|_| "{}".to_string())
}

/// Parse `TimeoutStopUSec=1min 30s` / `90s` style values to microseconds.
/// Covers s/ms/min/h; `None` on anything unexpected. Never raises.
pub fn parse_systemd_duration_to_us(raw: &str) -> Option<i64> {
    if raw.is_empty() {
        return None;
    }
    let unit_us = |u: &str| -> Option<i64> {
        Some(match u.to_lowercase().as_str() {
            "us" => 1,
            "ms" => 1_000,
            "s" | "sec" => 1_000_000,
            "min" => 60_000_000,
            "h" | "hr" => 3_600_000_000,
            _ => return None,
        })
    };
    let mut total_us: i64 = 0;
    let mut token = String::new();
    let mut digits = String::new();
    for ch in raw.chars().chain(std::iter::once(' ')) {
        if ch.is_ascii_digit() || ch == '.' {
            if !token.is_empty() {
                let mult = unit_us(&token)?;
                if digits.is_empty() {
                    return None;
                }
                total_us += (digits.parse::<f64>().ok()? * mult as f64) as i64;
                digits.clear();
                token.clear();
            }
            digits.push(ch);
        } else if ch.is_alphabetic() {
            token.push(ch);
        } else if !digits.is_empty() && !token.is_empty() {
            let mult = unit_us(&token)?;
            total_us += (digits.parse::<f64>().ok()? * mult as f64) as i64;
            digits.clear();
            token.clear();
        } else if !digits.is_empty() && token.is_empty() {
            // Bare number = seconds.
            total_us += (digits.parse::<f64>().ok()? * 1_000_000.0) as i64;
            digits.clear();
        }
    }
    if total_us > 0 {
        Some(total_us)
    } else {
        None
    }
}

/// Startup sanity check that systemd's `TimeoutStopSec` covers the stop budget.
/// `None` when not under systemd or undeterminable.
pub fn check_systemd_timing_alignment(
    drain_timeout: f64,
    cron_drain_timeout: f64,
) -> Option<Map<String, Value>> {
    std::env::var("INVOCATION_ID")
        .ok()
        .filter(|v| !v.is_empty())?;

    // Identify our unit name from /proc/self/cgroup (the systemd line ends in
    // "<unit>.service").
    let cgroup = std::fs::read_to_string("/proc/self/cgroup").ok()?;
    let mut unit_name: Option<String> = None;
    for line in cgroup.lines() {
        if line.contains(".service") {
            for part in line.trim().split('/').rev() {
                if part.ends_with(".service") {
                    unit_name = Some(part.to_string());
                    break;
                }
            }
            if unit_name.is_some() {
                break;
            }
        }
    }
    let unit_name = unit_name?;

    // Query systemctl (try --user first, then system).
    let mut timeout_us: Option<i64> = None;
    for flag in [Some("--user"), None] {
        let mut cmd = std::process::Command::new("systemctl");
        if let Some(f) = flag {
            cmd.arg(f);
        }
        cmd.arg("show")
            .arg(&unit_name)
            .arg("--property=TimeoutStopUSec");
        let Ok(out) = cmd.output() else { continue };
        if !out.status.success() {
            continue;
        }
        let stdout = String::from_utf8_lossy(&out.stdout);
        for line in stdout.lines() {
            if let Some(value) = line.strip_prefix("TimeoutStopUSec=") {
                let value = value.trim();
                timeout_us = if value.chars().all(|c| c.is_ascii_digit()) && !value.is_empty() {
                    value.parse::<i64>().ok()
                } else {
                    parse_systemd_duration_to_us(value)
                };
                if timeout_us.is_some() {
                    break;
                }
            }
        }
        if timeout_us.is_some() {
            break;
        }
    }
    let timeout_us = timeout_us?;

    let timeout_stop_sec = timeout_us as f64 / 1_000_000.0;
    let expected = crate::restart::resolve_systemd_timeout_stop_sec(
        drain_timeout,
        cron_drain_timeout,
        crate::restart::CRON_DRAIN_CLEANUP_RESERVE_S,
        crate::restart::SYSTEMD_STOP_HEADROOM_S,
        crate::restart::SYSTEMD_TIMEOUT_STOP_SEC_FLOOR,
    ) as f64;

    let mut out = Map::new();
    out.insert("unit".into(), json!(unit_name));
    out.insert("timeout_stop_sec".into(), json!(timeout_stop_sec));
    out.insert("drain_timeout".into(), json!(drain_timeout));
    out.insert("cron_drain_timeout".into(), json!(cron_drain_timeout));
    out.insert("expected_min".into(), json!(expected));
    out.insert("mismatch".into(), json!(timeout_stop_sec < expected));
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signal_names() {
        #[cfg(unix)]
        {
            assert_eq!(signal_name(Some(libc::SIGTERM)), "SIGTERM");
            assert_eq!(signal_name(Some(libc::SIGINT)), "SIGINT");
        }
        assert_eq!(signal_name(Some(9999)), "signal#9999");
        assert_eq!(signal_name(None), "UNKNOWN");
    }

    #[test]
    fn snapshot_has_core_fields() {
        let ctx = snapshot_shutdown_context(Some(15));
        assert_eq!(
            ctx.get("pid").and_then(Value::as_i64),
            Some(std::process::id() as i64)
        );
        assert!(ctx.get("self").is_some());
        assert!(ctx.get("parent").is_some());
        assert!(ctx.get("under_systemd").and_then(Value::as_bool).is_some());
        // Our own proc summary names this test binary.
        let self_summary = ctx.get("self").and_then(Value::as_object).unwrap();
        assert_eq!(
            self_summary.get("pid").and_then(Value::as_i64),
            Some(std::process::id() as i64)
        );
    }

    #[test]
    fn format_line_is_scannable() {
        let mut ctx = Map::new();
        ctx.insert("signal".into(), json!("SIGTERM"));
        ctx.insert("under_systemd".into(), json!(true));
        let mut parent = Map::new();
        parent.insert("pid".into(), json!(1));
        parent.insert("name".into(), json!("systemd"));
        parent.insert("cmdline".into(), json!("/sbin/init"));
        ctx.insert("parent".into(), Value::Object(parent));
        ctx.insert("loadavg_1m".into(), json!(0.42));
        let line = format_context_for_log(&ctx);
        assert!(line.contains("signal=SIGTERM"));
        assert!(line.contains("under_systemd=yes"));
        assert!(line.contains("parent_name=systemd"));
        assert!(line.contains("loadavg_1m=0.42"));
        assert!(line.contains("parent_cmdline=\"/sbin/init\""));
    }

    #[test]
    fn parse_systemd_durations() {
        assert_eq!(parse_systemd_duration_to_us("90s"), Some(90_000_000));
        assert_eq!(parse_systemd_duration_to_us("1min 30s"), Some(90_000_000));
        assert_eq!(parse_systemd_duration_to_us("2h"), Some(7_200_000_000));
        assert_eq!(parse_systemd_duration_to_us("500ms"), Some(500_000));
        // Bare number = seconds.
        assert_eq!(parse_systemd_duration_to_us("45"), Some(45_000_000));
        assert_eq!(parse_systemd_duration_to_us(""), None);
        assert_eq!(parse_systemd_duration_to_us("garbage"), None);
    }

    #[test]
    fn context_as_json_is_valid() {
        let ctx = snapshot_shutdown_context(Some(2));
        let s = context_as_json(&ctx);
        let parsed: Value = serde_json::from_str(&s).unwrap();
        assert_eq!(parsed["signal"], json!("SIGINT"));
    }

    #[test]
    fn systemd_alignment_none_without_invocation() {
        // Not under systemd -> None (no INVOCATION_ID in the test env).
        std::env::remove_var("INVOCATION_ID");
        assert!(check_systemd_timing_alignment(0.0, 30.0).is_none());
    }
}
