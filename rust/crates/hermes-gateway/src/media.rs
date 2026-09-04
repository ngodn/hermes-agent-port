//! Port of the media-delivery core of gateway/platforms/base.py.
//!
// Public API is ahead of its callers (the delivery/send path wires it).
#![allow(dead_code)]
//!
//! Turns model response text into native attachments, safely. Two concerns:
//!
//!  * `validate_media_delivery_path` — the security gate. Resolves a candidate
//!    path (symlinks included), rejects the credential/system denylist
//!    (`/etc`, `/proc`, `~/.ssh`, `~/.aws`, `$HERMES_HOME/.env`, `auth.json`,
//!    OAuth token stores, `pairing/`, `mcp-tokens/`, ...), always honors the
//!    Hermes cache + operator allowlist, and in strict mode additionally
//!    requires an allowlisted root or a freshly-produced file. This is the
//!    guard that stops a prompt-injection `MEDIA:/etc/passwd` from exfiltrating
//!    host secrets.
//!  * `extract_media` / `extract_local_files` — pull `MEDIA:<path>` directives
//!    and bare deliverable paths out of a reply, masking fenced code / inline
//!    code / blockquotes / JSON string values first so prose examples and
//!    stored tool-result text are never delivered.
//!
//! Docker container-path translation (`MEDIA:/workspace/...`) is handled behind
//! the [`SandboxLayout`] seam: configured `TERMINAL_DOCKER_VOLUMES` and the
//! cwd->/workspace bind translate here; the session-scoped persistent-sandbox
//! roots are supplied by the terminal subsystem when it is ported. When
//! `TERMINAL_ENV` is not `docker` (the default) translation is a no-op and the
//! host-path security core runs.
//!
//! All character offsets follow Python's code-point indexing (not bytes), so
//! CJK path terminators (#88038) are masked/deleted at the right positions.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use fancy_regex::Regex;

// ── Extension allowlist — single source of truth ─────────────────────────────

/// Deliverable extensions (with leading dot, lowercase). Both extractors derive
/// their extension set from this, so `MEDIA:` and bare-path detection can never
/// drift (issue #34517).
pub const MEDIA_DELIVERY_EXTS: &[&str] = &[
    // Images (embed inline)
    ".png", ".jpg", ".jpeg", ".gif", ".webp", ".bmp", ".tiff", ".svg", // Video
    ".mp4", ".mov", ".avi", ".mkv", ".webm", ".3gp", // Audio
    ".mp3", ".m2a", ".wav", ".ogg", ".opus", ".m4a", ".flac", // Documents
    ".pdf", ".docx", ".doc", ".odt", ".rtf", ".txt", ".md", ".epub",
    // Spreadsheets / data
    ".xlsx", ".xls", ".ods", ".csv", ".tsv", ".json", ".xml", ".yaml", ".yml",
    // Geospatial
    ".kmz", ".kml", ".geojson", ".gpx", // Presentations
    ".pptx", ".ppt", ".odp", ".key", // Archives
    ".zip", ".tar", ".gz", ".tgz", ".bz2", ".xz", ".7z", ".rar", ".apk", ".ipa",
    // Web
    ".html", ".htm",
];

/// Audio extensions (a `[[audio_as_voice]]` tag only makes these voice).
const AUDIO_EXTS: &[&str] = &[".mp3", ".m2a", ".wav", ".ogg", ".opus", ".m4a", ".flac"];

/// CJK full-width punctuation accepted as `MEDIA:` path terminators (#88038).
const CJK_TERMINATORS: &str = "（）〈〉《》：，。；！？、\u{201c}\u{201d}\u{2018}\u{2019}【】";

// ── Denylist / allowlist config ──────────────────────────────────────────────

const DENIED_PREFIXES: &[&str] = &[
    "/etc", "/proc", "/sys", "/dev", "/root", "/boot", "/var/log", "/var/lib", "/var/run",
];

const DENIED_HOME_SUBPATHS: &[&str] = &[
    ".ssh",
    ".aws",
    ".gnupg",
    ".kube",
    ".docker",
    ".config",
    ".azure",
    ".gcloud",
    "Library/Keychains",
];

const CACHE_SUBDIRS: &[&str] = &["images", "audio", "videos", "documents", "screenshots"];

/// Per-file credential/secret stores at a Hermes root (mirrors the read guard in
/// agent/file_safety.py so delivery can't trail the write side).
const ROOT_CREDENTIAL_FILES: &[&str] = &[
    ".env",
    "auth.json",
    "auth.lock",
    "credentials",
    "config.yaml",
    ".anthropic_oauth.json",
    "google_token.json",
    "google_oauth_pending.json",
    "auth/google_oauth.json",
    "webhook_subscriptions.json",
    "cache/bws_cache.json",
    "cache/bws_cache.enc.json",
];

const ROOT_CREDENTIAL_DIRS: &[&str] = &["pairing", "mcp-tokens"];

const ALLOW_DIRS_ENV: &str = "HERMES_MEDIA_ALLOW_DIRS";
const TRUST_RECENT_ENV: &str = "HERMES_MEDIA_TRUST_RECENT_FILES";
const TRUST_RECENT_SECONDS_ENV: &str = "HERMES_MEDIA_TRUST_RECENT_SECONDS";
const STRICT_ENV: &str = "HERMES_MEDIA_DELIVERY_STRICT";
const TRUST_RECENT_DEFAULT_SECONDS: f64 = 600.0;

// ── Path helpers ─────────────────────────────────────────────────────────────

fn expanduser(raw: &str) -> Option<PathBuf> {
    // Reject an embedded NUL (Python expanduser raises ValueError).
    if raw.contains('\0') {
        return None;
    }
    if raw == "~" {
        return std::env::var_os("HOME").map(PathBuf::from);
    }
    if let Some(rest) = raw.strip_prefix("~/") {
        if let Some(home) = std::env::var_os("HOME") {
            return Some(PathBuf::from(home).join(rest));
        }
    }
    Some(PathBuf::from(raw))
}

/// `Path.resolve(strict=False)`: canonicalize when the file exists, else a
/// lexical best-effort (used for allowlist/denylist roots that may not exist).
fn resolve_lenient(path: &Path) -> PathBuf {
    if let Ok(c) = path.canonicalize() {
        return c;
    }
    normalize_lexical(path)
}

/// Lexical normalization: drop `.`, resolve `..` against prior components.
fn normalize_lexical(path: &Path) -> PathBuf {
    use std::path::Component;
    let mut out = PathBuf::new();
    for comp in path.components() {
        match comp {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

fn path_is_within(path: &Path, root: &Path) -> bool {
    path.starts_with(root)
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

fn allowed_roots() -> Vec<PathBuf> {
    let home = crate::config_file::hermes_home();
    let root = crate::config_file::hermes_root();
    let mut roots: Vec<PathBuf> = Vec::new();

    // Canonical + legacy cache layouts under HERMES_HOME.
    for sub in CACHE_SUBDIRS {
        roots.push(home.join("cache").join(sub));
    }
    for legacy in [
        "image_cache",
        "audio_cache",
        "video_cache",
        "document_cache",
        "browser_screenshots",
    ] {
        roots.push(home.join(legacy));
    }

    // Per-profile cache roots: <root>/profiles/<name>/cache/<subdir>.
    let profiles_dir = root.join("profiles");
    if let Ok(entries) = std::fs::read_dir(&profiles_dir) {
        for entry in entries.flatten() {
            if entry.path().is_dir() {
                for sub in CACHE_SUBDIRS {
                    roots.push(entry.path().join("cache").join(sub));
                }
            }
        }
    }

    // Kanban attachment roots.
    roots.extend(kanban_attachment_roots(&root));

    // Operator allowlist (HERMES_MEDIA_ALLOW_DIRS: os.pathsep- and comma-split).
    if let Ok(extra) = std::env::var(ALLOW_DIRS_ENV) {
        for chunk in extra.split(PATH_SEP) {
            for raw in chunk.split(',') {
                let raw = raw.trim();
                if raw.is_empty() {
                    continue;
                }
                if let Some(p) = expanduser(raw) {
                    if p.is_absolute() {
                        roots.push(p);
                    }
                }
            }
        }
    }
    roots
}

#[cfg(windows)]
const PATH_SEP: char = ';';
#[cfg(not(windows))]
const PATH_SEP: char = ':';

fn kanban_attachment_roots(hermes_root: &Path) -> Vec<PathBuf> {
    if let Ok(over) = std::env::var("HERMES_KANBAN_ATTACHMENTS_ROOT") {
        let over = over.trim();
        if !over.is_empty() {
            if let Some(p) = expanduser(over) {
                return vec![p];
            }
        }
    }
    let root = match std::env::var("HERMES_KANBAN_HOME") {
        Ok(h) if !h.trim().is_empty() => {
            expanduser(h.trim()).unwrap_or_else(|| hermes_root.to_path_buf())
        }
        _ => hermes_root.to_path_buf(),
    };
    let mut roots = vec![root.join("kanban").join("attachments")];
    let boards_root = root.join("kanban").join("boards");
    if let Ok(entries) = std::fs::read_dir(&boards_root) {
        for entry in entries.flatten() {
            let p = entry.path();
            if p.is_dir()
                && !p
                    .symlink_metadata()
                    .map(|m| m.file_type().is_symlink())
                    .unwrap_or(true)
                && p.file_name()
                    .and_then(|n| n.to_str())
                    .map(is_board_name)
                    .unwrap_or(false)
                && p.join("kanban.db").is_file()
            {
                roots.push(p.join("attachments"));
            }
        }
    }
    roots
}

/// `[a-z0-9][a-z0-9_-]{0,63}` board-dir name check.
fn is_board_name(name: &str) -> bool {
    let b = name.as_bytes();
    if b.is_empty() || b.len() > 64 {
        return false;
    }
    let first_ok = b[0].is_ascii_lowercase() || b[0].is_ascii_digit();
    first_ok
        && b[1..]
            .iter()
            .all(|&c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'_' || c == b'-')
}

fn denied_paths() -> Vec<PathBuf> {
    let mut denied: Vec<PathBuf> = DENIED_PREFIXES.iter().map(PathBuf::from).collect();
    if let Some(home) = home_dir() {
        for sub in DENIED_HOME_SUBPATHS {
            denied.push(home.join(sub));
        }
    }
    for hermes_root in [
        crate::config_file::hermes_home(),
        crate::config_file::hermes_root(),
    ] {
        for rel in ROOT_CREDENTIAL_FILES {
            denied.push(hermes_root.join(rel));
        }
        for rel in ROOT_CREDENTIAL_DIRS {
            denied.push(hermes_root.join(rel));
        }
    }
    denied
}

fn path_under_denied_prefix(resolved: &Path) -> bool {
    let home = home_dir().map(|h| resolve_lenient(&h));
    for denied in denied_paths() {
        let resolved_denied = resolve_lenient(&denied);
        if !(path_is_within(resolved, &resolved_denied) || resolved == resolved_denied) {
            continue;
        }
        // The running user's own home tree is allowed; its credential sub-dirs
        // are caught by their own more-specific denylist entries.
        if home.as_deref() == Some(&resolved_denied) {
            continue;
        }
        return true;
    }
    false
}

fn recency_seconds() -> f64 {
    let raw = std::env::var(TRUST_RECENT_ENV).unwrap_or_else(|_| "1".to_string());
    let raw = raw.trim().to_lowercase();
    if matches!(raw.as_str(), "0" | "false" | "no" | "off" | "") {
        return 0.0;
    }
    if let Ok(custom) = std::env::var(TRUST_RECENT_SECONDS_ENV) {
        let custom = custom.trim();
        if !custom.is_empty() {
            if let Ok(secs) = custom.parse::<f64>() {
                return secs.max(0.0);
            }
        }
    }
    TRUST_RECENT_DEFAULT_SECONDS
}

fn strict_mode() -> bool {
    let raw = std::env::var(STRICT_ENV).unwrap_or_else(|_| "0".to_string());
    matches!(
        raw.trim().to_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    )
}

fn file_is_recently_produced(resolved: &Path, window_seconds: f64) -> bool {
    if window_seconds <= 0.0 {
        return false;
    }
    let Ok(meta) = resolved.metadata() else {
        return false;
    };
    let Ok(mtime) = meta.modified() else {
        return false;
    };
    let now = std::time::SystemTime::now();
    match now.duration_since(mtime) {
        Ok(age) => age.as_secs_f64() <= window_seconds,
        Err(_) => true, // mtime in the future -> age <= 0 <= window.
    }
}

// ── Docker container-path translation seam ───────────────────────────────────

/// Supplies session-scoped persistent-sandbox roots for Docker MEDIA-path
/// translation. The terminal subsystem implements this when it is ported; until
/// then translation still handles configured `TERMINAL_DOCKER_VOLUMES` and the
/// cwd->/workspace bind, which is the host-resolvable part.
pub trait SandboxLayout: Send + Sync {
    /// (host, container) cache-dir mounts (e.g. host cache -> `/root/.hermes/...`).
    fn cache_mounts(&self) -> Vec<(PathBuf, PathBuf)> {
        Vec::new()
    }
    /// Host roots bound to the persistent `/workspace` mount, best-first.
    fn workspace_host_roots(&self, _session_key: &str) -> Vec<PathBuf> {
        Vec::new()
    }
    /// Host roots bound to the persistent `/root` home mount, best-first.
    fn home_host_roots(&self, _session_key: &str) -> Vec<PathBuf> {
        Vec::new()
    }
}

static SANDBOX_LAYOUT: OnceLock<Box<dyn SandboxLayout>> = OnceLock::new();

/// Install the terminal subsystem's sandbox layout (call once at startup).
pub fn set_sandbox_layout(layout: Box<dyn SandboxLayout>) {
    let _ = SANDBOX_LAYOUT.set(layout);
}

fn tenv(name: &str, default: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| default.to_string())
}

fn terminal_is_docker() -> bool {
    tenv("TERMINAL_ENV", "")
        .trim()
        .eq_ignore_ascii_case("docker")
}

/// Parse `TERMINAL_DOCKER_VOLUMES` (JSON list of `host:container[:mode]`) into
/// `(host, container)` mounts. Named volumes / non-absolute hosts are skipped.
fn parse_docker_volume_mounts() -> Vec<(PathBuf, PathBuf)> {
    let raw = tenv("TERMINAL_DOCKER_VOLUMES", "");
    let raw = raw.trim();
    if raw.is_empty() {
        return Vec::new();
    }
    let Ok(serde_json::Value::Array(entries)) = serde_json::from_str::<serde_json::Value>(raw)
    else {
        return Vec::new();
    };
    let mut mounts = Vec::new();
    for entry in entries {
        let Some(spec) = entry.as_str() else { continue };
        let spec = spec.trim();
        if spec.is_empty() {
            continue;
        }
        // Prefer the first ":/" so absolute container paths are unambiguous.
        let Some(sep) = spec.find(":/") else { continue };
        let host = &spec[..sep];
        let mut container = &spec[sep + 1..];
        // Strip an optional trailing :mode.
        if let Some(mode_sep) = container.rfind(':') {
            let after = &container[mode_sep + 1..];
            if matches!(
                after,
                "ro" | "rw" | "z" | "Z" | "cached" | "delegated" | "consistent"
            ) {
                container = &container[..mode_sep];
            }
        }
        let host_path = PathBuf::from(host);
        let container_path = PathBuf::from(container);
        if host_path.is_absolute() && container_path.is_absolute() {
            mounts.push((host_path, container_path));
        }
    }
    mounts
}

/// Translate a container-absolute path to its host path, or `None`.
fn translate_docker_container_media_path(candidate: &Path, session_key: &str) -> Option<PathBuf> {
    if !candidate.is_absolute() {
        return None;
    }
    let mut mounts = parse_docker_volume_mounts();

    if let Some(layout) = SANDBOX_LAYOUT.get() {
        mounts.extend(layout.cache_mounts());
    }

    // Synthetic /workspace mount.
    let has_workspace = mounts.iter().any(|(_, c)| c == Path::new("/workspace"));
    if !has_workspace {
        for ws in default_workspace_host_roots(session_key) {
            mounts.push((ws, PathBuf::from("/workspace")));
        }
    }
    // Synthetic /root mount (but never for /root/.hermes credential surface).
    let has_root = mounts.iter().any(|(_, c)| c == Path::new("/root"));
    if !has_root && !candidate.starts_with("/root/.hermes") {
        for hr in home_host_roots(session_key) {
            mounts.push((hr, PathBuf::from("/root")));
        }
    }

    if mounts.is_empty() {
        return None;
    }

    // Longest container-prefix match, then insertion order among equal lengths.
    let mut matched: Vec<(PathBuf, PathBuf, usize)> = Vec::new();
    for (host_root, container_root) in &mounts {
        let cstr = container_root.to_string_lossy();
        let cstr = cstr.trim_end_matches('/');
        let cstr = if cstr.is_empty() { "/" } else { cstr };
        let cand = candidate.to_string_lossy();
        if cand == cstr || cand.starts_with(&format!("{cstr}/")) {
            matched.push((host_root.clone(), container_root.clone(), cstr.len()));
        }
    }
    if matched.is_empty() {
        return None;
    }
    matched.sort_by_key(|m| std::cmp::Reverse(m.2));
    for (host_root, container_root, _score) in matched {
        let Ok(relative) = candidate.strip_prefix(&container_root) else {
            continue;
        };
        let Ok(translated) = host_root.join(relative).canonicalize() else {
            continue;
        };
        if translated != host_root && !path_is_within(&translated, &host_root) {
            continue;
        }
        return Some(translated);
    }
    None
}

fn default_workspace_host_roots(session_key: &str) -> Vec<PathBuf> {
    if !terminal_is_docker() {
        return Vec::new();
    }
    let persistent = tenv("TERMINAL_CONTAINER_PERSISTENT", "true")
        .trim()
        .to_lowercase();
    if !matches!(persistent.as_str(), "1" | "true" | "yes" | "on") {
        return Vec::new();
    }
    // Explicit cwd mount takes over /workspace when enabled.
    let mount_cwd = tenv("TERMINAL_DOCKER_MOUNT_CWD_TO_WORKSPACE", "false")
        .trim()
        .to_lowercase();
    if matches!(mount_cwd.as_str(), "1" | "true" | "yes" | "on") {
        let cwd = {
            let c = tenv("TERMINAL_CWD", "");
            if c.trim().is_empty() {
                std::env::current_dir()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .to_string()
            } else {
                c
            }
        };
        if let Some(p) = expanduser(cwd.trim()) {
            let host = resolve_lenient(&p);
            if host.is_dir() {
                return vec![host];
            }
        }
        return Vec::new();
    }
    SANDBOX_LAYOUT
        .get()
        .map(|l| l.workspace_host_roots(session_key))
        .unwrap_or_default()
}

fn home_host_roots(session_key: &str) -> Vec<PathBuf> {
    if !terminal_is_docker() {
        return Vec::new();
    }
    SANDBOX_LAYOUT
        .get()
        .map(|l| l.home_host_roots(session_key))
        .unwrap_or_default()
}

// ── The security gate ────────────────────────────────────────────────────────

/// Return a safe absolute file path for native media delivery, else `None`.
///
/// Default (private gateway): accept any existing regular file not under the
/// credential/system denylist. Strict mode (`HERMES_MEDIA_DELIVERY_STRICT=1`):
/// the file must be under a Hermes cache, an operator-allowlisted root, or
/// freshly produced within the recency window. Symlinks are resolved first.
pub fn validate_media_delivery_path(path: &str, session_key: &str) -> Option<String> {
    if path.is_empty() {
        return None;
    }
    let mut candidate = path.trim().to_string();
    // Strip a matching surrounding quote/backtick pair, then loose edges.
    if let (Some(first), Some(last)) = (candidate.chars().next(), candidate.chars().last()) {
        if candidate.chars().count() >= 2 && first == last && matches!(first, '`' | '"' | '\'') {
            let inner: String = {
                let mut it = candidate.chars();
                it.next();
                it.next_back();
                it.collect()
            };
            candidate = inner.trim().to_string();
        }
    }
    candidate = candidate
        .trim_start_matches(['`', '"', '\''])
        .trim_end_matches(['`', '"', '\'', ',', '.', ';', ':', ')', '}', ']'])
        .to_string();
    if candidate.is_empty() {
        return None;
    }

    let expanded = expanduser(&candidate)?;
    if !expanded.is_absolute() {
        return None;
    }

    let resolved = match translate_docker_container_media_path(&expanded, session_key) {
        Some(t) => t,
        None => expanded.canonicalize().ok()?, // resolve(strict=True): must exist.
    };

    if !resolved.is_file() {
        return None;
    }

    // Cache / operator allowlist is always trusted.
    for root in allowed_roots() {
        let resolved_root = resolve_lenient(&root);
        if path_is_within(&resolved, &resolved_root) {
            return Some(resolved.to_string_lossy().to_string());
        }
    }

    if !strict_mode() {
        if path_under_denied_prefix(&resolved) {
            return None;
        }
        return Some(resolved.to_string_lossy().to_string());
    }

    // Strict mode: recency-based trust for freshly-produced files.
    let window = recency_seconds();
    if window > 0.0
        && !path_under_denied_prefix(&resolved)
        && file_is_recently_produced(&resolved, window)
    {
        return Some(resolved.to_string_lossy().to_string());
    }
    None
}

// ── MEDIA tag extraction ─────────────────────────────────────────────────────

fn ext_alternation() -> &'static str {
    static ALT: OnceLock<String> = OnceLock::new();
    ALT.get_or_init(|| {
        let mut bare: Vec<String> = MEDIA_DELIVERY_EXTS
            .iter()
            .map(|e| e.trim_start_matches('.').to_string())
            .collect();
        // Longest-first so a shorter ext can't match as a prefix of a longer one.
        bare.sort_by_key(|b| std::cmp::Reverse(b.len()));
        bare.join("|")
    })
}

fn ext_alternation_unsorted() -> &'static str {
    static ALT: OnceLock<String> = OnceLock::new();
    ALT.get_or_init(|| {
        MEDIA_DELIVERY_EXTS
            .iter()
            .map(|e| e.trim_start_matches('.'))
            .collect::<Vec<_>>()
            .join("|")
    })
}

fn cleanup_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        let pat = [
            r#"(?i)[`"'*_]{0,3}MEDIA:\s*(?P<path>`[^`\n]+?`|"[^"\n]+?"|'[^'\n]+?'|(?:~/|/|[A-Za-z]:[/\\])\S+?(?:[^\S\n]+\S+?)*?\.(?:"#,
            ext_alternation(),
            r#"))(?=[\s`"'*_,;:)\]}\["#,
            CJK_TERMINATORS,
            r#"]|MEDIA:|\.(?:\s|$)|$)[`"'*_]{0,3}\.?"#,
        ]
        .concat();
        Regex::new(&pat).expect("cleanup regex")
    })
}

fn extensionless_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        let pat = [
            r#"(?i)[`"'*_]{0,3}MEDIA:\s*(?P<path>`[^`\n]+`|"[^"\n]+"|'[^'\n]+'|(?:~/|/|[A-Za-z]:[/\\])[^\s\n`"']+?)(?=[`"'\s,;:)\]}"#,
            CJK_TERMINATORS,
            r#"]|MEDIA:|$)[`"'*_]{0,3}\s*"#,
        ]
        .concat();
        Regex::new(&pat).expect("extensionless regex")
    })
}

fn local_path_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        let pat = [
            r#"(?i)(?<![/:\w.])(?:~/|/|[A-Za-z]:[/\\])(?:[\w.\-]+[/\\])*[\w.\-]+\.(?:"#,
            ext_alternation_unsorted(),
            r#")\b"#,
        ]
        .concat();
        Regex::new(&pat).expect("local path regex")
    })
}

/// Byte offsets of each char start plus the total length, for byte->char index
/// conversion (Python indexes strings by code point).
fn char_bounds(s: &str) -> Vec<usize> {
    let mut v: Vec<usize> = s.char_indices().map(|(i, _)| i).collect();
    v.push(s.len());
    v
}

fn byte_to_char(bounds: &[usize], byte: usize) -> usize {
    match bounds.binary_search(&byte) {
        Ok(i) => i,
        Err(i) => i,
    }
}

/// Normalize a captured MEDIA path (strip a matching quote pair, then edges).
fn normalize_media_tag_path(raw: &str) -> String {
    let mut path = raw.trim().to_string();
    if let (Some(first), Some(last)) = (path.chars().next(), path.chars().last()) {
        if path.chars().count() >= 2 && first == last && matches!(first, '`' | '"' | '\'') {
            let inner: String = {
                let mut it = path.chars();
                it.next();
                it.next_back();
                it.collect()
            };
            path = inner.trim().to_string();
        }
    }
    path.trim_start_matches(['`', '"', '\''])
        .trim_end_matches(['`', '"', '\'', ',', '.', ';', ':', ')', '}', ']'])
        .to_string()
}

fn path_lacks_deliverable_extension(path: &str) -> bool {
    match Path::new(path).extension().and_then(|e| e.to_str()) {
        None => true,
        Some(ext) => {
            let dotted = format!(".{}", ext.to_lowercase());
            !MEDIA_DELIVERY_EXTS.contains(&dotted.as_str())
        }
    }
}

/// Merge overlapping/nested (start,end) char spans.
fn merge_spans(mut spans: Vec<(usize, usize)>) -> Vec<(usize, usize)> {
    spans.sort();
    let mut merged: Vec<(usize, usize)> = Vec::new();
    for (s, e) in spans {
        if let Some(last) = merged.last_mut() {
            if s <= last.1 {
                last.1 = last.1.max(e);
                continue;
            }
        }
        merged.push((s, e));
    }
    merged
}

/// Replace protected spans (fenced code, inline code, blockquotes) with spaces,
/// preserving char count and newlines, so `MEDIA:` inside them is not delivered.
fn mask_protected_spans(content: &str) -> String {
    let mut chars: Vec<char> = content.chars().collect();
    let bounds = char_bounds(content);
    let mut spans: Vec<(usize, usize)> = Vec::new();

    // Fenced code blocks ```...```
    for m in regex_matches(fenced_re(), content) {
        spans.push((byte_to_char(&bounds, m.0), byte_to_char(&bounds, m.1)));
    }
    // Inline code `...`, except a real deliverable MEDIA tag.
    for m in regex_matches(inline_code_re(), content) {
        let cs = byte_to_char(&bounds, m.0);
        // Prefix (up to 20 chars before) ending in `MEDIA:\s*` -> a MEDIA path quote.
        let prefix_start = cs.saturating_sub(20);
        let prefix: String = chars[prefix_start..cs].iter().collect();
        if media_prefix_re().is_match(&prefix).unwrap_or(false) {
            continue;
        }
        let inner: String = chars[cs + 1..byte_to_char(&bounds, m.1) - 1]
            .iter()
            .collect();
        let inner_trim = inner.trim();
        if inner_trim.to_uppercase().starts_with("MEDIA:") {
            let candidate = normalize_media_tag_path(&inner_trim[6..]);
            if !candidate.is_empty() && validate_media_delivery_path(&candidate, "").is_some() {
                continue; // real deliverable tag in inline code
            }
        }
        spans.push((cs, byte_to_char(&bounds, m.1)));
    }
    // Blockquote lines.
    for m in regex_matches(blockquote_re(), content) {
        spans.push((byte_to_char(&bounds, m.0), byte_to_char(&bounds, m.1)));
    }

    for (start, end) in spans {
        for c in chars.iter_mut().take(end).skip(start) {
            if *c != '\n' {
                *c = ' ';
            }
        }
    }
    chars.into_iter().collect()
}

/// Blank out `MEDIA:<bare-path>` occurrences inside JSON string *values* so
/// stored tool-result text is never re-delivered (#34375).
fn mask_json_string_media(content: &str) -> String {
    if !content.contains('"') || !content.contains("MEDIA:") {
        return content.to_string();
    }
    let mut chars: Vec<char> = content.chars().collect();
    let bounds = char_bounds(content);
    let re = json_value_string_re();
    let inner = json_media_bare_re();
    let mut idx = 0usize;
    while let Ok(Some(m)) = re.captures_from_pos(content, idx) {
        let whole = m.get(0).unwrap();
        idx = whole.end().max(whole.start() + 1);
        if let Some(g1) = m.get(1) {
            let seg = &content[g1.start()..g1.end()];
            if inner.is_match(seg).unwrap_or(false) {
                let cs = byte_to_char(&bounds, g1.start());
                let ce = byte_to_char(&bounds, g1.end());
                for c in chars.iter_mut().take(ce).skip(cs) {
                    if *c != '\n' {
                        *c = ' ';
                    }
                }
            }
        }
    }
    chars.into_iter().collect()
}

/// Resolve an extensionless MEDIA match to a validated on-disk path, extending
/// forward across single spaces (validation-gated) for spaced paths (#24032).
/// Returns `(safe_path, end_char_offset_in_scan)`.
fn match_extensionless_path(
    scan_chars: &[char],
    path_start_char: usize,
    path_end_char: usize,
) -> Option<(String, usize)> {
    let raw: String = scan_chars[path_start_char..path_end_char].iter().collect();
    let path = normalize_media_tag_path(&raw);
    if path.is_empty() {
        return None;
    }
    if let Some(safe) = validate_media_delivery_path(&path, "") {
        return Some((safe, path_end_char));
    }
    // Progressive forward extension across single spaces, bounded at 8 tokens,
    // stopping at newline or the next MEDIA: keyword.
    let start = path_start_char;
    let nl = scan_chars[start..].iter().position(|&c| c == '\n');
    let limit = match nl {
        Some(n) => start + n,
        None => scan_chars.len(),
    };
    let mut segment: Vec<char> = scan_chars[start..limit].to_vec();
    // Find next "MEDIA:" after position 1 within the segment.
    if let Some(nxt) = find_subslice(&segment, &['M', 'E', 'D', 'I', 'A', ':'], 1) {
        segment.truncate(nxt);
    }
    let mut pos = path_end_char - start;
    for _ in 0..8 {
        while pos < segment.len() && (segment[pos] == ' ' || segment[pos] == '\t') {
            pos += 1;
        }
        if pos >= segment.len() {
            break;
        }
        let mut tok_end = pos;
        while tok_end < segment.len() && segment[tok_end] != ' ' && segment[tok_end] != '\t' {
            tok_end += 1;
        }
        let candidate: String = segment[..tok_end].iter().collect();
        let candidate = normalize_media_tag_path(&candidate);
        if let Some(safe) = validate_media_delivery_path(&candidate, "") {
            return Some((safe, start + tok_end));
        }
        pos = tok_end;
    }
    None
}

fn find_subslice(hay: &[char], needle: &[char], from: usize) -> Option<usize> {
    if needle.is_empty() || from > hay.len() {
        return None;
    }
    (from..=hay.len().saturating_sub(needle.len())).find(|&i| &hay[i..i + needle.len()] == needle)
}

/// Extract `MEDIA:<path>` tags and `[[audio_as_voice]]` directives. Returns the
/// `(path, is_voice)` pairs and the cleaned text with delivered tags removed.
pub fn extract_media(content: &str) -> (Vec<(String, bool)>, String) {
    let mut media: Vec<(String, bool)> = Vec::new();

    let has_voice_tag = content.contains("[[audio_as_voice]]");
    let mut cleaned = content
        .replace("[[audio_as_voice]]", "")
        .replace("[[as_document]]", "");

    // Mask example / stored MEDIA paths before scanning.
    let scan = mask_json_string_media(&mask_protected_spans(content));
    let scan_chars: Vec<char> = scan.chars().collect();
    let scan_bounds = char_bounds(&scan);

    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();

    // Known-extension tags.
    for cap in captures_all(cleanup_re(), &scan) {
        if let Some((ps, pe)) = cap.path_span {
            let raw: String = scan[ps..pe].to_string();
            let path = normalize_media_tag_path(&raw);
            if path.is_empty() {
                continue;
            }
            let ext = Path::new(&path)
                .extension()
                .and_then(|e| e.to_str())
                .map(|e| format!(".{}", e.to_lowercase()))
                .unwrap_or_default();
            let is_voice = has_voice_tag && AUDIO_EXTS.contains(&ext.as_str());
            let Some(expanded) = expanduser(&path) else {
                continue;
            };
            let expanded = expanded.to_string_lossy().to_string();
            if seen.insert(expanded.clone()) {
                media.push((expanded, is_voice));
            }
        }
    }

    // Extensionless / unknown-extension tags (validated).
    for cap in captures_all(extensionless_re(), &scan) {
        if let Some((ps, pe)) = cap.path_span {
            let raw: String = scan[ps..pe].to_string();
            let path = normalize_media_tag_path(&raw);
            if path.is_empty() || !path_lacks_deliverable_extension(&path) {
                continue;
            }
            let ps_char = byte_to_char(&scan_bounds, ps);
            let pe_char = byte_to_char(&scan_bounds, pe);
            if let Some((safe, _end)) = match_extensionless_path(&scan_chars, ps_char, pe_char) {
                if !seen.contains(&safe) {
                    let ext = Path::new(&safe)
                        .extension()
                        .and_then(|e| e.to_str())
                        .map(|e| format!(".{}", e.to_lowercase()))
                        .unwrap_or_default();
                    media.push((
                        safe.clone(),
                        has_voice_tag && AUDIO_EXTS.contains(&ext.as_str()),
                    ));
                    seen.insert(safe);
                }
            }
        }
    }

    // Delete delivered tag spans from the (unmasked) cleaned text.
    if !media.is_empty() {
        let masked_cleaned = mask_json_string_media(&mask_protected_spans(&cleaned));
        let mc_chars: Vec<char> = masked_cleaned.chars().collect();
        let mc_bounds = char_bounds(&masked_cleaned);
        let mut spans: Vec<(usize, usize)> = Vec::new();
        for cap in captures_all(cleanup_re(), &masked_cleaned) {
            spans.push((
                byte_to_char(&mc_bounds, cap.whole.0),
                byte_to_char(&mc_bounds, cap.whole.1),
            ));
        }
        for cap in captures_all(extensionless_re(), &masked_cleaned) {
            if let Some((ps, pe)) = cap.path_span {
                let raw: String = masked_cleaned[ps..pe].to_string();
                let path = normalize_media_tag_path(&raw);
                if path.is_empty() || !path_lacks_deliverable_extension(&path) {
                    continue;
                }
                let ps_char = byte_to_char(&mc_bounds, ps);
                let pe_char = byte_to_char(&mc_bounds, pe);
                if let Some((_safe, end)) = match_extensionless_path(&mc_chars, ps_char, pe_char) {
                    spans.push((byte_to_char(&mc_bounds, cap.whole.0), end));
                }
            }
        }
        if !spans.is_empty() {
            let mut chars: Vec<char> = cleaned.chars().collect();
            for (start, end) in merge_spans(spans).into_iter().rev() {
                let end = end.min(chars.len());
                let start = start.min(end);
                chars.drain(start..end);
            }
            cleaned = chars.into_iter().collect();
            cleaned = collapse_blank_lines(&cleaned).trim().to_string();
        }
    }

    (media, cleaned)
}

/// Detect bare local file paths (absolute / `~/`) ending in a deliverable
/// extension, skipping fenced/inline code. Returns expanded paths + cleaned text.
pub fn extract_local_files(content: &str) -> (Vec<String>, String) {
    // Spans covered by fenced/inline code.
    let mut code_spans: Vec<(usize, usize)> = Vec::new();
    for m in regex_matches(fenced_re(), content) {
        code_spans.push(m);
    }
    for m in regex_matches(inline_code_re(), content) {
        code_spans.push(m);
    }
    let in_code = |pos: usize| code_spans.iter().any(|&(s, e)| s <= pos && pos < e);

    let mut found: Vec<(String, String)> = Vec::new(); // (raw, expanded)
    for cap in captures_all(local_path_re(), content) {
        if in_code(cap.whole.0) {
            continue;
        }
        let raw = content[cap.whole.0..cap.whole.1].to_string();
        let Some(expanded) = expanduser(&raw) else {
            continue;
        };
        if expanded.is_file() {
            found.push((raw, expanded.to_string_lossy().to_string()));
        }
    }

    // Dedup by expanded path, preserving order.
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut unique: Vec<(String, String)> = Vec::new();
    for (raw, expanded) in found {
        if seen.insert(expanded.clone()) {
            unique.push((raw, expanded));
        }
    }
    let paths: Vec<String> = unique.iter().map(|(_, e)| e.clone()).collect();

    let mut cleaned = content.to_string();
    if !unique.is_empty() {
        for (raw, _) in &unique {
            cleaned = cleaned.replace(raw.as_str(), "");
        }
        cleaned = collapse_blank_lines(&cleaned).trim().to_string();
    }
    (paths, cleaned)
}

fn collapse_blank_lines(s: &str) -> String {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| Regex::new(r"\n{3,}").unwrap());
    re.replace_all(s, "\n\n").into_owned()
}

// ── regex plumbing ───────────────────────────────────────────────────────────

fn fenced_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?s)```[^\n]*\n.*?```").unwrap())
}
fn inline_code_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"`[^`\n]+`").unwrap())
}
fn blockquote_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?m)^>.*$").unwrap())
}
fn media_prefix_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"MEDIA:\s*$").unwrap())
}
fn json_value_string_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r#"(?<=[:,{\[])\s*"((?:[^"\\\n]|\\.)*)""#).unwrap())
}
fn json_media_bare_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r#"MEDIA:\s*(?:~/|/|[A-Za-z]:[/\\])"#).unwrap())
}

/// Non-overlapping (start_byte, end_byte) matches of `re` over `text`.
fn regex_matches(re: &Regex, text: &str) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let mut pos = 0usize;
    while let Ok(Some(m)) = re.find_from_pos(text, pos) {
        out.push((m.start(), m.end()));
        pos = m.end().max(m.start() + 1);
    }
    out
}

struct Cap {
    whole: (usize, usize),
    path_span: Option<(usize, usize)>,
}

/// Non-overlapping captures with the whole-match span and the `path` group span.
fn captures_all(re: &Regex, text: &str) -> Vec<Cap> {
    let mut out = Vec::new();
    let mut pos = 0usize;
    while let Ok(Some(c)) = re.captures_from_pos(text, pos) {
        let whole = c.get(0).unwrap();
        let path_span = c.name("path").map(|m| (m.start(), m.end()));
        out.push(Cap {
            whole: (whole.start(), whole.end()),
            path_span,
        });
        pos = whole.end().max(whole.start() + 1);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// Serializes tests that mutate process env / cwd-sensitive validation.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn write_temp_file(dir: &Path, name: &str, body: &[u8]) -> PathBuf {
        let p = dir.join(name);
        let mut f = std::fs::File::create(&p).unwrap();
        f.write_all(body).unwrap();
        p
    }

    fn fresh_home() -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "hermes_media_{}_{}",
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
    fn validate_accepts_recent_file_and_rejects_denylist() {
        let _g = ENV_LOCK.lock().unwrap();
        let home = fresh_home();
        std::env::set_var("HOME", &home);
        std::env::set_var("HERMES_HOME", home.join(".hermes"));
        std::env::remove_var("HERMES_MEDIA_DELIVERY_STRICT");
        std::env::remove_var("HERMES_MEDIA_ALLOW_DIRS");

        // A freshly produced file in the home tree delivers (non-strict).
        let f = write_temp_file(&home, "report.pdf", b"%PDF-1.4");
        let got = validate_media_delivery_path(f.to_str().unwrap(), "");
        assert!(got.is_some(), "recent home file should validate");

        // A credential store under ~/.ssh is denied even though it exists+is new.
        std::fs::create_dir_all(home.join(".ssh")).unwrap();
        let key = write_temp_file(&home.join(".ssh"), "id_rsa", b"secret");
        assert_eq!(
            validate_media_delivery_path(key.to_str().unwrap(), ""),
            None
        );

        // ~/.hermes/.env is a credential file -> denied.
        std::fs::create_dir_all(home.join(".hermes")).unwrap();
        let env = write_temp_file(&home.join(".hermes"), ".env", b"KEY=v");
        assert_eq!(
            validate_media_delivery_path(env.to_str().unwrap(), ""),
            None
        );

        // Nonexistent path -> None.
        assert_eq!(
            validate_media_delivery_path(home.join("nope.png").to_str().unwrap(), ""),
            None
        );

        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn extract_media_parses_and_cleans() {
        let _g = ENV_LOCK.lock().unwrap();
        let home = fresh_home();
        std::env::set_var("HOME", &home);
        std::env::set_var("HERMES_HOME", home.join(".hermes"));
        std::env::remove_var("HERMES_MEDIA_DELIVERY_STRICT");

        let img = write_temp_file(&home, "chart.png", b"\x89PNG");
        let text = format!("Here is your chart.\nMEDIA:{}\nThanks!", img.display());
        let (media, cleaned) = extract_media(&text);
        assert_eq!(media.len(), 1);
        assert_eq!(media[0].0, img.to_string_lossy());
        assert!(!media[0].1); // not voice
        assert!(!cleaned.contains("MEDIA:"), "tag stripped: {cleaned:?}");
        assert!(cleaned.contains("Here is your chart."));
        assert!(cleaned.contains("Thanks!"));

        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn extract_media_dedupes_same_path() {
        let _g = ENV_LOCK.lock().unwrap();
        let home = fresh_home();
        std::env::set_var("HOME", &home);
        std::env::set_var("HERMES_HOME", home.join(".hermes"));
        let img = write_temp_file(&home, "a.png", b"x");
        let text = format!("MEDIA:{p}\nsummary\nMEDIA:{p}", p = img.display());
        let (media, _cleaned) = extract_media(&text);
        assert_eq!(media.len(), 1, "same file delivered once");
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn media_inside_code_block_is_not_delivered() {
        let _g = ENV_LOCK.lock().unwrap();
        let home = fresh_home();
        std::env::set_var("HOME", &home);
        std::env::set_var("HERMES_HOME", home.join(".hermes"));
        let img = write_temp_file(&home, "real.png", b"x");
        // A fenced code block containing a MEDIA tag must be ignored.
        let text = format!("```\nMEDIA:{}\n```\nno attachment here", img.display());
        let (media, cleaned) = extract_media(&text);
        assert!(media.is_empty(), "code-block MEDIA not delivered");
        assert!(cleaned.contains("MEDIA:"), "code block preserved verbatim");
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn voice_tag_only_marks_audio() {
        let _g = ENV_LOCK.lock().unwrap();
        let home = fresh_home();
        std::env::set_var("HOME", &home);
        std::env::set_var("HERMES_HOME", home.join(".hermes"));
        let ogg = write_temp_file(&home, "v.ogg", b"OggS");
        let png = write_temp_file(&home, "p.png", b"x");
        let text = format!(
            "[[audio_as_voice]]\nMEDIA:{}\nMEDIA:{}",
            ogg.display(),
            png.display()
        );
        let (media, _c) = extract_media(&text);
        let by_path: std::collections::HashMap<_, _> = media.iter().cloned().collect();
        assert_eq!(by_path.get(&ogg.to_string_lossy().to_string()), Some(&true));
        assert_eq!(
            by_path.get(&png.to_string_lossy().to_string()),
            Some(&false)
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn extract_local_files_skips_code_and_missing() {
        let _g = ENV_LOCK.lock().unwrap();
        let home = fresh_home();
        std::env::set_var("HOME", &home);
        std::env::set_var("HERMES_HOME", home.join(".hermes"));
        let doc = write_temp_file(&home, "notes.md", b"# hi");
        let text = format!(
            "See {p}.\nAlso `code {p} here` and {missing}",
            p = doc.display(),
            missing = home.join("ghost.pdf").display()
        );
        let (paths, _cleaned) = extract_local_files(&text);
        assert_eq!(paths, vec![doc.to_string_lossy().to_string()]);
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn merge_spans_merges_overlaps() {
        assert_eq!(
            merge_spans(vec![(0, 3), (2, 5), (7, 9)]),
            vec![(0, 5), (7, 9)]
        );
    }

    #[test]
    fn docker_volume_parsing() {
        let _g = ENV_LOCK.lock().unwrap();
        std::env::set_var(
            "TERMINAL_DOCKER_VOLUMES",
            r#"["/host/proj:/workspace:ro", "named:/data", "/h:/c"]"#,
        );
        let mounts = parse_docker_volume_mounts();
        assert!(mounts.contains(&(PathBuf::from("/host/proj"), PathBuf::from("/workspace"))));
        assert!(mounts.contains(&(PathBuf::from("/h"), PathBuf::from("/c"))));
        // Named volume (non-absolute host) skipped.
        assert!(!mounts.iter().any(|(h, _)| h == Path::new("named")));
        std::env::remove_var("TERMINAL_DOCKER_VOLUMES");
    }
}
