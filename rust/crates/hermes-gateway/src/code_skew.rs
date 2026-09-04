//! Port of gateway/code_skew.py.
//!
// Public API is ahead of its callers (risky callers like `/model` switching
// will refuse when this reports drift); allow it until they are wired.
#![allow(dead_code)]
//!
//! Detect when the gateway is running stale code after a hot `git pull`.
//!
//! The gateway is a single long-lived process. If the checkout is updated
//! underneath it (a manual `git pull`, or the window before `hermes update`'s
//! graceful restart fires), a first-time lazy load on a new code path can
//! resolve a freshly-pulled module against a stale cached dependency. We
//! snapshot the checkout revision at gateway startup and compare on demand, so
//! risky callers can refuse with a clear "restart the gateway" message instead
//! of crashing on a cryptic error.
//!
//! If the revision can't be read (non-git install, IO error), the boot
//! snapshot stays `None` and skew detection no-ops. It never produces a false
//! positive.
//!
//! The Python module reads the checkout fingerprint via
//! `hermes_cli.main._read_git_revision_fingerprint`. That reader is a pure
//! `.git`-file parser (no GatewayRunner / adapter / agent-core coupling), so it
//! is ported here as the private `read_git_revision_fingerprint` /
//! `read_packed_ref` helpers rather than reaching across crates.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

// The boot-time fingerprint snapshot. `None` means "not recorded yet" or
// "could not be read"; the Python module cannot tell those apart either, and
// both make skew detection no-op, so a single `Option` is faithful.
static BOOT_FINGERPRINT: Mutex<Option<String>> = Mutex::new(None);

/// The repo root whose `.git` we fingerprint. Python takes the parent of the
/// `gateway/` package (`Path(__file__).resolve().parent.parent`). The Rust
/// crate lives at `rust/crates/hermes-gateway`, so the repo root is three
/// ancestors up from the crate dir. This is a compile-time path: on a
/// deployed binary that path may not exist, which just means `fingerprint`
/// returns `None` and detection no-ops (the same "never a false positive"
/// fallback the Python guarantees).
fn project_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(3)
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")))
}

/// Current checkout fingerprint, or `None` when it can't be read. Mirrors the
/// Python `_fingerprint()` (which wraps the read in a broad try/except and
/// returns `None`).
fn fingerprint() -> Option<String> {
    read_git_revision_fingerprint(&project_root())
}

/// Snapshot the checkout revision at gateway startup (idempotent). Like the
/// Python, it only reads while the slot is still empty, so a boot where the
/// read failed will retry on a later call.
pub fn record_boot_fingerprint() {
    let mut guard = BOOT_FINGERPRINT.lock().unwrap();
    if guard.is_none() {
        *guard = fingerprint();
    }
}

/// Return `(boot_rev, disk_rev)` short labels if the checkout drifted since
/// boot, else `None`.
pub fn detect_code_skew() -> Option<(String, String)> {
    let boot = BOOT_FINGERPRINT.lock().unwrap().clone();
    compute_skew(boot.as_deref(), fingerprint())
}

// The pure comparison behind `detect_code_skew`, split out so the state machine
// is testable without touching the process-global slot or the real checkout.
fn compute_skew(boot: Option<&str>, current: Option<String>) -> Option<(String, String)> {
    let boot = boot?;
    let current = current?;
    if current == boot {
        return None;
    }
    Some((short(boot), short(&current)))
}

/// Render a `git:<ref>:<sha>` fingerprint as a compact label.
fn short(fingerprint: &str) -> String {
    // Python: `fingerprint.rsplit(":", 1)[-1]`. With no ':' that is the whole
    // string; `rsplit_once` returning `None` gives the same fallback.
    let sha = fingerprint
        .rsplit_once(':')
        .map(|(_, tail)| tail)
        .unwrap_or(fingerprint);
    // len() in Python is a char count; count chars, not bytes.
    if !sha.is_empty() && sha != "unresolved" && sha.chars().count() > 10 {
        return sha.chars().take(10).collect();
    }
    // Python `sha or fingerprint`: an empty sha (e.g. a trailing ':') falls
    // back to the full fingerprint.
    if sha.is_empty() {
        fingerprint.to_string()
    } else {
        sha.to_string()
    }
}

// Read a file as UTF-8, replacing invalid bytes. Mirrors Python's
// `read_text(encoding="utf-8", errors="replace")`. Returns an IO error so
// callers can distinguish "could not read" (the Python OSError paths) from a
// successful read.
fn read_text_lossy(path: &Path) -> std::io::Result<String> {
    std::fs::read(path).map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
}

/// Look up a ref in `<common_dir>/packed-refs` without spawning git. Lines look
/// like `<sha> <ref>` with optional `^<sha>` peel lines and `#` comments.
fn read_packed_ref(common_dir: &Path, want_ref: &str) -> Option<String> {
    // OSError (missing / unreadable) -> None.
    let text = read_text_lossy(&common_dir.join("packed-refs")).ok()?;
    for line in text.lines() {
        if line.is_empty() || line.starts_with('#') || line.starts_with('^') {
            continue;
        }
        // Python `line.split(" ", 1)`: split on one space, at most once.
        let mut parts = line.splitn(2, ' ');
        let sha = parts.next().unwrap_or("");
        if let Some(rest) = parts.next() {
            if rest.trim() == want_ref {
                return Some(sha.trim().to_string());
            }
        }
    }
    None
}

/// Return a cheap checkout fingerprint without spawning git, or `None` when the
/// checkout can't be read. Faithful port of
/// `hermes_cli.main._read_git_revision_fingerprint`.
fn read_git_revision_fingerprint(repo_root: &Path) -> Option<String> {
    let mut git_dir = repo_root.join(".git");
    // A `.git` file (worktree / submodule) points at the real gitdir.
    if git_dir.is_file() {
        // Reading the `.git` file itself can fail with OSError -> None.
        let text = read_text_lossy(&git_dir).ok()?;
        for line in text.lines() {
            // Python `line.partition(":")`: split on the first ':' (no match
            // leaves value empty).
            let (key, value) = line.split_once(':').unwrap_or((line, ""));
            if key.trim() == "gitdir" && !value.trim().is_empty() {
                // Python resolves this path; we join and let the filesystem
                // resolve any `..`/symlinks on read, which yields the same
                // files. The fingerprint we return carries refs, not paths, so
                // the exact normalized form is never observed.
                git_dir = repo_root.join(value.trim());
                break;
            }
        }
    }

    // Worktrees keep HEAD in the per-worktree gitdir but pack their refs in the
    // main repo's gitdir, referenced via `commondir`. Resolve it up front so
    // packed-refs lookups hit the right file.
    let mut common_dir = git_dir.clone();
    let commondir_file = git_dir.join("commondir");
    if commondir_file.exists() {
        // Python swallows OSError here (keeps common_dir = git_dir).
        if let Ok(text) = read_text_lossy(&commondir_file) {
            let rel = text.trim();
            if !rel.is_empty() {
                common_dir = git_dir.join(rel);
            }
        }
    }

    // A missing / unreadable HEAD is an OSError -> None.
    let head = read_text_lossy(&git_dir.join("HEAD")).ok()?;
    let head = head.trim();

    if let Some(rest) = head.strip_prefix("ref:") {
        // Python: `head.split(":", 1)[1].strip()`. Everything after the first
        // ':' (the "ref" prefix), stripped.
        let want_ref = rest.trim();
        // Loose refs may live in the worktree gitdir OR the common dir.
        for candidate in [&git_dir, &common_dir] {
            let ref_file = candidate.join(want_ref);
            if ref_file.exists() {
                // A ref that exists but fails to read is an OSError -> None.
                let sha = read_text_lossy(&ref_file).ok()?;
                return Some(format!("git:{}:{}", want_ref, sha.trim()));
            }
        }
        if let Some(packed_sha) = read_packed_ref(&common_dir, want_ref) {
            return Some(format!("git:{}:{}", want_ref, packed_sha));
        }
        // Ref name is known but unresolved: still stable across launches.
        return Some(format!("git:{}:unresolved", want_ref));
    }

    // Detached HEAD: the raw sha.
    Some(format!("git:HEAD:{}", head))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "hermes_code_skew_{}_{}_{}",
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

    fn write(root: &Path, rel: &str, content: &str) {
        let p = root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, content).unwrap();
    }

    // Golden values locked against the real Python `_short`.
    #[test]
    fn short_matches_python_golden() {
        assert_eq!(
            short("git:refs/heads/main:0123456789abcdef0123"),
            "0123456789"
        );
        assert_eq!(short("git:refs/heads/main:unresolved"), "unresolved");
        assert_eq!(short("git:refs/heads/main:short"), "short");
        assert_eq!(short("git:HEAD:"), "git:HEAD:");
        // Exactly 10 chars is not > 10, so it stays as-is.
        assert_eq!(short("git:refs/heads/main:0123456789"), "0123456789");
        assert_eq!(short("git:refs/heads/main:0123456789a"), "0123456789");
        assert_eq!(short("noColonHere"), "noColonHer");
        assert_eq!(short(""), "");
    }

    const SHA: &str = "0123456789abcdef0123456789abcdef01234567";

    // Golden values locked against the real Python
    // `_read_git_revision_fingerprint`.
    #[test]
    fn fingerprint_loose_ref() {
        let d = temp_dir("loose");
        write(&d, ".git/HEAD", "ref: refs/heads/main\n");
        write(&d, ".git/refs/heads/main", &format!("{SHA}\n"));
        assert_eq!(
            read_git_revision_fingerprint(&d),
            Some(format!("git:refs/heads/main:{SHA}"))
        );
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn fingerprint_packed_ref() {
        let d = temp_dir("packed");
        write(&d, ".git/HEAD", "ref: refs/heads/main\n");
        write(
            &d,
            ".git/packed-refs",
            &format!(
                "# pack-refs with: peeled fully-peeled sorted \n{SHA} refs/heads/main\n^abc\n"
            ),
        );
        assert_eq!(
            read_git_revision_fingerprint(&d),
            Some(format!("git:refs/heads/main:{SHA}"))
        );
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn fingerprint_unresolved_ref() {
        let d = temp_dir("unresolved");
        write(&d, ".git/HEAD", "ref: refs/heads/nope\n");
        assert_eq!(
            read_git_revision_fingerprint(&d),
            Some("git:refs/heads/nope:unresolved".to_string())
        );
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn fingerprint_detached_head() {
        let d = temp_dir("detached");
        write(&d, ".git/HEAD", &format!("{SHA}\n"));
        assert_eq!(
            read_git_revision_fingerprint(&d),
            Some(format!("git:HEAD:{SHA}"))
        );
        let _ = std::fs::remove_dir_all(&d);
    }

    // A `.git` FILE pointing at a per-worktree gitdir, whose HEAD ref is packed
    // in the common dir referenced via `commondir`.
    #[test]
    fn fingerprint_worktree() {
        let root = temp_dir("worktree");
        let wt = root.join("wtgit");
        let common = root.join("maingit");
        std::fs::create_dir_all(&wt).unwrap();
        std::fs::create_dir_all(common.join("refs/heads")).unwrap();
        std::fs::write(common.join("refs/heads/feature"), format!("{SHA}\n")).unwrap();
        std::fs::write(wt.join("HEAD"), "ref: refs/heads/feature\n").unwrap();
        std::fs::write(wt.join("commondir"), "../maingit\n").unwrap();
        std::fs::write(root.join(".git"), format!("gitdir: {}\n", wt.display())).unwrap();
        assert_eq!(
            read_git_revision_fingerprint(&root),
            Some(format!("git:refs/heads/feature:{SHA}"))
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn fingerprint_missing_head_is_none() {
        let d = temp_dir("nohead");
        write(&d, ".git/config", "x");
        assert_eq!(read_git_revision_fingerprint(&d), None);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn fingerprint_no_git_is_none() {
        let d = temp_dir("nogit");
        assert_eq!(read_git_revision_fingerprint(&d), None);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn packed_ref_skips_comments_and_peels() {
        let d = temp_dir("packed_lookup");
        std::fs::write(
            d.join("packed-refs"),
            format!("# header\n{SHA} refs/heads/main\n^peeledsha\ndeadbeef refs/tags/v1\n"),
        )
        .unwrap();
        assert_eq!(
            read_packed_ref(&d, "refs/heads/main"),
            Some(SHA.to_string())
        );
        assert_eq!(
            read_packed_ref(&d, "refs/tags/v1"),
            Some("deadbeef".to_string())
        );
        assert_eq!(read_packed_ref(&d, "refs/heads/absent"), None);
        // Missing packed-refs file -> None.
        assert_eq!(read_packed_ref(&temp_dir("empty"), "refs/heads/main"), None);
        let _ = std::fs::remove_dir_all(&d);
    }

    // The skew state machine, exercised without the process-global slot.
    #[test]
    fn compute_skew_semantics() {
        // No boot snapshot -> never reports.
        assert_eq!(
            compute_skew(None, Some("git:HEAD:aaaaaaaaaaaa".into())),
            None
        );
        // Current unreadable -> never reports.
        assert_eq!(compute_skew(Some("git:HEAD:aaaaaaaaaaaa"), None), None);
        // Same revision -> no skew.
        assert_eq!(
            compute_skew(
                Some("git:HEAD:aaaaaaaaaaaa"),
                Some("git:HEAD:aaaaaaaaaaaa".into())
            ),
            None
        );
        // Drift -> short labels for both sides.
        assert_eq!(
            compute_skew(
                Some("git:refs/heads/main:0123456789abcdef"),
                Some("git:refs/heads/main:fedcba9876543210".into())
            ),
            Some(("0123456789".to_string(), "fedcba9876".to_string()))
        );
    }

    // The process-global path resolves against the real repo without panicking;
    // record then detect on an unchanged checkout must not report skew.
    #[test]
    fn record_then_detect_on_unchanged_checkout() {
        record_boot_fingerprint();
        // Same checkout on both reads -> None (or None because the build path
        // is not a git checkout, which also yields None).
        assert_eq!(detect_code_skew(), None);
    }
}
