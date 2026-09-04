//! Port of agent/secret_scope.py.
//!
// Public API is ahead of its callers: the multiplexing gateway and the config
// load path that consume get_secret are not fully wired yet.
#![allow(dead_code)]
//!
//! Profile-scoped credential resolution for multi-profile gateway multiplexing.
//! Each profile has its own `.env` with its own provider keys, so they cannot be
//! unioned into the process-global environment (that would leak profile A's keys
//! into profile B's turns). This provides a fail-closed, context-local secret
//! scope: `get_secret` reads the active profile's mapping when a scope is
//! installed, and otherwise reads the process environment (or, in multiplex
//! mode with no scope, fails closed).
//!
//! Faithfulness note on the scope mechanism. Python uses a `contextvars.ContextVar`,
//! whose imperative `set(...) -> Token` / `reset(token)` API also propagates into
//! the agent's worker thread via `copy_context()`. Rust's analog is a tokio
//! task-local, which is scope-based rather than token-based: install a scope with
//! [`with_secret_scope`] (the analog of `set_secret_scope` + `reset_secret_scope`
//! around a block), and [`current_secret_scope`] / [`get_secret`] read it. The
//! scope propagates through `.await` within that async subtree; it does NOT
//! automatically propagate into `tokio::spawn` / `spawn_blocking` children (unlike
//! Python's `copy_context`), so blocking work that needs the scope must re-enter
//! it. This matters only once profile multiplexing is actually built; the
//! single-profile gateway installs no scope and reads the environment, exactly as
//! Python does with multiplexing off.

use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

// ---------------------------------------------------------------------------
// multiplex-active flag (process-global, not per-task)
// ---------------------------------------------------------------------------

static MULTIPLEX_ACTIVE: AtomicBool = AtomicBool::new(false);

/// Mark whether the process is running as a profile multiplexer. Called once at
/// gateway startup. When true, [`get_secret`] fails closed on an unscoped read
/// instead of falling back to the environment.
pub fn set_multiplex_active(active: bool) {
    MULTIPLEX_ACTIVE.store(active, Ordering::SeqCst);
}

/// Whether the process is running as a profile multiplexer.
pub fn is_multiplex_active() -> bool {
    MULTIPLEX_ACTIVE.load(Ordering::SeqCst)
}

// ---------------------------------------------------------------------------
// the secret scope (contextvar -> tokio task-local)
// ---------------------------------------------------------------------------

tokio::task_local! {
    static SECRET_SCOPE: Option<Arc<HashMap<String, String>>>;
}

/// Raised (returned) when a secret is read in multiplex mode with no scope
/// installed. The fail-closed signal: a credential read reached [`get_secret`]
/// without a profile scope active, which in a multiplexer would otherwise leak
/// whichever profile's value happened to be in the environment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnscopedSecretError {
    pub name: String,
}

impl std::fmt::Display for UnscopedSecretError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "get_secret({:?}) called with no profile secret scope active while \
             multiplexing is on. This credential read must run inside a \
             with_secret_scope(...) block (the per-turn / per-adapter profile \
             scope). Reading the environment here would risk leaking another \
             profile's value.",
            self.name
        )
    }
}

impl std::error::Error for UnscopedSecretError {}

/// The active secret mapping, or `None` when no scope is installed. Mirrors
/// `current_secret_scope`.
pub fn current_secret_scope() -> Option<Arc<HashMap<String, String>>> {
    SECRET_SCOPE.try_with(|s| s.clone()).ok().flatten()
}

/// Run `fut` with `scope` installed as the active profile secret mapping.
///
/// The scope-based analog of Python's `set_secret_scope(mapping)` /
/// `reset_secret_scope(token)`: the scope is active for the duration of `fut`
/// (and the `.await` subtree beneath it) and is torn down when it resolves.
/// Pass `None` to install an explicit empty (cleared) scope.
pub async fn with_secret_scope<F>(scope: Option<HashMap<String, String>>, fut: F) -> F::Output
where
    F: std::future::Future,
{
    SECRET_SCOPE.scope(scope.map(Arc::new), fut).await
}

// ---------------------------------------------------------------------------
// genuinely-global env vars (NOT per-profile secrets)
// ---------------------------------------------------------------------------

const GLOBAL_ENV_EXACT: &[&str] = &[
    // Hermes runtime / deployment
    "HERMES_HOME",
    "HERMES_PROFILE",
    "HERMES_GATEWAY_LOCK_DIR",
    "HERMES_MAX_ITERATIONS",
    "HERMES_MAX_TOKENS",
    "HERMES_API_TIMEOUT",
    "HERMES_REDACT_SECRETS",
    "HERMES_NOUS_TIMEOUT_SECONDS",
    "_HERMES_GATEWAY",
    // OS / interpreter
    "PATH",
    "HOME",
    "USER",
    "LANG",
    "LC_ALL",
    "TZ",
    "PWD",
    "SHELL",
    "TMPDIR",
    "VIRTUAL_ENV",
    "PYTHONPATH",
    "SSL_CERT_FILE",
    // Kanban paths (per-board, not per-profile-secret)
    "HERMES_KANBAN_DB",
    "HERMES_KANBAN_WORKSPACES_ROOT",
    "HERMES_KANBAN_BOARD",
    // API-server LISTENER settings (deployment config, not profile secrets).
    // API_SERVER_KEY is deliberately NOT here: it is a credential and stays
    // profile-scoped.
    "API_SERVER_ENABLED",
    "API_SERVER_HOST",
    "API_SERVER_PORT",
    "API_SERVER_CORS_ORIGINS",
    // Relay-connector ROUTING stamps (deployment config). The auth material
    // (GATEWAY_RELAY_SECRET / _ID / _DELIVERY_KEY and IDP_*) is deliberately
    // NOT here: it stays profile-scoped with the fail-closed guard.
    "GATEWAY_RELAY_URL",
    "GATEWAY_RELAY_ENDPOINT",
    "GATEWAY_RELAY_ALLOW_DIRECT_PLATFORMS",
    "GATEWAY_RELAY_PLATFORMS",
    "GATEWAY_RELAY_BOT_IDS",
    "GATEWAY_RELAY_ROUTE_KEYS",
    "GATEWAY_RELAY_INSTANCE_ID",
    "GATEWAY_RELAY_WAKE_URL",
    "GATEWAY_RELAY_DISPLAY_NAME",
];

const GLOBAL_ENV_PREFIXES: &[&str] = &[
    "HERMES_KANBAN_",
    "HERMES_TELEGRAM_", // tuning knobs, NOT the token
    "TERMINAL_",        // terminal/sandbox backend settings
];

/// True for genuinely process-global (non-profile-secret) env vars.
pub fn is_global_env(name: &str) -> bool {
    if GLOBAL_ENV_EXACT.contains(&name) {
        return true;
    }
    GLOBAL_ENV_PREFIXES.iter().any(|p| name.starts_with(p))
}

// ---------------------------------------------------------------------------
// get_secret
// ---------------------------------------------------------------------------

/// Read an environment-variable-shaped value from the process environment,
/// mirroring `os.environ.get(name)` for the common UTF-8 case.
fn env_get(name: &str) -> Option<String> {
    std::env::var(name).ok()
}

/// Resolve a credential by env-var name, honoring the active profile scope.
///
/// Resolution order (faithful to Python `get_secret`):
///
/// 1. Genuinely-global vars ([`is_global_env`]) always read the environment.
/// 2. With a scope installed: a hit returns the scoped value. A miss returns
///    `default` when multiplexing is on (the scope is authoritative, the
///    environment may hold another profile's value); when multiplexing is off
///    the scope is an overlay, so a miss falls through to the environment.
/// 3. No scope installed: multiplex off reads the environment (legacy behavior);
///    multiplex on FAILS CLOSED with [`UnscopedSecretError`].
pub fn get_secret(
    name: &str,
    default: Option<&str>,
) -> Result<Option<String>, UnscopedSecretError> {
    let default_owned = || default.map(String::from);

    if is_global_env(name) {
        return Ok(env_get(name).or_else(default_owned));
    }

    if let Some(scope) = current_secret_scope() {
        if let Some(val) = scope.get(name) {
            return Ok(Some(val.clone()));
        }
        if is_multiplex_active() {
            return Ok(default_owned());
        }
        // Multiplex off: the scope is an overlay over the process environment,
        // not an isolation boundary. Fall through so credentials injected only
        // into the environment stay visible inside a scoped block.
        return Ok(env_get(name).or_else(default_owned));
    }

    if is_multiplex_active() {
        return Err(UnscopedSecretError {
            name: name.to_string(),
        });
    }

    Ok(env_get(name).or_else(default_owned))
}

// ---------------------------------------------------------------------------
// .env parsing
// ---------------------------------------------------------------------------

/// Parse the small `.env` value subset Hermes writes itself. Mirrors
/// `hermes_cli.config._parse_env_value`: double-quoted values reverse the
/// writer's `\"` and `\\` escapes; single-quoted values just drop the quotes;
/// bare values pass through. All indexing is over Unicode code points, as in
/// Python.
pub fn parse_env_value(raw_value: &str) -> String {
    let value = raw_value.trim();
    let chars: Vec<char> = value.chars().collect();
    if chars.len() >= 2 && chars[0] == '"' && chars[chars.len() - 1] == '"' {
        let quoted = &chars[1..chars.len() - 1];
        let mut parsed = String::new();
        let mut i = 0;
        while i < quoted.len() {
            let ch = quoted[i];
            if ch == '\\' && i + 1 < quoted.len() {
                let next = quoted[i + 1];
                if next == '"' || next == '\\' {
                    parsed.push(next);
                    i += 2;
                    continue;
                }
            }
            parsed.push(ch);
            i += 1;
        }
        return parsed;
    }
    if chars.len() >= 2 && chars[0] == '\'' && chars[chars.len() - 1] == '\'' {
        return chars[1..chars.len() - 1].iter().collect();
    }
    value.to_string()
}

/// Strip a dotenv-style inline comment from a raw `.env` value. Mirrors
/// python-dotenv 1.2.2 semantics as reproduced in Python `_strip_inline_comment`.
///
/// Quoted values: scan for the matching close quote (backslash-escape-aware for
/// double quotes); a trailing `# ...` after the close quote is discarded,
/// non-comment trailing junk is left in place, and an unterminated quote is left
/// as-is. Unquoted values: truncate only at a `#` preceded by whitespace, so
/// `foo#bar` is kept but `value # comment` becomes `value`; a leading `#` is
/// kept.
pub fn strip_inline_comment(value: &str) -> String {
    let value = value.trim();
    if value.is_empty() {
        return String::new();
    }
    let chars: Vec<char> = value.chars().collect();
    let quote = chars[0];
    if quote == '\'' || quote == '"' {
        let mut i = 1;
        while i < chars.len() {
            let ch = chars[i];
            if quote == '"' && ch == '\\' {
                i += 2; // skip the escaped character
                continue;
            }
            if ch == quote {
                let remainder: String = chars[i + 1..].iter().collect();
                if remainder.trim_start().starts_with('#') {
                    return chars[..i + 1].iter().collect();
                }
                return value.to_string();
            }
            i += 1;
        }
        return value.to_string(); // unterminated quote: leave as-is
    }
    // Unquoted: split at the first run of whitespace immediately followed by '#'.
    for i in 0..chars.len() {
        if chars[i] == '#' && i > 0 && chars[i - 1].is_whitespace() {
            // Back up over the whitespace run that precedes the '#'.
            let mut start = i;
            while start > 0 && chars[start - 1].is_whitespace() {
                start -= 1;
            }
            let head: String = chars[..start].iter().collect();
            return head.trim().to_string();
        }
    }
    value.to_string()
}

/// Parse a `.env` file into a plain map WITHOUT touching the process
/// environment. Parses the small KEY=VALUE subset Hermes writes (`export`
/// prefix, `#` comments full-line and dotenv-compatible inline). A leading
/// UTF-8 BOM is stripped (utf-8-sig). Returns an empty map on any read error.
pub fn load_env_file(env_path: &Path) -> HashMap<String, String> {
    let mut secrets: HashMap<String, String> = HashMap::new();
    let text = match std::fs::read(env_path) {
        Ok(bytes) => bytes,
        Err(_) => return secrets,
    };
    // utf-8-sig: drop a leading BOM, then decode lossily (Python read_text would
    // raise UnicodeDecodeError and return {} on invalid utf-8; lossy decode is a
    // close, non-panicking stand-in and never mis-keys valid utf-8 files).
    let text = strip_utf8_bom(&text);
    let text = String::from_utf8_lossy(text);

    for raw in text.lines() {
        let mut line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(rest) = line.strip_prefix("export ") {
            line = rest.trim_start();
        }
        if !line.contains('=') {
            continue;
        }
        let (key, value) = line.split_once('=').unwrap();
        let key = key.trim();
        if key.is_empty() {
            continue;
        }
        secrets.insert(
            key.to_string(),
            parse_env_value(&strip_inline_comment(value)),
        );
    }
    secrets
}

fn strip_utf8_bom(bytes: &[u8]) -> &[u8] {
    bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(bytes)
}

/// Build a profile's secret mapping from its `<home>/.env`. Genuinely-global
/// vars are intentionally not copied in (they resolve from the environment).
///
/// Divergence: Python also merges `hermes_cli.env_loader.get_secret_source_values`
/// (external secret sources like a secret manager). That loader is not ported,
/// so only the `.env` file is read here; the external-source merge is a
/// documented TODO for when that module lands.
pub fn build_profile_secret_scope(hermes_home: &Path) -> HashMap<String, String> {
    load_env_file(&hermes_home.join(".env"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    // The multiplex flag and the process environment are global; serialize the
    // tests that mutate them. The guard is taken once per test and never nested,
    // so a single non-reentrant Mutex is safe.
    static GLOBAL_LOCK: Mutex<()> = Mutex::new(());

    fn block_on<F: std::future::Future>(fut: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(fut)
    }

    #[test]
    fn is_global_env_exact_and_prefix() {
        assert!(is_global_env("HERMES_HOME"));
        assert!(is_global_env("PATH"));
        assert!(is_global_env("API_SERVER_HOST"));
        assert!(is_global_env("GATEWAY_RELAY_URL"));
        // prefixes
        assert!(is_global_env("HERMES_KANBAN_ANYTHING"));
        assert!(is_global_env("HERMES_TELEGRAM_BATCH_DELAY"));
        assert!(is_global_env("TERMINAL_BACKEND"));
        // NOT global: real credentials
        assert!(!is_global_env("API_SERVER_KEY"));
        assert!(!is_global_env("GATEWAY_RELAY_SECRET"));
        assert!(!is_global_env("ANTHROPIC_API_KEY"));
        // Faithful quirk: the "HERMES_TELEGRAM_" prefix catches the token too, so
        // Python treats HERMES_TELEGRAM_TOKEN as global despite the "NOT the
        // token" comment on the prefix. Match that exactly (verified vs Python).
        assert!(is_global_env("HERMES_TELEGRAM_TOKEN"));
    }

    #[test]
    fn get_secret_no_scope_multiplex_off_reads_env() {
        let _g = GLOBAL_LOCK.lock().unwrap();
        set_multiplex_active(false);
        std::env::set_var("SECRETSCOPE_TEST_K", "envval");
        assert_eq!(
            get_secret("SECRETSCOPE_TEST_K", None).unwrap().as_deref(),
            Some("envval")
        );
        std::env::remove_var("SECRETSCOPE_TEST_K");
        assert_eq!(
            get_secret("SECRETSCOPE_TEST_K", Some("dflt"))
                .unwrap()
                .as_deref(),
            Some("dflt")
        );
        assert_eq!(get_secret("SECRETSCOPE_TEST_K", None).unwrap(), None);
    }

    #[test]
    fn get_secret_no_scope_multiplex_on_fails_closed() {
        let _g = GLOBAL_LOCK.lock().unwrap();
        set_multiplex_active(true);
        let err = get_secret("SOME_CREDENTIAL", None).unwrap_err();
        assert_eq!(err.name, "SOME_CREDENTIAL");
        // A global var still reads the environment even in multiplex mode.
        std::env::set_var("HERMES_HOME", "/tmp/x");
        assert_eq!(
            get_secret("HERMES_HOME", None).unwrap().as_deref(),
            Some("/tmp/x")
        );
        set_multiplex_active(false);
    }

    #[test]
    fn get_secret_scoped_hit_and_miss() {
        let _g = GLOBAL_LOCK.lock().unwrap();
        let mut scope = HashMap::new();
        scope.insert("SCOPED_KEY".to_string(), "scopedval".to_string());

        // Multiplex ON: scope is authoritative, a miss returns the default and
        // never falls through to the environment.
        set_multiplex_active(true);
        std::env::set_var("SCOPE_MISS_KEY", "leaky-env-value");
        block_on(with_secret_scope(Some(scope.clone()), async {
            assert_eq!(
                get_secret("SCOPED_KEY", None).unwrap().as_deref(),
                Some("scopedval")
            );
            assert_eq!(
                get_secret("SCOPE_MISS_KEY", Some("d")).unwrap().as_deref(),
                Some("d")
            );
        }));

        // Multiplex OFF: the scope is an overlay, a miss falls through to env.
        set_multiplex_active(false);
        block_on(with_secret_scope(Some(scope), async {
            assert_eq!(
                get_secret("SCOPE_MISS_KEY", None).unwrap().as_deref(),
                Some("leaky-env-value")
            );
        }));
        std::env::remove_var("SCOPE_MISS_KEY");
    }

    #[test]
    fn current_scope_none_outside_block() {
        assert!(current_secret_scope().is_none());
    }

    #[test]
    fn parse_env_value_matches_python() {
        assert_eq!(parse_env_value("plain"), "plain");
        assert_eq!(parse_env_value("  spaced  "), "spaced");
        assert_eq!(parse_env_value("\"quoted\""), "quoted");
        // reversed escapes inside double quotes
        assert_eq!(parse_env_value("\"a\\\"b\""), "a\"b");
        assert_eq!(parse_env_value("\"a\\\\b\""), "a\\b");
        // a non-escape backslash is preserved
        assert_eq!(parse_env_value("\"a\\nb\""), "a\\nb");
        // single quotes: no unescaping
        assert_eq!(parse_env_value("'a\\\"b'"), "a\\\"b");
    }

    #[test]
    fn strip_inline_comment_matches_python() {
        // unquoted
        assert_eq!(strip_inline_comment("value # comment"), "value");
        assert_eq!(strip_inline_comment("foo#bar"), "foo#bar");
        assert_eq!(strip_inline_comment("#leading"), "#leading");
        assert_eq!(strip_inline_comment("plain"), "plain");
        // quoted: trailing comment after close quote is dropped, inner # kept
        assert_eq!(
            strip_inline_comment("\"has # inside\" # trailing"),
            "\"has # inside\""
        );
        assert_eq!(strip_inline_comment("\"no comment\""), "\"no comment\"");
        // unterminated quote left as-is
        assert_eq!(strip_inline_comment("\"unterminated"), "\"unterminated");
    }

    #[test]
    fn load_env_file_parses_subset() {
        let dir = std::env::temp_dir().join(format!("secretscope_env_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join(".env");
        std::fs::write(
            &p,
            b"# a comment\nexport A=1\nB = two # trailing\nC=\"q\\\"x\"\n\nBAD LINE\n=noKey\n",
        )
        .unwrap();
        let m = load_env_file(&p);
        assert_eq!(m.get("A").map(String::as_str), Some("1"));
        assert_eq!(m.get("B").map(String::as_str), Some("two"));
        assert_eq!(m.get("C").map(String::as_str), Some("q\"x"));
        assert!(!m.contains_key("BAD LINE"));
        assert!(!m.contains_key(""));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_env_file_strips_bom() {
        let dir = std::env::temp_dir().join(format!("secretscope_bom_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join(".env");
        let mut bytes = vec![0xEF, 0xBB, 0xBF];
        bytes.extend_from_slice(b"NAME=value\n");
        std::fs::write(&p, bytes).unwrap();
        let m = load_env_file(&p);
        assert_eq!(m.get("NAME").map(String::as_str), Some("value"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
