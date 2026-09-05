//! Port of the READ half of `agent/file_safety.py`: `get_read_block_error`,
//! `raise_if_read_blocked`, and `_BLOCKED_PROJECT_ENV_BASENAMES`.
//!
//! Write gates (`build_write_*`, `_classify_write_denial`, cross-profile and
//! sandbox-mirror guards) are intentionally NOT ported here.
//!
//! Not a security boundary, same as the Python docstring: the terminal tool
//! runs as the same OS user and can `cat` any of these files. This is
//! defense-in-depth that returns a clear denial to tool callers and leaves an
//! audit trail.
//
// Native image loading calls check_read before reading files. The live
// dispatcher consumer remains part of rich event integration.
#![allow(dead_code)]

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};

/// Common secret-bearing project-local environment file basenames. Blocked
/// because `.env` files routinely carry API keys, DB passwords, etc. Compared
/// case-insensitively (the Python source lower-cases `resolved.name`).
const BLOCKED_PROJECT_ENV_BASENAMES: &[&str] = &[
    ".env",
    ".env.local",
    ".env.development",
    ".env.production",
    ".env.test",
    ".env.staging",
    ".envrc",
];

/// Resolver context for the read guard. All roots are passed in explicitly so
/// the guard performs NO global env / process-cwd lookups of its own.
///
/// * `home` anchors `~` / `~/...` tilde expansion.
/// * `cwd` anchors relative input paths (the Python source anchors `.resolve()`
///   at the process cwd; here that is `cwd`).
/// * `hermes_home` is the active (profile-aware) HERMES_HOME.
/// * `hermes_root` is the global Hermes root (parent of any profile).
///
/// `home`, `cwd`, `hermes_home`, and `hermes_root` are expected to be absolute.
#[derive(Debug, Clone)]
pub struct FileReadPolicy {
    pub home: PathBuf,
    pub cwd: PathBuf,
    pub hermes_home: PathBuf,
    pub hermes_root: PathBuf,
}

impl FileReadPolicy {
    /// Port of `get_read_block_error`. Returns `Ok(Some(msg))` when the read is
    /// denied, `Ok(None)` when allowed. `Err` only when resolving the *input*
    /// path itself fails unexpectedly (e.g. a `readlink` race on a component the
    /// filesystem just reported as a symlink), this mirrors the un-guarded
    /// top-level `Path(path).expanduser().resolve()` in the source, whose
    /// exception propagates out of `get_read_block_error`.
    ///
    /// The `{path}` in every message is the raw input, not the resolved path,
    /// exactly like the source.
    pub fn get_read_block_error(&self, path: &str) -> anyhow::Result<Option<String>> {
        let resolved = self
            .resolve(path)
            .map_err(|e| anyhow::anyhow!("failed to resolve read path {path:?}: {e}"))?;

        // Resolve BOTH the active HERMES_HOME and the global Hermes root, so
        // credential stores at <root>/auth.json etc. are blocked even under a
        // profile. Per-dir resolution errors are swallowed (continue), matching
        // the source's try/except. Deduped, first-seen order preserved.
        let mut hermes_dirs: Vec<PathBuf> = Vec::new();
        for base in [&self.hermes_home, &self.hermes_root] {
            if let Ok(real) = self.resolve(&base.to_string_lossy()) {
                if !hermes_dirs.contains(&real) {
                    hermes_dirs.push(real);
                }
            }
        }

        // Skills .hub: prompt-injection carriers. LEXICAL check (blocked dirs
        // are NOT re-resolved in the source), so symlinks inside `.hub` are not
        // followed for this match. index-cache is checked before .hub; both
        // yield the same message.
        for hd in &hermes_dirs {
            let blocked_dirs = [
                hd.join("skills").join(".hub").join("index-cache"),
                hd.join("skills").join(".hub"),
            ];
            for blocked in &blocked_dirs {
                if is_at_or_under(&resolved, blocked) {
                    return Ok(Some(format!(
                        "Access denied: {path} is an internal Hermes cache file \
                         and cannot be read directly to prevent prompt injection. \
                         Use the skills_list or skill_view tools instead."
                    )));
                }
            }
        }

        // Credential / secret stores: exact-file matches under either dir. These
        // targets ARE resolved (symlinks followed). Per-name resolution errors
        // are swallowed, matching the source.
        let credential_file_names: [PathBuf; 7] = [
            PathBuf::from("auth.json"),
            PathBuf::from("auth.lock"),
            PathBuf::from(".anthropic_oauth.json"),
            PathBuf::from(".env"),
            PathBuf::from("webhook_subscriptions.json"),
            ["auth", "google_oauth.json"].iter().collect(),
            ["cache", "bws_cache.json"].iter().collect(),
        ];
        for hd in &hermes_dirs {
            for name in &credential_file_names {
                let blocked = match self.resolve(&hd.join(name).to_string_lossy()) {
                    Ok(p) => p,
                    Err(_) => continue,
                };
                if resolved == blocked {
                    return Ok(Some(format!(
                        "Access denied: {path} is a Hermes credential store \
                         and cannot be read directly. Provider tools consume \
                         these credentials through internal channels. \
                         (Defense-in-depth \u{2014} not a security boundary; the \
                         terminal tool can still bypass.)"
                    )));
                }
            }
        }

        // mcp-tokens/: directory prefix match (resolved target). Exact dir gets
        // its own message; anything inside gets the token-file message.
        for hd in &hermes_dirs {
            let mcp_tokens = match self.resolve(&hd.join("mcp-tokens").to_string_lossy()) {
                Ok(p) => p,
                Err(_) => continue,
            };
            if resolved == mcp_tokens {
                return Ok(Some(format!(
                    "Access denied: {path} is the Hermes MCP token directory \
                     and cannot be read directly. (Defense-in-depth \u{2014} not a \
                     security boundary; the terminal tool can still bypass.)"
                )));
            }
            if is_at_or_under(&resolved, &mcp_tokens) {
                return Ok(Some(format!(
                    "Access denied: {path} is a Hermes MCP token file \
                     and cannot be read directly. (Defense-in-depth \u{2014} not a \
                     security boundary; the terminal tool can still bypass.)"
                )));
            }
        }

        // browser-profile/: real-profile browsing snapshot (copied
        // cookies/logins). Same directory-prefix shape as mcp-tokens.
        for hd in &hermes_dirs {
            let browser_profile = match self.resolve(&hd.join("browser-profile").to_string_lossy())
            {
                Ok(p) => p,
                Err(_) => continue,
            };
            if resolved == browser_profile {
                return Ok(Some(format!(
                    "Access denied: {path} is the Hermes real-profile browser \
                     snapshot directory (copied cookies/logins) and cannot be read \
                     directly. (Defense-in-depth \u{2014} not a security boundary; the \
                     terminal tool can still bypass.)"
                )));
            }
            if is_at_or_under(&resolved, &browser_profile) {
                return Ok(Some(format!(
                    "Access denied: {path} is inside the Hermes real-profile browser \
                     snapshot (copied cookies/logins) and cannot be read directly. \
                     (Defense-in-depth \u{2014} not a security boundary; the terminal tool \
                     can still bypass.)"
                )));
            }
        }

        // Project-local secret env files anywhere on disk (case-insensitive
        // basename match).
        if let Some(name) = resolved.file_name() {
            let lname = name.to_string_lossy().to_lowercase();
            if BLOCKED_PROJECT_ENV_BASENAMES.contains(&lname.as_str()) {
                return Ok(Some(format!(
                    "Access denied: {path} is a secret-bearing environment file \
                     and cannot be read to prevent credential leakage. \
                     If you need to check the file structure, read .env.example instead. \
                     (Defense-in-depth \u{2014} not a security boundary; the terminal tool can still bypass.)"
                )));
            }
        }

        Ok(None)
    }

    /// Port of `raise_if_read_blocked`. A native reader calls this before
    /// opening a local file: a real hit is `Err`, an unexpected internal
    /// resolution error is swallowed to `Ok(())` (the guard must never break
    /// local-file loading), and an allowed path is `Ok(())`.
    pub fn check_read(&self, path: &str) -> anyhow::Result<()> {
        match self.get_read_block_error(path) {
            Ok(Some(msg)) => Err(anyhow::anyhow!(msg)),
            Ok(None) => Ok(()),
            Err(_) => Ok(()),
        }
    }

    /// `Path(input).expanduser().resolve()` for this policy's roots.
    ///
    /// Expands a leading `~` / `~user`, anchors relative paths at `cwd`, then
    /// applies `Path.resolve(strict=False)` semantics (existing symlink
    /// ancestors resolved before `..`, nonexistent tails kept, symlink loops
    /// bailed out of rather than followed). Returns `Err` only if `readlink`
    /// fails on a component that was just reported as a symlink.
    fn resolve(&self, input: &str) -> io::Result<PathBuf> {
        let expanded = self.expanduser(input)?;
        let abs = to_abs(&expanded, &self.cwd);
        realpath_abs(abs)
    }

    /// Port of `os.path.expanduser` as `pathlib` applies it: only a leading `~`
    /// is expanded, and only the first path component is inspected.
    fn expanduser(&self, path: &str) -> io::Result<String> {
        if !path.starts_with('~') {
            return Ok(path.to_string());
        }
        // Index of the first '/' after the leading '~' (or end of string).
        let i = match path[1..].find('/') {
            Some(rel) => 1 + rel,
            None => path.len(),
        };
        let tail = &path[i..];
        let userhome: String = if i == 1 {
            // "~" or "~/...": use this policy's home.
            self.home.to_string_lossy().into_owned()
        } else {
            // Path.expanduser raises when os.path.expanduser cannot resolve a
            // named user. Keep that error for the outer best-effort wrapper.
            match getpwnam_home(&path[1..i]) {
                Some(h) => h,
                None => return Err(io::Error::other("could not determine home directory")),
            }
        };
        // posixpath: userhome = userhome.rstrip('/') or '/'; result or '/'.
        let trimmed = userhome.trim_end_matches('/');
        let base = if trimmed.is_empty() { "/" } else { trimmed };
        let combined = format!("{base}{tail}");
        if combined.is_empty() {
            Ok("/".to_string())
        } else {
            Ok(combined)
        }
    }
}

/// Look up a user's home directory via the POSIX passwd database (covers NSS,
/// not just /etc/passwd). Returns `None` if the user is unknown or has no home,
/// matching Python's `pwd.getpwnam` KeyError path.
///
/// Gap: non-UTF-8 home directories are returned lossily; a read guard input is
/// UTF-8 in practice, so this does not affect matching.
#[cfg(unix)]
fn getpwnam_home(user: &str) -> Option<String> {
    use std::ffi::{CStr, CString};
    let cname = CString::new(user).ok()?;
    let mut size = 16384;
    loop {
        let mut buffer = vec![0u8; size];
        let mut entry = std::mem::MaybeUninit::<libc::passwd>::uninit();
        let mut result = std::ptr::null_mut();
        // SAFETY: both result structures and the backing string buffer live
        // through the call and copy. The reentrant API avoids static storage
        // races between concurrent gateway sessions.
        unsafe {
            let code = libc::getpwnam_r(
                cname.as_ptr(),
                entry.as_mut_ptr(),
                buffer.as_mut_ptr().cast(),
                buffer.len(),
                &mut result,
            );
            if code == libc::ERANGE {
                size = size.checked_mul(2)?;
                continue;
            }
            if code != 0 || result.is_null() {
                return None;
            }
            let dir = (*result).pw_dir;
            if dir.is_null() {
                return None;
            }
            return Some(CStr::from_ptr(dir).to_string_lossy().into_owned());
        }
    }
}

/// Non-unix fallback: no passwd database, so `~user` stays literal (documented
/// gap). This crate targets Linux; the branch exists only to keep the module
/// compiling on other targets.
#[cfg(not(unix))]
fn getpwnam_home(_user: &str) -> Option<String> {
    None
}

/// True if `child` is at or lexically under `ancestor` (component-wise, no
/// filesystem access), the `PurePath.relative_to` test used by the source.
/// Component comparison (not string prefix) so `/a/bc` is not "under" `/a/b`.
fn is_at_or_under(child: &Path, ancestor: &Path) -> bool {
    let mut a = ancestor.components();
    let mut c = child.components();
    loop {
        match a.next() {
            None => return true, // ancestor exhausted: child is at or under it
            Some(ac) => match c.next() {
                Some(cc) if cc == ac => continue,
                _ => return false,
            },
        }
    }
}

/// Make `path` absolute against `cwd` without resolving symlinks yet (the
/// symlink walk happens in `realpath_abs`). Mirrors the abspath prefixing step.
fn to_abs(path: &str, cwd: &Path) -> String {
    if path.starts_with('/') {
        return path.to_string();
    }
    let c = cwd.to_string_lossy();
    if c.ends_with('/') {
        format!("{c}{path}")
    } else {
        format!("{c}/{path}")
    }
}

/// `Path.resolve(strict=False)` on an already-absolute path string. Walks
/// components, resolving symlink ancestors before applying `..`, keeping
/// nonexistent tails. Python 3.11/3.12 pathlib raises on a symlink loop even
/// though its underlying posixpath helper returns a partial path.
fn realpath_abs(abs: String) -> io::Result<PathBuf> {
    let mut seen: HashMap<String, Option<String>> = HashMap::new();
    let (p, ok) = join_realpath(String::new(), abs, &mut seen)?;
    if !ok {
        return Err(io::Error::other("symlink loop"));
    }
    Ok(PathBuf::from(normpath(&p)))
}

/// Faithful port of CPython `posixpath._joinrealpath` (strict=False). Returns
/// `(resolved, ok)`; `ok == false` signals an unresolved symlink loop, in which
/// case `resolved` already carries the remaining unprocessed tail appended.
fn join_realpath(
    mut path: String,
    rest: String,
    seen: &mut HashMap<String, Option<String>>,
) -> io::Result<(String, bool)> {
    let mut rest = rest;
    if rest.starts_with('/') {
        rest = rest[1..].to_string();
        path = "/".to_string();
    }
    while !rest.is_empty() {
        let (name, remainder) = match rest.find('/') {
            Some(idx) => (rest[..idx].to_string(), rest[idx + 1..].to_string()),
            None => (rest.clone(), String::new()),
        };
        rest = remainder;
        if name.is_empty() || name == "." {
            continue;
        }
        if name == ".." {
            if !path.is_empty() {
                let (head, tail) = split(&path);
                if tail == ".." {
                    path = join2(&join2(&head, ".."), "..");
                } else {
                    path = head;
                }
            } else {
                path = "..".to_string();
            }
            continue;
        }
        let newpath = join2(&path, &name);
        let is_link = match std::fs::symlink_metadata(&newpath) {
            Ok(md) => md.file_type().is_symlink(),
            Err(_) => false, // strict=False: missing/unreadable component is not a link
        };
        if !is_link {
            path = newpath;
            continue;
        }
        // Symlink: resolve it, guarding against loops via `seen`.
        if let Some(cached) = seen.get(&newpath) {
            match cached {
                Some(resolved) => {
                    path = resolved.clone();
                    continue;
                }
                None => {
                    // Loop detected. strict=False: return already-resolved part
                    // plus the untouched remaining path, no error.
                    return Ok((join2(&newpath, &rest), false));
                }
            }
        }
        seen.insert(newpath.clone(), None);
        // A confirmed symlink whose target cannot be read is the one unexpected
        // error the source lets propagate out of resolve().
        let target = std::fs::read_link(&newpath)?;
        let target = target.to_string_lossy().into_owned();
        let (newp, ok) = join_realpath(path.clone(), target, seen)?;
        path = newp;
        if !ok {
            return Ok((join2(&path, &rest), false));
        }
        seen.insert(newpath, Some(path.clone()));
    }
    Ok((path, true))
}

/// Port of `posixpath.split`.
fn split(p: &str) -> (String, String) {
    let i = match p.rfind('/') {
        Some(idx) => idx + 1,
        None => 0,
    };
    let head = &p[..i];
    let tail = &p[i..];
    // Strip trailing slashes from head unless it is all slashes (the root).
    let head = if !head.is_empty() && !head.chars().all(|c| c == '/') {
        head.trim_end_matches('/')
    } else {
        head
    };
    (head.to_string(), tail.to_string())
}

/// Port of `posixpath.join` for two components.
fn join2(a: &str, b: &str) -> String {
    if b.starts_with('/') {
        b.to_string()
    } else if a.is_empty() || a.ends_with('/') {
        format!("{a}{b}")
    } else {
        format!("{a}/{b}")
    }
}

/// Lexical normalization matching what `Path.resolve` yields: collapse `.`,
/// duplicate slashes, and resolvable `..`, and reduce leading slashes to one.
fn normpath(path: &str) -> String {
    if path.is_empty() {
        return ".".to_string();
    }
    let is_abs = path.starts_with('/');
    let mut out: Vec<&str> = Vec::new();
    for comp in path.split('/') {
        if comp.is_empty() || comp == "." {
            continue;
        }
        if comp != ".." || (!is_abs && out.is_empty()) || out.last() == Some(&"..") {
            out.push(comp);
        } else if !out.is_empty() {
            out.pop();
        }
    }
    let mut s = out.join("/");
    if is_abs {
        s = format!("/{s}");
    }
    if s.is_empty() {
        ".".to_string()
    } else {
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::symlink;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static COUNTER: AtomicUsize = AtomicUsize::new(0);

    /// A temp dir tree under the OS temp dir. All test state (home, cwd, hermes
    /// dirs) lives here, never the real user home.
    struct Sandbox {
        root: PathBuf,
    }

    impl Sandbox {
        fn new() -> Self {
            let n = COUNTER.fetch_add(1, Ordering::SeqCst);
            let root = std::env::temp_dir().join(format!(
                "hermes_read_safety_{}_{}",
                std::process::id(),
                n
            ));
            fs::create_dir_all(&root).unwrap();
            Sandbox { root }
        }

        fn path(&self, rel: &str) -> PathBuf {
            self.root.join(rel)
        }

        fn mkdirs(&self, rel: &str) -> PathBuf {
            let p = self.path(rel);
            fs::create_dir_all(&p).unwrap();
            p
        }

        fn touch(&self, rel: &str) -> PathBuf {
            let p = self.path(rel);
            if let Some(parent) = p.parent() {
                fs::create_dir_all(parent).unwrap();
            }
            fs::write(&p, b"x").unwrap();
            p
        }

        /// Policy with home/cwd at the sandbox root and hermes dirs under it.
        fn policy(&self) -> FileReadPolicy {
            let hermes_root = self.mkdirs(".hermes");
            FileReadPolicy {
                home: self.root.clone(),
                cwd: self.root.clone(),
                hermes_home: hermes_root.clone(),
                hermes_root,
            }
        }
    }

    impl Drop for Sandbox {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn credential_auth_json_denied() {
        let sb = Sandbox::new();
        let p = sb.policy();
        let target = sb.touch(".hermes/auth.json");
        let msg = p
            .get_read_block_error(&target.to_string_lossy())
            .unwrap()
            .expect("should be denied");
        assert!(msg.contains("Hermes credential store"));
        assert!(msg.contains(&*target.to_string_lossy()));
        assert!(p.check_read(&target.to_string_lossy()).is_err());
    }

    #[test]
    fn hermes_env_hits_credential_message_first() {
        // <hermes_home>/.env is in credential_file_names AND the basename set;
        // the credential exact-match runs first, so the message is the
        // credential-store one, not the project-env one.
        let sb = Sandbox::new();
        let p = sb.policy();
        let target = sb.touch(".hermes/.env");
        let msg = p
            .get_read_block_error(&target.to_string_lossy())
            .unwrap()
            .unwrap();
        assert!(msg.contains("Hermes credential store"));
    }

    #[test]
    fn project_env_anywhere_denied() {
        let sb = Sandbox::new();
        let p = sb.policy();
        let target = sb.touch("projects/app/.env");
        let msg = p
            .get_read_block_error(&target.to_string_lossy())
            .unwrap()
            .unwrap();
        assert!(msg.contains("secret-bearing environment file"));
    }

    #[test]
    fn project_env_basename_case_insensitive() {
        let sb = Sandbox::new();
        let p = sb.policy();
        let target = sb.touch("projects/app/.ENV");
        assert!(p
            .get_read_block_error(&target.to_string_lossy())
            .unwrap()
            .is_some());
    }

    #[test]
    fn mcp_tokens_dir_and_file() {
        let sb = Sandbox::new();
        let p = sb.policy();
        let dir = sb.mkdirs(".hermes/mcp-tokens");
        let dir_msg = p
            .get_read_block_error(&dir.to_string_lossy())
            .unwrap()
            .unwrap();
        assert!(dir_msg.contains("MCP token directory"));

        let file = sb.touch(".hermes/mcp-tokens/server/token.json");
        let file_msg = p
            .get_read_block_error(&file.to_string_lossy())
            .unwrap()
            .unwrap();
        assert!(file_msg.contains("MCP token file"));
    }

    #[test]
    fn browser_profile_dir_and_inside() {
        let sb = Sandbox::new();
        let p = sb.policy();
        let dir = sb.mkdirs(".hermes/browser-profile");
        let dir_msg = p
            .get_read_block_error(&dir.to_string_lossy())
            .unwrap()
            .unwrap();
        assert!(dir_msg.contains("snapshot directory"));

        let file = sb.touch(".hermes/browser-profile/Default/Cookies");
        let inside_msg = p
            .get_read_block_error(&file.to_string_lossy())
            .unwrap()
            .unwrap();
        assert!(inside_msg.contains("inside the Hermes real-profile browser"));
    }

    #[test]
    fn hub_lexical_match() {
        let sb = Sandbox::new();
        let p = sb.policy();
        let file = sb.touch(".hermes/skills/.hub/index-cache/entry.json");
        let msg = p
            .get_read_block_error(&file.to_string_lossy())
            .unwrap()
            .unwrap();
        assert!(msg.contains("internal Hermes cache file"));
    }

    #[test]
    fn hub_lexical_does_not_follow_symlinks() {
        // The .hub check is lexical: a path textually under <hd>/skills/.hub is
        // blocked even if the tail does not exist / is not resolvable.
        let sb = Sandbox::new();
        let p = sb.policy();
        sb.mkdirs(".hermes/skills/.hub");
        let phantom = sb.path(".hermes/skills/.hub/does/not/exist.json");
        assert!(p
            .get_read_block_error(&phantom.to_string_lossy())
            .unwrap()
            .is_some());
    }

    #[test]
    fn allowed_normal_path_is_none() {
        let sb = Sandbox::new();
        let p = sb.policy();
        let target = sb.touch("projects/app/main.rs");
        assert!(p
            .get_read_block_error(&target.to_string_lossy())
            .unwrap()
            .is_none());
        assert!(p.check_read(&target.to_string_lossy()).is_ok());
    }

    #[test]
    fn symlink_ancestor_resolves_to_credential() {
        // A symlinked directory ancestor is resolved before matching, so a path
        // reaching auth.json via a symlink is still denied.
        let sb = Sandbox::new();
        let p = sb.policy();
        sb.touch(".hermes/auth.json");
        symlink(sb.path(".hermes"), sb.path("link-to-hermes")).unwrap();
        let via_link = sb.path("link-to-hermes/auth.json");
        let msg = p
            .get_read_block_error(&via_link.to_string_lossy())
            .unwrap()
            .unwrap();
        assert!(msg.contains("Hermes credential store"));
    }

    #[test]
    fn relative_path_resolves_against_cwd() {
        // cwd is the sandbox root; "projects/app/.env" must resolve under it.
        let sb = Sandbox::new();
        let p = sb.policy();
        sb.touch("projects/app/.env");
        assert!(p
            .get_read_block_error("projects/app/.env")
            .unwrap()
            .is_some());
    }

    #[test]
    fn tilde_expansion_uses_policy_home() {
        // home is the sandbox root; ~/.hermes/auth.json -> denied credential.
        let sb = Sandbox::new();
        let p = sb.policy();
        sb.touch(".hermes/auth.json");
        let msg = p
            .get_read_block_error("~/.hermes/auth.json")
            .unwrap()
            .unwrap();
        assert!(msg.contains("Hermes credential store"));
    }

    #[test]
    fn dotdot_after_symlink_ancestor() {
        // real/dir is real, `link` -> real/dir; link/../auth.json must resolve
        // to real/auth.json (symlink resolved before ..), not the cwd sibling.
        let sb = Sandbox::new();
        let mut p = sb.policy();
        // Point hermes_home at the "real" dir so the resolved target is guarded.
        let real = sb.mkdirs("real");
        p.hermes_home = real.clone();
        p.hermes_root = real.clone();
        sb.mkdirs("real/dir");
        sb.touch("real/auth.json");
        symlink(sb.path("real/dir"), sb.path("link")).unwrap();
        let via = sb.path("link/../auth.json");
        let msg = p
            .get_read_block_error(&via.to_string_lossy())
            .unwrap()
            .unwrap();
        assert!(msg.contains("Hermes credential store"));
    }

    #[test]
    fn cyclic_symlink_errors_but_best_effort_wrapper_allows() {
        // Match the supported Python reference's pathlib exception, which the
        // outer read wrapper swallows without turning it into a denial.
        let sb = Sandbox::new();
        let p = sb.policy();
        symlink("b", sb.path("a")).unwrap();
        symlink("a", sb.path("b")).unwrap();
        let cyc = sb.path("a");
        assert!(p.get_read_block_error(&cyc.to_string_lossy()).is_err());
        assert!(p.check_read(&cyc.to_string_lossy()).is_ok());
    }

    #[test]
    fn nonexistent_tail_still_checked() {
        // A path that does not exist still resolves lexically and is checked;
        // <hermes>/mcp-tokens/missing.json is blocked even though absent.
        let sb = Sandbox::new();
        let p = sb.policy();
        let missing = sb.path(".hermes/mcp-tokens/missing.json");
        assert!(p
            .get_read_block_error(&missing.to_string_lossy())
            .unwrap()
            .is_some());
    }
}

#[cfg(all(test, unix))]
mod golden_corpus {
    use super::*;
    use serde_json::Value;

    #[test]
    fn real_paths_match_python_read_guard() {
        let fixture: Value =
            serde_json::from_str(include_str!("../../../tools/file-read-safety-goldens.json"))
                .unwrap();
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root =
            std::env::temp_dir().join(format!("hermes-read-corpus-{}-{stamp}", std::process::id()));
        struct Cleanup(PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let _cleanup = Cleanup(root.clone());
        for entry in fixture["directories"].as_array().unwrap() {
            std::fs::create_dir_all(root.join(entry.as_str().unwrap())).unwrap();
        }
        for (entry, target) in fixture["links"].as_object().unwrap() {
            std::os::unix::fs::symlink(target.as_str().unwrap(), root.join(entry)).unwrap();
        }
        let policy = FileReadPolicy {
            home: root.join("user"),
            cwd: root.join("project"),
            hermes_home: root.join("user/.hermes/profiles/work"),
            hermes_root: root.join("user/.hermes"),
        };
        for case in fixture["cases"].as_array().unwrap() {
            let path = case["path"]
                .as_str()
                .unwrap()
                .replace("__ROOT__", root.to_str().unwrap());
            let actual = match policy.get_read_block_error(&path) {
                Ok(result) => {
                    serde_json::json!(result.map(|s| s.replace(root.to_str().unwrap(), "__ROOT__")))
                }
                Err(_) => serde_json::json!("resolution_error"),
            };
            assert_eq!(actual, case["expected"], "{case}");
            assert_eq!(
                policy.check_read(&path).is_ok(),
                case["allowed"].as_bool().unwrap(),
                "{case}"
            );
        }
    }
}
