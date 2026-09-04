//! Gateway-owned local control socket (identify/status, v1).
//!
//! Port of the POSIX core of `gateway/control_socket.py`. A local Unix-domain
//! socket the gateway creates at startup and removes on clean shutdown, so other
//! processes (updater, dashboard, tooling) can discover this gateway's identity
//! and status by an OWNED contract instead of scanning the process table. A
//! connectable socket that answers `identify` IS liveness.
//!
//! Wire contract: one request per connection, a single JSON line in
//! (`{"verb": "...", "id"?: ...}`) and a single JSON line out
//! (`{"ok": bool, "protocol": 1, "result"|"error": ..., "id"?: ...}`), then the
//! server closes. Never a TCP port; filesystem ACLs (owner-only 0600) are the
//! auth boundary. All failures are non-fatal: the gateway never refuses to serve
//! because its control socket could not bind.
//!
//! Windows named-pipe transport and the full runtime-status payload are not
//! ported (status answers a minimal live payload until gateway/status lands).

use std::collections::HashMap;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

pub const CONTROL_PROTOCOL_VERSION: i64 = 1;

const SOCKET_FILENAME: &str = "gateway.sock";
const POINTER_FILENAME: &str = "gateway.sock.path";
/// Stay under the smaller sun_path bound (104 on macOS/BSD) with margin.
const MAX_UNIX_PATH: usize = 100;
const MAX_REQUEST_BYTES: usize = 64 * 1024;
const CLIENT_TIMEOUT: Duration = Duration::from_secs(2);

/// A verb handler: produces the `result` payload for one verb.
pub type VerbHandler = Box<dyn Fn() -> Value + Send + Sync>;

fn home_hash(home: &Path) -> String {
    let canonical = std::fs::canonicalize(home)
        .unwrap_or_else(|_| home.to_path_buf())
        .to_string_lossy()
        .to_string();
    let digest = Sha256::digest(canonical.as_bytes());
    digest.iter().take(8).map(|b| format!("{b:02x}")).collect()
}

/// Where the server should bind, plus the pointer file to write (Some only when
/// the direct in-home path exceeds sun_path and a temp-dir fallback is used).
pub fn resolve_server_socket_path(home: &Path) -> (PathBuf, Option<PathBuf>) {
    let direct = home.join(SOCKET_FILENAME);
    if direct.as_os_str().len() <= MAX_UNIX_PATH {
        return (direct, None);
    }
    let fallback = std::env::temp_dir().join(format!("hermes-gw-{}.sock", home_hash(home)));
    (fallback, Some(home.join(POINTER_FILENAME)))
}

/// Best-effort supervisor kind for THIS process, from its own environment.
fn detect_supervisor() -> &'static str {
    if std::env::var_os("INVOCATION_ID").is_some() {
        "systemd"
    } else if std::env::var_os("HERMES_DESKTOP_MANAGED").is_some() {
        "desktop"
    } else {
        "manual"
    }
}

fn process_start_ticks() -> Option<i64> {
    let pid = std::process::id();
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let tail = stat.rsplit_once(')').map(|(_, t)| t)?;
    tail.split_whitespace().nth(19)?.parse::<i64>().ok()
}

/// The default `identify` payload: this gateway's declared identity.
pub fn build_identify_payload() -> Value {
    json!({
        "protocol": CONTROL_PROTOCOL_VERSION,
        "kind": "gateway",
        "pid": std::process::id(),
        "start_time": process_start_ticks(),
        "hermes_home": crate::config_file::hermes_home().to_string_lossy(),
        "supervisor": detect_supervisor(),
        "code_version": env!("CARGO_PKG_VERSION"),
        "runtime": "rust",
    })
}

/// The default `status` payload. Minimal until gateway/status is ported.
pub fn build_status_payload() -> Value {
    json!({
        "protocol": CONTROL_PROTOCOL_VERSION,
        "answered_at": std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs_f64())
            .unwrap_or(0.0),
        "answering_pid": std::process::id(),
    })
}

/// Process one JSON request line into one JSON response line. Never panics.
pub fn handle_request_line(raw: &str, handlers: &HashMap<String, VerbHandler>) -> String {
    let response = match serde_json::from_str::<Value>(raw) {
        Ok(Value::Object(req)) => {
            let verb = req.get("verb").and_then(Value::as_str);
            let mut resp = match verb.and_then(|v| handlers.get(v)) {
                Some(handler) => json!({
                    "ok": true,
                    "protocol": CONTROL_PROTOCOL_VERSION,
                    "result": handler(),
                }),
                None => {
                    let mut supported: Vec<&String> = handlers.keys().collect();
                    supported.sort();
                    json!({
                        "ok": false,
                        "error": format!("unknown verb: {:?}", verb),
                        "protocol": CONTROL_PROTOCOL_VERSION,
                        "supported_verbs": supported,
                    })
                }
            };
            // Echo the request id when present.
            if let Some(id) = req.get("id") {
                resp["id"] = id.clone();
            }
            resp
        }
        _ => json!({
            "ok": false,
            "error": "request must be a JSON object",
            "protocol": CONTROL_PROTOCOL_VERSION,
        }),
    };
    // Ensure it serializes; fall back to a minimal error line if not.
    serde_json::to_string(&response)
        .unwrap_or_else(|_| r#"{"ok":false,"error":"response serialization failed"}"#.to_string())
}

/// The default verb handlers (identify + status).
fn default_handlers() -> HashMap<String, VerbHandler> {
    let mut h: HashMap<String, VerbHandler> = HashMap::new();
    h.insert("identify".to_string(), Box::new(build_identify_payload));
    h.insert("status".to_string(), Box::new(build_status_payload));
    h
}

/// Bind the control socket and serve until `shutdown` is cancelled, then clean
/// up the socket (and pointer) files. Best-effort: a bind failure logs and
/// returns without taking the gateway down.
pub async fn serve(home: PathBuf, shutdown: CancellationToken) {
    let (bind_path, pointer_file) = resolve_server_socket_path(&home);

    // Clear a stale socket left by a crashed predecessor.
    if bind_path.exists() {
        let _ = std::fs::remove_file(&bind_path);
    }
    let listener = match UnixListener::bind(&bind_path) {
        Ok(l) => l,
        Err(e) => {
            warn!(path = %bind_path.display(), error = %e, "control socket bind failed (non-fatal)");
            return;
        }
    };
    // Owner-only: the socket must never be world-connectable.
    let _ = std::fs::set_permissions(&bind_path, std::fs::Permissions::from_mode(0o600));
    if let Some(pf) = &pointer_file {
        let _ = std::fs::write(pf, bind_path.to_string_lossy().as_bytes());
    }
    info!(path = %bind_path.display(), "control socket listening");

    let handlers = Arc::new(default_handlers());

    loop {
        tokio::select! {
            _ = shutdown.cancelled() => break,
            accepted = listener.accept() => {
                match accepted {
                    Ok((stream, _addr)) => {
                        let handlers = Arc::clone(&handlers);
                        tokio::spawn(handle_connection(stream, handlers));
                    }
                    Err(e) => {
                        warn!(error = %e, "control socket accept failed");
                    }
                }
            }
        }
    }

    // Cleanup on shutdown.
    let _ = std::fs::remove_file(&bind_path);
    if let Some(pf) = &pointer_file {
        let _ = std::fs::remove_file(pf);
    }
    info!("control socket stopped");
}

async fn handle_connection(
    stream: tokio::net::UnixStream,
    handlers: Arc<HashMap<String, VerbHandler>>,
) {
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    // One line in, bounded, with a client timeout.
    let read = tokio::time::timeout(CLIENT_TIMEOUT, reader.read_line(&mut line)).await;
    match read {
        Ok(Ok(n)) if n > 0 && n <= MAX_REQUEST_BYTES => {
            let response = handle_request_line(line.trim_end(), &handlers);
            let mut stream = reader.into_inner();
            let _ = stream.write_all(response.as_bytes()).await;
            let _ = stream.write_all(b"\n").await;
            let _ = stream.flush().await;
        }
        _ => {} // empty, too large, timed out, or errored: just close
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn handlers() -> HashMap<String, VerbHandler> {
        let mut h: HashMap<String, VerbHandler> = HashMap::new();
        h.insert("identify".to_string(), Box::new(|| json!({"who": "me"})));
        h
    }

    #[test]
    fn identify_ok_with_result_and_protocol() {
        let resp = handle_request_line(r#"{"verb":"identify"}"#, &handlers());
        let v: Value = serde_json::from_str(&resp).unwrap();
        assert_eq!(v["ok"], true);
        assert_eq!(v["protocol"], CONTROL_PROTOCOL_VERSION);
        assert_eq!(v["result"]["who"], "me");
    }

    #[test]
    fn request_id_is_echoed() {
        let resp = handle_request_line(r#"{"verb":"identify","id":42}"#, &handlers());
        let v: Value = serde_json::from_str(&resp).unwrap();
        assert_eq!(v["id"], 42);
    }

    #[test]
    fn unknown_verb_lists_supported() {
        let resp = handle_request_line(r#"{"verb":"nope"}"#, &handlers());
        let v: Value = serde_json::from_str(&resp).unwrap();
        assert_eq!(v["ok"], false);
        assert_eq!(v["supported_verbs"], json!(["identify"]));
    }

    #[test]
    fn malformed_request_is_a_clean_error() {
        let resp = handle_request_line("not json", &handlers());
        let v: Value = serde_json::from_str(&resp).unwrap();
        assert_eq!(v["ok"], false);
        // A JSON array is not a request object either.
        let resp2 = handle_request_line("[1,2,3]", &handlers());
        let v2: Value = serde_json::from_str(&resp2).unwrap();
        assert_eq!(v2["ok"], false);
    }

    #[test]
    fn default_handlers_cover_identify_and_status() {
        let h = default_handlers();
        assert!(h.contains_key("identify"));
        assert!(h.contains_key("status"));
        // identify payload carries the declared identity fields.
        let id = build_identify_payload();
        assert_eq!(id["kind"], "gateway");
        assert_eq!(id["runtime"], "rust");
        assert_eq!(id["protocol"], CONTROL_PROTOCOL_VERSION);
    }

    #[test]
    fn short_home_binds_directly_no_pointer() {
        let (path, pointer) = resolve_server_socket_path(Path::new("/tmp/h"));
        assert!(path.ends_with("gateway.sock"));
        assert!(pointer.is_none());
    }
}
