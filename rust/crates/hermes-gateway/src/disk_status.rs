//! Port of gateway/disk_status.py.
//!
// Public API is ahead of its callers (the /api/status disk block wires it).
#![allow(dead_code)]
//!
//! Disk-usage rollup for `/api/status`. A hosted agent can fill its data volume
//! completely (SQLite writes failing, session persistence dead, config saves
//! lost) while its dashboard still looks healthy. Readiness already probes disk,
//! but that is a component verdict, not user-facing telemetry; this produces the
//! public block the dashboard and NAS availability sweep consume.
//!
//! Disk is sampled live via `statvfs` (the same call the readiness probe makes),
//! so there is no staleness dimension. Everything is best-effort and read-only:
//! an unreadable filesystem degrades to `pressure="unknown"` rather than raising.

use serde::Serialize;
use std::path::Path;

// Disk-pressure thresholds. Percent alone misleads both ways: 90% used on a
// 100 GB volume leaves a comfortable 10 GB, while 50% on a tiny volume can be
// one image download from write failures. So the percent triggers are gated on
// absolute headroom being low too, and a hard absolute floor applies regardless
// of size (below it, SQLite journaling and config writes are at genuine risk).
const CRITICAL_FREE_MB: i64 = 256; // < 256 MB free: critical on any volume
const CRITICAL_PERCENT: f64 = 95.0; // >= 95% used AND < 1 GB free: critical
const CRITICAL_HEADROOM_MB: i64 = 1024;
const ELEVATED_FREE_MB: i64 = 512; // < 512 MB free: elevated on any volume
const ELEVATED_PERCENT: f64 = 85.0; // >= 85% used AND < 4 GB free: elevated
const ELEVATED_HEADROOM_MB: i64 = 4096;

const BYTES_PER_MB: u64 = 1024 * 1024;

/// The `disk` block for `/api/status`. `None` fields plus `pressure="unknown"`
/// when the filesystem could not be sampled.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct DiskStatus {
    pub pressure: String,
    pub total_mb: Option<i64>,
    pub free_mb: Option<i64>,
    pub used_percent: Option<f64>,
}

impl DiskStatus {
    fn unknown() -> Self {
        Self {
            pressure: "unknown".to_string(),
            total_mb: None,
            free_mb: None,
            used_percent: None,
        }
    }
}

/// Reject negative values (Python guards `bool`/non-int/`< 0`), returning the
/// MB count or `None`.
fn coerce_mb(value: i64) -> Option<i64> {
    if value < 0 {
        None
    } else {
        Some(value)
    }
}

/// Map free/total MB to `ok`/`elevated`/`critical`, or `unknown` when the
/// sample is missing or malformed. The caller must not read "we could not read
/// it" as "disk is fine".
pub fn classify_disk_pressure(free_mb: i64, total_mb: i64) -> String {
    let (Some(free), Some(total)) = (coerce_mb(free_mb), coerce_mb(total_mb)) else {
        return "unknown".to_string();
    };
    if total <= 0 {
        return "unknown".to_string();
    }
    let used_percent = (1.0 - free as f64 / total as f64) * 100.0;
    if free < CRITICAL_FREE_MB || (used_percent >= CRITICAL_PERCENT && free < CRITICAL_HEADROOM_MB)
    {
        return "critical".to_string();
    }
    if free < ELEVATED_FREE_MB || (used_percent >= ELEVATED_PERCENT && free < ELEVATED_HEADROOM_MB)
    {
        return "elevated".to_string();
    }
    "ok".to_string()
}

/// A raw disk-usage sample in bytes (Python `shutil.disk_usage`: `total` uses
/// `f_blocks`, `free` uses `f_bavail`, `used` is `total - f_bfree*frsize`).
struct Usage {
    total: u64,
    free: u64,
    used: u64,
}

#[cfg(unix)]
fn disk_usage(path: &Path) -> Option<Usage> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let c_path = CString::new(path.as_os_str().as_bytes()).ok()?;
    // SAFETY: statvfs writes into a zeroed buffer we own; c_path is a valid
    // NUL-terminated C string for the duration of the call.
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::statvfs(c_path.as_ptr(), &mut stat) };
    if rc != 0 {
        return None;
    }
    // Fragment size is the correct multiplier for f_blocks/f_bavail (matches
    // CPython's shutil.disk_usage on POSIX).
    let frsize = stat.f_frsize as u64;
    let total = stat.f_blocks as u64 * frsize;
    let free = stat.f_bavail as u64 * frsize;
    let used = (stat.f_blocks as u64 - stat.f_bfree as u64) * frsize;
    Some(Usage { total, free, used })
}

#[cfg(not(unix))]
fn disk_usage(_path: &Path) -> Option<Usage> {
    None
}

/// Build the `disk` block. `home` scopes the sample to a profile's HERMES_HOME;
/// on hosted images every profile shares one data volume so the answer is the
/// same, but scoping keeps the contract identical to the memory block's. Always
/// returns a value; an unreadable/unmounted filesystem yields `unknown`.
pub fn collect_disk_status(home: Option<&Path>) -> DiskStatus {
    let home_buf;
    let home = match home {
        Some(h) => h,
        None => {
            home_buf = crate::config_file::hermes_home();
            &home_buf
        }
    };

    let Some(usage) = disk_usage(home) else {
        return DiskStatus::unknown();
    };
    if usage.total == 0 {
        return DiskStatus::unknown();
    }
    let total_mb = (usage.total / BYTES_PER_MB) as i64;
    let free_mb = (usage.free / BYTES_PER_MB) as i64;
    let used_percent = round1((usage.used as f64 / usage.total as f64) * 100.0);
    DiskStatus {
        pressure: classify_disk_pressure(free_mb, total_mb),
        total_mb: Some(total_mb),
        free_mb: Some(free_mb),
        used_percent: Some(used_percent),
    }
}

/// Round to one decimal place (Python `round(x, 1)`).
fn round1(x: f64) -> f64 {
    (x * 10.0).round() / 10.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn healthy_volume_is_ok() {
        // 50 GB free of 100 GB.
        assert_eq!(classify_disk_pressure(50_000, 100_000), "ok");
    }

    #[test]
    fn hard_floor_trips_critical_on_large_volume() {
        // 200 MB free on a huge volume: below the 256 MB hard floor.
        assert_eq!(classify_disk_pressure(200, 1_000_000), "critical");
    }

    #[test]
    fn percent_gated_on_headroom() {
        // 96% used but 3 GB free: not critical (headroom > 1 GB), and 3 GB < 4 GB
        // headroom at 85%+ used -> elevated.
        assert_eq!(classify_disk_pressure(3000, 75_000), "elevated");
        // 96% used AND < 1 GB free -> critical.
        assert_eq!(classify_disk_pressure(800, 20_000), "critical");
    }

    #[test]
    fn elevated_free_floor() {
        // 400 MB free of 5 GB: ~92% used (below the 95% critical percent) and
        // above the 256 MB critical floor, but below the 512 MB elevated floor
        // -> elevated. (On a huge volume the same 400 MB would be critical, since
        // it is then >95% used with under 1 GB free.)
        assert_eq!(classify_disk_pressure(400, 5000), "elevated");
    }

    #[test]
    fn unknown_on_bad_sample() {
        assert_eq!(classify_disk_pressure(-1, 100), "unknown");
        assert_eq!(classify_disk_pressure(10, 0), "unknown");
        assert_eq!(classify_disk_pressure(10, -5), "unknown");
    }

    #[test]
    fn collect_returns_a_real_sample_for_a_real_dir() {
        // The temp dir always exists and is on a mounted fs; we should get a
        // populated block with a known pressure enum.
        let st = collect_disk_status(Some(&std::env::temp_dir()));
        assert!(st.total_mb.is_some());
        assert!(st.free_mb.is_some());
        assert!(st.used_percent.is_some());
        assert!(["ok", "elevated", "critical"].contains(&st.pressure.as_str()));
    }
}
