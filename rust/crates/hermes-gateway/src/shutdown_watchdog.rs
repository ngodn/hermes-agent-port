//! Port of gateway/shutdown_watchdog.py.
//!
// Public API is ahead of its callers: the runner arms these backstops, and the
// runner is not ported yet.
#![allow(dead_code)]
//!
//! Out-of-loop shutdown backstops. When the async runtime freezes mid-drain,
//! every recovery path that needs that same runtime is structurally unable to
//! fire, and a service manager's KeepAlive only revives a *dead* process, so a
//! wedged-but-alive gateway sits as a zombie. This module provides the pieces
//! that do not depend on the runtime:
//!
//! 1. [`arm_shutdown_watchdog`], a plain OS-thread hard-exit backstop armed at
//!    stop(). If shutdown has not completed within the leash it dumps
//!    diagnostics and hard-exits so the service manager can revive the process.
//! 2. The loop-liveness heartbeat file at `<HERMES_HOME>/state/gateway.heartbeat`
//!    ([`write_loop_heartbeat`]) so external supervision can tell "process
//!    alive" from "loop frozen".
//! 3. The path helpers for the heartbeat, the PID-suffixed loop-tick witness
//!    socket, and the watchdog dump.
//!
//! FAITHFULNESS NOTE on the diagnostic dump. Python calls
//! `faulthandler.dump_traceback(all_threads=True)`, which walks every thread's
//! Python stack. Rust has no equivalent: `std::backtrace::Backtrace` captures
//! only the calling thread, and there is no portable way to unwind other
//! threads from inside the process. [`write_watchdog_dump`] therefore writes the
//! same JSON metadata header (the part supervisors actually parse) plus a
//! backtrace of the watchdog thread, and records that the all-thread dump is
//! unavailable. The header, the file location, the append semantics, and the
//! stderr mirror are byte-compatible with Python.
//!
//! DEFERRED (reported, not stubbed): `start_loop_liveness_watchdog`,
//! `_arm_loop_floor_timer` and the `_tick_socket_handler` witness socket exist
//! specifically because a Python asyncio loop can freeze while its selector
//! sleeps. Their Rust analog is a tokio-runtime liveness probe, which is a
//! different mechanism (systemd_notify.rs already carries a watchdog that only
//! feeds while the runtime makes progress). They land with the runner, which is
//! what owns the runtime. Only [`get_loop_tick_socket_path`] is ported here,
//! since the path shape is part of the on-disk contract.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{json, Map, Value};

/// Extra leash beyond the drain timeout so a slow-but-progressing drain is not
/// cut short.
pub const DEFAULT_SHUTDOWN_WATCHDOG_GRACE_S: f64 = 60.0;
pub const DEFAULT_HEARTBEAT_INTERVAL_S: f64 = 30.0;
pub const DEFAULT_LOOP_FLOOR_TIMER_INTERVAL_S: f64 = 5.0;
pub const DEFAULT_LOOP_WATCHDOG_INTERVAL_S: f64 = 30.0;
pub const DEFAULT_LOOP_WATCHDOG_TIMEOUT_S: f64 = 10.0;
/// 3 sustained misses (~90-120s of loop block) escalate.
pub const DEFAULT_LOOP_WATCHDOG_MAX_STRIKES: i64 = 3;

const HEARTBEAT_RELATIVE: [&str; 2] = ["state", "gateway.heartbeat"];
const WATCHDOG_DUMP_RELATIVE: [&str; 2] = ["logs", "gateway-shutdown-watchdog.log"];

/// HERMES_HOME for process-level identity files, ignoring profile overrides.
///
/// Mirrors `_process_hermes_home`: the raw env var wins when set and non-blank
/// (after strip), otherwise fall back to the resolved home.
fn process_hermes_home() -> PathBuf {
    if let Ok(val) = std::env::var("HERMES_HOME") {
        let trimmed = val.trim();
        if !trimmed.is_empty() {
            return PathBuf::from(trimmed);
        }
    }
    crate::config_file::hermes_home()
}

fn base_or_process_home(home: Option<&Path>) -> PathBuf {
    match home {
        Some(h) => h.to_path_buf(),
        None => process_hermes_home(),
    }
}

/// `<HERMES_HOME>/state/gateway.heartbeat`.
pub fn get_loop_heartbeat_path(home: Option<&Path>) -> PathBuf {
    let mut p = base_or_process_home(home);
    for seg in HEARTBEAT_RELATIVE {
        p.push(seg);
    }
    p
}

/// The loop-scheduling witness socket for `pid`:
/// `<HERMES_HOME>/state/gateway.loop-tick.<pid>.sock`.
///
/// PID-suffixed so a leftover node from a previous process can never be
/// mistaken for this gateway's witness.
pub fn get_loop_tick_socket_path(home: Option<&Path>, pid: Option<u32>) -> PathBuf {
    let pid = pid.unwrap_or_else(std::process::id);
    let mut p = base_or_process_home(home);
    p.push("state");
    p.push(format!("gateway.loop-tick.{pid}.sock"));
    p
}

/// The diagnostic dump path for a fired watchdog.
pub fn get_shutdown_watchdog_dump_path(home: Option<&Path>) -> PathBuf {
    let mut p = base_or_process_home(home);
    for seg in WATCHDOG_DUMP_RELATIVE {
        p.push(seg);
    }
    p
}

/// Seconds since an arbitrary fixed point, standing in for `time.monotonic()`.
fn monotonic() -> f64 {
    use std::sync::OnceLock;
    static BASE: OnceLock<Instant> = OnceLock::new();
    BASE.get_or_init(Instant::now).elapsed().as_secs_f64()
}

/// Atomically (tmp + rename) write `payload` as compact JSON. Best effort.
fn atomic_json_write(path: &Path, payload: &Value) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension(format!("tmp.{}", std::process::id()));
    let body = serde_json::to_string(payload).unwrap_or_else(|_| "{}".to_string());
    std::fs::write(&tmp, body.as_bytes())?;
    match std::fs::rename(&tmp, path) {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            Err(e)
        }
    }
}

/// Atomically rewrite the loop-liveness heartbeat file.
///
/// `start_time` is the gateway process start (epoch seconds) so supervisors can
/// detect PID reuse. Best effort: never returns an error, matching Python's
/// swallow-and-log. Embeds a cheap memory sample so the heartbeat doubles as
/// pre-death telemetry (after an unclean death it is the closest surviving
/// record of memory pressure).
pub fn write_loop_heartbeat(
    pid: Option<u32>,
    start_time: Option<f64>,
    home: Option<&Path>,
    extra: Option<&Map<String, Value>>,
) -> PathBuf {
    let path = get_loop_heartbeat_path(home);
    let mut payload = Map::new();
    payload.insert(
        "pid".to_string(),
        json!(pid.unwrap_or_else(std::process::id)),
    );
    payload.insert(
        "updated_at".to_string(),
        json!(chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Micros, false)),
    );
    payload.insert("monotonic".to_string(), json!(monotonic()));
    if let Some(st) = start_time {
        payload.insert("start_time".to_string(), json!(st));
    }
    let mem = crate::lifecycle_ledger::sample_memory();
    if !mem.is_empty() {
        payload.insert("mem".to_string(), Value::Object(mem));
    }
    if let Some(extra) = extra {
        for (k, v) in extra {
            payload.insert(k.clone(), v.clone());
        }
    }
    if atomic_json_write(&path, &Value::Object(payload)).is_err() {
        tracing::debug!("Failed to write gateway loop heartbeat");
    }
    path
}

/// The wall-clock leash for the shutdown watchdog thread.
///
/// Mirrors `resolve_shutdown_watchdog_delay`: each input is clamped at 0, and a
/// non-finite/unparseable drain becomes 0 while a bad grace falls back to the
/// default. (In Python the `except (TypeError, ValueError)` arms catch a
/// non-numeric argument; the Rust signature is already `f64`, so the equivalent
/// bad input is NaN, which `max` would propagate.)
pub fn resolve_shutdown_watchdog_delay(drain_timeout: f64, grace_s: f64) -> f64 {
    let drain = if drain_timeout.is_nan() {
        0.0
    } else {
        drain_timeout.max(0.0)
    };
    let grace = if grace_s.is_nan() {
        DEFAULT_SHUTDOWN_WATCHDOG_GRACE_S
    } else {
        grace_s.max(0.0)
    };
    drain + grace
}

/// Best-effort diagnostic dump before a hard exit.
///
/// Appends a JSON metadata header (the machine-readable part, identical to
/// Python's) plus a stack trace, then mirrors a one-line notice to stderr so
/// journald/launchd capture it even when the file write failed (a wedged disk
/// is one of the failure modes this exists for).
///
/// See the module doc: the trace is this thread's only, because Rust cannot
/// dump every thread's stack the way `faulthandler` does.
pub fn write_watchdog_dump(dump_path: &Path, delay_s: f64, snapshot: Option<&Value>) {
    use std::io::Write;

    if let Some(parent) = dump_path.parent() {
        if std::fs::create_dir_all(parent).is_err() {
            return;
        }
    }

    let header = json!({
        "event": "shutdown_watchdog_fired",
        "pid": std::process::id(),
        "delay_s": delay_s,
        "fired_at": chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Micros, false),
        "snapshot": snapshot.cloned().unwrap_or_else(|| json!({})),
    });

    if let Ok(mut fh) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dump_path)
    {
        let _ = writeln!(fh, "{header}");
        let _ = writeln!(fh, "--- backtrace (watchdog thread) ---");
        let _ = writeln!(
            fh,
            "(all-thread dump unavailable in the Rust port; Python used faulthandler)"
        );
        let bt = std::backtrace::Backtrace::force_capture();
        let _ = writeln!(fh, "{bt}");
        let _ = writeln!(fh, "--- end dump ---");
        let _ = fh.flush();
    }

    let mut err = std::io::stderr();
    let _ = writeln!(
        err,
        "Gateway shutdown watchdog fired after {:.0}s (pid={}); dumping thread stack.",
        delay_s,
        std::process::id()
    );
    let _ = err.flush();
}

/// Handle for a armed shutdown watchdog: set it to disarm.
///
/// Stands in for Python's `threading.Event` (`done_event`). Returned from
/// [`arm_shutdown_watchdog`] so the caller can disarm on successful shutdown.
#[derive(Clone, Debug, Default)]
pub struct DoneFlag(Arc<AtomicBool>);

impl DoneFlag {
    pub fn new() -> Self {
        Self(Arc::new(AtomicBool::new(false)))
    }
    /// Disarm: the watchdog thread exits quietly.
    pub fn set(&self) {
        self.0.store(true, Ordering::SeqCst);
    }
    pub fn is_set(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

/// Arm a daemon-thread hard-exit backstop for a wedged shutdown path.
///
/// If the returned flag is set before `delay_s` elapses, the thread exits
/// quietly (shutdown completed). Otherwise it dumps diagnostics and hard-exits
/// with `exit_code`. Never panics. A non-positive delay arms nothing.
///
/// Before exiting it releases the PID file and the runtime lock FIRST (locks
/// must never be stranded), then records the exit reason so the next boot's
/// unclean-death detector reports "shutdown watchdog fired" rather than a
/// SIGKILL/OOM. `libc::_exit` is used rather than `std::process::exit` because,
/// like Python's `os._exit`, it must not run atexit/destructor machinery that
/// the wedged path may be blocked in.
pub fn arm_shutdown_watchdog(
    delay_s: f64,
    done: Option<DoneFlag>,
    snapshot_fn: Option<Box<dyn Fn() -> Value + Send>>,
    exit_code: i32,
    dump_path: Option<PathBuf>,
    name: &str,
) -> DoneFlag {
    let done = done.unwrap_or_default();
    let delay = if delay_s.is_nan() {
        DEFAULT_SHUTDOWN_WATCHDOG_GRACE_S
    } else {
        delay_s.max(0.0)
    };
    if delay <= 0.0 {
        return done;
    }

    let thread_done = done.clone();
    let builder = std::thread::Builder::new().name(name.to_string());
    let spawned = builder.spawn(move || {
        // Wait in short chunks so a late disarm is observed promptly instead of
        // sleeping out the full remaining leash.
        let deadline = Instant::now() + Duration::from_secs_f64(delay);
        while Instant::now() < deadline {
            if thread_done.is_set() {
                return;
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            std::thread::sleep(remaining.min(Duration::from_secs(1)));
        }
        if thread_done.is_set() {
            return;
        }

        let snapshot: Option<Value> = snapshot_fn.map(|f| {
            // A panicking snapshot must not take the watchdog down; mirror
            // Python recording the failure instead.
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(f))
                .unwrap_or_else(|_| json!({"snapshot_error": "snapshot_fn panicked"}))
        });

        let target = dump_path.unwrap_or_else(|| get_shutdown_watchdog_dump_path(None));
        write_watchdog_dump(&target, delay, snapshot.as_ref());

        tracing::error!(
            delay_s = delay,
            dump = %target.display(),
            "Shutdown watchdog fired; forcing process exit (drain path appears wedged)"
        );

        // Release the PID file + runtime lock BEFORE anything else so locks are
        // never stranded, then record why we died.
        crate::status::remove_pid_file();
        crate::status::release_gateway_runtime_lock();
        crate::lifecycle_ledger::mark_exited(Some(exit_code as i64), "shutdown_watchdog", None);

        // SAFETY: _exit is async-signal-safe and deliberately skips destructors
        // and atexit handlers, matching Python's os._exit on a wedged path.
        unsafe { libc::_exit(exit_code) };
    });

    if spawned.is_err() {
        tracing::debug!("Failed to arm shutdown watchdog");
    }
    done
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_home(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "hermes_swd_{}_{}_{}",
            tag,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn paths_match_python_shapes() {
        let home = PathBuf::from("/tmp/h");
        assert_eq!(
            get_loop_heartbeat_path(Some(&home)),
            PathBuf::from("/tmp/h/state/gateway.heartbeat")
        );
        assert_eq!(
            get_shutdown_watchdog_dump_path(Some(&home)),
            PathBuf::from("/tmp/h/logs/gateway-shutdown-watchdog.log")
        );
        assert_eq!(
            get_loop_tick_socket_path(Some(&home), Some(4242)),
            PathBuf::from("/tmp/h/state/gateway.loop-tick.4242.sock")
        );
    }

    #[test]
    fn resolve_delay_matches_python() {
        // Golden values from Python resolve_shutdown_watchdog_delay.
        assert_eq!(resolve_shutdown_watchdog_delay(30.0, 60.0), 90.0);
        assert_eq!(resolve_shutdown_watchdog_delay(0.0, 60.0), 60.0);
        // Negatives clamp to 0 rather than subtracting.
        assert_eq!(resolve_shutdown_watchdog_delay(-5.0, 60.0), 60.0);
        assert_eq!(resolve_shutdown_watchdog_delay(30.0, -5.0), 30.0);
        // A NaN grace falls back to the default; a NaN drain is treated as 0.
        assert_eq!(
            resolve_shutdown_watchdog_delay(30.0, f64::NAN),
            30.0 + DEFAULT_SHUTDOWN_WATCHDOG_GRACE_S
        );
        assert_eq!(resolve_shutdown_watchdog_delay(f64::NAN, 60.0), 60.0);
    }

    #[test]
    fn heartbeat_writes_expected_payload() {
        let home = temp_home("hb");
        let mut extra = Map::new();
        extra.insert("phase".to_string(), json!("draining"));
        let path = write_loop_heartbeat(Some(1234), Some(1000.5), Some(&home), Some(&extra));
        assert_eq!(path, home.join("state").join("gateway.heartbeat"));

        let text = std::fs::read_to_string(&path).unwrap();
        let v: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(v["pid"], json!(1234));
        assert_eq!(v["start_time"], json!(1000.5));
        assert_eq!(v["phase"], json!("draining"));
        assert!(v["updated_at"].is_string());
        assert!(v["monotonic"].is_number());
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn heartbeat_defaults_pid_and_omits_start_time() {
        let home = temp_home("hb2");
        let path = write_loop_heartbeat(None, None, Some(&home), None);
        let v: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(v["pid"], json!(std::process::id()));
        assert!(v.get("start_time").is_none());
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn watchdog_dump_appends_parsable_header() {
        let home = temp_home("dump");
        let dump = home.join("logs").join("gateway-shutdown-watchdog.log");
        write_watchdog_dump(&dump, 90.0, Some(&json!({"stage": "drain"})));
        write_watchdog_dump(&dump, 91.0, None);

        let text = std::fs::read_to_string(&dump).unwrap();
        // Each fire appends its own JSON header line, so both survive.
        let headers: Vec<Value> = text
            .lines()
            .filter(|l| l.starts_with('{'))
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(headers.len(), 2);
        assert_eq!(headers[0]["event"], json!("shutdown_watchdog_fired"));
        assert_eq!(headers[0]["delay_s"], json!(90.0));
        assert_eq!(headers[0]["snapshot"]["stage"], json!("drain"));
        assert_eq!(headers[1]["delay_s"], json!(91.0));
        // An omitted snapshot becomes {} rather than null, as in Python.
        assert_eq!(headers[1]["snapshot"], json!({}));
        assert!(text.contains("--- end dump ---"));
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn non_positive_delay_arms_nothing() {
        // Must return promptly and never spawn a thread that could exit us.
        let d = arm_shutdown_watchdog(0.0, None, None, 1, None, "test-noop");
        assert!(!d.is_set());
        let d2 = arm_shutdown_watchdog(-3.0, None, None, 1, None, "test-noop2");
        assert!(!d2.is_set());
    }

    #[test]
    fn disarm_before_deadline_exits_quietly() {
        // A long leash we immediately disarm: if the thread ignored the flag it
        // would hard-exit the test process, so passing IS the assertion.
        let done = arm_shutdown_watchdog(30.0, None, None, 1, None, "test-disarm");
        done.set();
        std::thread::sleep(Duration::from_millis(50));
        assert!(done.is_set());
    }
}
