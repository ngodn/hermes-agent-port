//! Port of gateway/systemd_notify.py.
//!
// Public API is ahead of its callers (the systemd deploy path wires it).
#![allow(dead_code)]
//!
//! Minimal, optional systemd `sd_notify` support for the gateway. When systemd
//! runs the gateway with `Type=notify` (and optionally `WatchdogSec=`), it sets
//! `NOTIFY_SOCKET` in the environment. This sends the `READY=1`, `WATCHDOG=1`,
//! `STATUS=` and `STOPPING=1` datagrams over that socket.
//!
//! Everything here is deliberately best-effort and non-fatal: a missing socket,
//! an older platform, or a full receiver buffer must never stop the gateway from
//! starting or wedge its runtime. The Python original guards the asyncio event
//! loop's progress; the Rust port feeds the watchdog only while a spawned tokio
//! task keeps waking within its lag budget, which is the tokio-runtime analogue
//! of "the loop is still making progress".

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use tokio::task::JoinHandle;

/// Send one nonblocking sd_notify datagram when systemd configured it.
///
/// Returns `true` only when the datagram was actually sent. A missing
/// `NOTIFY_SOCKET`, an empty message, or any socket error returns `false`
/// without surfacing an error, matching the Python contract.
pub fn notify(message: &str) -> bool {
    let address = std::env::var("NOTIFY_SOCKET").unwrap_or_default();
    let address = address.trim();
    if address.is_empty() || message.is_empty() {
        return false;
    }
    send_datagram(address, message.as_bytes())
}

#[cfg(unix)]
fn send_datagram(address: &str, payload: &[u8]) -> bool {
    use std::os::unix::net::UnixDatagram;

    let Ok(sock) = UnixDatagram::unbound() else {
        return false;
    };
    // A full receiver buffer must not stall the gateway.
    if sock.set_nonblocking(true).is_err() {
        return false;
    }

    // systemd's "@name" abstract-namespace notation maps to a leading NUL in
    // the sockaddr. Path form connects to the filesystem socket directly.
    if let Some(name) = address.strip_prefix('@') {
        #[cfg(target_os = "linux")]
        {
            use std::os::linux::net::SocketAddrExt;
            use std::os::unix::net::SocketAddr;
            let Ok(addr) = SocketAddr::from_abstract_name(name.as_bytes()) else {
                return false;
            };
            if sock.connect_addr(&addr).is_err() {
                return false;
            }
        }
        #[cfg(not(target_os = "linux"))]
        {
            // Abstract sockets are Linux-only; nothing to notify elsewhere.
            let _ = name;
            return false;
        }
    } else if sock.connect(address).is_err() {
        return false;
    }

    sock.send(payload).is_ok()
}

#[cfg(not(unix))]
fn send_datagram(_address: &str, _payload: &[u8]) -> bool {
    false
}

/// systemd's configured watchdog interval in seconds, from `WATCHDOG_USEC`.
/// `None` when the watchdog is not configured or the value is unusable.
pub fn watchdog_interval_seconds() -> Option<f64> {
    let notify_socket = std::env::var("NOTIFY_SOCKET").unwrap_or_default();
    if notify_socket.trim().is_empty() {
        return None;
    }
    let raw = std::env::var("WATCHDOG_USEC").unwrap_or_default();
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    let interval = raw.parse::<f64>().ok()? / 1_000_000.0;
    if !interval.is_finite() || interval <= 0.0 {
        return None;
    }
    Some(interval)
}

/// Feed systemd's watchdog while the tokio runtime keeps making progress.
///
/// A spawned task wakes on a cadence of half the watchdog interval and sends
/// `WATCHDOG=1` only if it woke within its lag budget. If a wake is late (the
/// runtime is stalled), it stops feeding and marks itself unhealthy, letting
/// systemd's `WatchdogSec` expire and restart the service, which is the whole
/// point of the watchdog.
pub struct SystemdWatchdog {
    config_enabled: bool,
    pub interval_seconds: Option<f64>,
    lag_tolerance_seconds: Option<f64>,
    unhealthy: Arc<AtomicBool>,
    stopping: Arc<AtomicBool>,
    stopping_notified: Arc<AtomicBool>,
    task: Option<JoinHandle<()>>,
}

impl SystemdWatchdog {
    /// Build a watchdog. `config_enabled` lets the caller disable it even when
    /// systemd offers one; `lag_tolerance_seconds` overrides the default budget
    /// of 25% of the interval (floored at 0.1s).
    pub fn new(config_enabled: bool, lag_tolerance_seconds: Option<f64>) -> Self {
        Self {
            config_enabled,
            interval_seconds: watchdog_interval_seconds(),
            lag_tolerance_seconds,
            unhealthy: Arc::new(AtomicBool::new(false)),
            stopping: Arc::new(AtomicBool::new(false)),
            stopping_notified: Arc::new(AtomicBool::new(false)),
            task: None,
        }
    }

    /// True when systemd configured a watchdog and the caller left it enabled.
    pub fn enabled(&self) -> bool {
        self.config_enabled && self.interval_seconds.is_some()
    }

    pub fn unhealthy(&self) -> bool {
        self.unhealthy.load(Ordering::SeqCst)
    }

    fn lag_tolerance(&self) -> f64 {
        let interval = self.interval_seconds.unwrap_or(0.0);
        match self.lag_tolerance_seconds {
            None => (interval * 0.25).max(0.1),
            Some(v) if v.is_finite() => v.max(0.0),
            Some(_) => (interval * 0.25).max(0.1),
        }
    }

    /// Tell systemd startup completed and the gateway is ready.
    pub fn ready(&self, status: &str) -> bool {
        if !self.enabled() {
            return false;
        }
        let status = if status.is_empty() {
            "Gateway running"
        } else {
            status
        };
        let safe = status.replace('\n', " ");
        notify(&format!("READY=1\nSTATUS={safe}"))
    }

    /// Start the loop-progress sampler. Returns `true` when it started (or was
    /// already running), `false` when the watchdog is disabled.
    pub fn start(&mut self) -> bool {
        if !self.enabled() {
            return false;
        }
        if self.task.as_ref().is_some_and(|t| !t.is_finished()) {
            return true;
        }
        let interval = match self.interval_seconds {
            Some(i) => i,
            None => return false,
        };
        self.stopping.store(false, Ordering::SeqCst);
        self.unhealthy.store(false, Ordering::SeqCst);
        self.stopping_notified.store(false, Ordering::SeqCst);

        let cadence = (interval / 2.0).max(0.01);
        let tolerance = self.lag_tolerance();
        let stopping = self.stopping.clone();
        let unhealthy = self.unhealthy.clone();

        self.task = Some(tokio::spawn(async move {
            let cadence = std::time::Duration::from_secs_f64(cadence);
            let mut scheduled_at = tokio::time::Instant::now() + cadence;
            loop {
                if stopping.load(Ordering::SeqCst) || unhealthy.load(Ordering::SeqCst) {
                    return;
                }
                tokio::time::sleep_until(scheduled_at).await;
                if stopping.load(Ordering::SeqCst) {
                    return;
                }
                let now = tokio::time::Instant::now();
                let lag = now.saturating_duration_since(scheduled_at).as_secs_f64();
                if lag > tolerance {
                    unhealthy.store(true, Ordering::SeqCst);
                    notify("STATUS=watchdog unhealthy: event loop progress is late");
                    return;
                }
                notify("WATCHDOG=1");
                scheduled_at += cadence;
                if scheduled_at < now {
                    scheduled_at = now + cadence;
                }
            }
        }));
        true
    }

    /// Stop feeding systemd and emit `STOPPING=1` at most once.
    pub async fn stop(&mut self) {
        self.stopping.store(true, Ordering::SeqCst);
        if let Some(task) = self.task.take() {
            task.abort();
            let _ = task.await;
        }
        if self.enabled()
            && self
                .stopping_notified
                .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok()
        {
            notify("STOPPING=1");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Guards mutations of NOTIFY_SOCKET / WATCHDOG_USEC across tests, since the
    /// process environment is shared and these tests read it.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn notify_returns_false_without_socket() {
        let _g = ENV_LOCK.lock().unwrap();
        std::env::remove_var("NOTIFY_SOCKET");
        assert!(!notify("READY=1"));
    }

    #[test]
    fn notify_returns_false_on_empty_message() {
        let _g = ENV_LOCK.lock().unwrap();
        std::env::set_var("NOTIFY_SOCKET", "/run/does-not-exist.sock");
        assert!(!notify(""));
        std::env::remove_var("NOTIFY_SOCKET");
    }

    #[test]
    fn watchdog_interval_parses_usec() {
        let _g = ENV_LOCK.lock().unwrap();
        std::env::set_var("NOTIFY_SOCKET", "/run/notify.sock");
        std::env::set_var("WATCHDOG_USEC", "30000000"); // 30s
        assert_eq!(watchdog_interval_seconds(), Some(30.0));

        std::env::set_var("WATCHDOG_USEC", "0");
        assert_eq!(watchdog_interval_seconds(), None);

        std::env::set_var("WATCHDOG_USEC", "not-a-number");
        assert_eq!(watchdog_interval_seconds(), None);

        std::env::remove_var("WATCHDOG_USEC");
        assert_eq!(watchdog_interval_seconds(), None);

        std::env::remove_var("NOTIFY_SOCKET");
    }

    #[test]
    fn watchdog_interval_needs_notify_socket() {
        let _g = ENV_LOCK.lock().unwrap();
        std::env::remove_var("NOTIFY_SOCKET");
        std::env::set_var("WATCHDOG_USEC", "30000000");
        assert_eq!(watchdog_interval_seconds(), None);
        std::env::remove_var("WATCHDOG_USEC");
    }

    #[test]
    fn disabled_watchdog_is_inert() {
        let _g = ENV_LOCK.lock().unwrap();
        std::env::remove_var("NOTIFY_SOCKET");
        std::env::remove_var("WATCHDOG_USEC");
        let mut wd = SystemdWatchdog::new(true, None);
        assert!(!wd.enabled());
        assert!(!wd.start());
        assert!(!wd.ready("up"));
    }

    #[test]
    fn lag_tolerance_defaults_to_quarter_interval() {
        let _g = ENV_LOCK.lock().unwrap();
        std::env::set_var("NOTIFY_SOCKET", "/run/notify.sock");
        std::env::set_var("WATCHDOG_USEC", "40000000"); // 40s -> tolerance 10s
        let wd = SystemdWatchdog::new(true, None);
        assert_eq!(wd.lag_tolerance(), 10.0);
        // Explicit override wins.
        let wd2 = SystemdWatchdog::new(true, Some(2.5));
        assert_eq!(wd2.lag_tolerance(), 2.5);
        // A non-finite override falls back to the default.
        let wd3 = SystemdWatchdog::new(true, Some(f64::NAN));
        assert_eq!(wd3.lag_tolerance(), 10.0);
        std::env::remove_var("NOTIFY_SOCKET");
        std::env::remove_var("WATCHDOG_USEC");
    }
}
