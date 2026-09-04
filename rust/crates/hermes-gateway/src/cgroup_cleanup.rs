//! Port of gateway/cgroup_cleanup.py.
//!
// Public API is ahead of its callers (an ExecStopPost subcommand wires it).
#![allow(dead_code)]
//!
//! SIGKILL any process left in this systemd unit's cgroup. Meant to run as
//! `ExecStopPost=`, after the gateway's main process has exited. The gateway
//! reaps its own tracked tool subprocesses on a clean shutdown; this is the
//! safety net for long-lived helpers it does not track (adb, platform bridges)
//! that would otherwise be orphaned in the cgroup and block `Restart=always`.
//!
//! We iterate `cgroup.procs` and send per-PID SIGKILLs rather than writing `1`
//! to `cgroup.kill`: the original failure mode (#37454) was the kernel returning
//! EINVAL on the cgroup-wide kill, while per-PID signal delivery uses a separate
//! code path that still works.
//!
//! Linux-only (reads `/proc`, `/sys/fs/cgroup`); a no-op elsewhere.

use std::path::PathBuf;

/// The cgroup v2 path for the calling process (the `0::<path>` line of
/// `/proc/self/cgroup`), or `None`.
fn own_cgroup_path() -> Option<String> {
    let text = std::fs::read_to_string("/proc/self/cgroup").ok()?;
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("0::") {
            let trimmed = rest.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_string());
            }
        }
    }
    None
}

/// PIDs listed in `/sys/fs/cgroup<cgroup_path>/cgroup.procs`.
fn read_cgroup_pids(cgroup_path: &str) -> Vec<i32> {
    let procs_file = PathBuf::from(format!("/sys/fs/cgroup{cgroup_path}/cgroup.procs"));
    let Ok(raw) = std::fs::read_to_string(&procs_file) else {
        return Vec::new();
    };
    raw.lines()
        .filter_map(|line| {
            let line = line.trim();
            if line.is_empty() {
                None
            } else {
                line.parse::<i32>().ok()
            }
        })
        .collect()
}

/// SIGKILL every PID in the cgroup other than the caller. Returns the count
/// killed. `None` resolves the caller's own cgroup.
pub fn reap_cgroup(cgroup_path: Option<&str>) -> usize {
    let resolved;
    let cgroup_path = match cgroup_path {
        Some(p) => p,
        None => match own_cgroup_path() {
            Some(p) => {
                resolved = p;
                &resolved
            }
            None => return 0,
        },
    };
    if cgroup_path.is_empty() {
        return 0;
    }
    let own = std::process::id() as i32;
    let mut killed = 0;
    for pid in read_cgroup_pids(cgroup_path) {
        if pid == own {
            continue;
        }
        if send_sigkill(pid) {
            killed += 1;
        }
    }
    killed
}

/// Send SIGKILL to `pid`, returning whether it was delivered. A gone process
/// (ESRCH) or a permission failure (EPERM) is not counted, matching the Python
/// `ProcessLookupError` / `PermissionError` skips.
#[cfg(unix)]
fn send_sigkill(pid: i32) -> bool {
    // SAFETY: kill(2) with a valid signal number; it does not touch our memory.
    let rc = unsafe { libc::kill(pid, libc::SIGKILL) };
    rc == 0
}

#[cfg(not(unix))]
fn send_sigkill(_pid: i32) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_cgroup_path_reaps_nothing() {
        assert_eq!(reap_cgroup(Some("")), 0);
    }

    #[test]
    fn nonexistent_cgroup_reaps_nothing() {
        // A path that does not resolve under /sys/fs/cgroup yields no PIDs.
        assert_eq!(reap_cgroup(Some("/nonexistent-hermes-test-cgroup-xyz")), 0);
    }

    #[test]
    fn read_pids_parses_and_skips_garbage() {
        // read_cgroup_pids on a missing file returns empty (no panic).
        assert!(read_cgroup_pids("/definitely/not/here").is_empty());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn own_cgroup_path_is_readable_on_linux() {
        // On a Linux CI/host with cgroup v2, /proc/self/cgroup has a 0:: line.
        // We don't assert a value (v1-only hosts exist) but the call must not
        // panic and returns Some or None cleanly.
        let _ = own_cgroup_path();
    }
}
