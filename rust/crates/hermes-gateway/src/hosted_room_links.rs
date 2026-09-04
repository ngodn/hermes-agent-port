//! Port of gateway/hosted_room_links.py.
//!
// Public API is ahead of its callers (wired later).
#![allow(dead_code)]
//! Private SQLite storage for negotiated hosted-room links. This is the typed
//! wrapper that sits on top of the room-link record layer ported in
//! `hosted_rooms`: it turns an opaque DB row into a validated [`StoredRoomLink`]
//! (re-checking the target URL, transport security, grant, status, timestamp and
//! the signed [`GatewayRoomCatalog`]) and back again. Route metadata and its
//! scoped grant share the gateway's private `state.db`; the grant is never
//! exposed in Debug output. The catalog is serialized to a canonical JSON string
//! (sorted keys, compact separators) so a row written here is byte-compatible
//! with the Python gateway during the strangler migration. Load, tolerant load
//! (which quarantines malformed rows by `room:member:invalid` identity),
//! save (with a best-effort `0o600` chmod on unix), and status marking all
//! delegate to the `crate::hosted_rooms` record functions.

use std::path::Path;

use serde_json::{Map, Value};

use crate::hosted_room_peer::{validate_room_link_url, GatewayRoomCatalog, HostedRoomPeerError};
use crate::hosted_rooms::{
    list_room_link_records, update_room_link_status, upsert_room_link_record, RoomLinkRecord,
};

/// Cap on stored room links (Python `MAX_LINKS = 512`).
pub const MAX_LINKS: usize = 512;
/// Grant size ceiling in code points (Python `MAX_GRANT_CHARS = 16 * 1024`).
pub const MAX_GRANT_CHARS: usize = 16 * 1024;

/// The nine fields every legacy stored-link mapping must carry.
const LEGACY_FIELDS: [&str; 9] = [
    "room_id",
    "member_id",
    "target_url",
    "target_profile",
    "grant",
    "catalog",
    "cancellation_scope_id",
    "trace_id",
    "updated_at",
];

/// The two later-added optional fields, on top of the legacy set.
const EXTRA_FIELDS: [&str; 2] = ["transport_security", "status"];

/// The recognized route-health statuses (Python `_STATUSES`).
fn is_status(value: &str) -> bool {
    matches!(value, "ready" | "unavailable" | "needs_reauthorization")
}

fn peer(message: &str) -> HostedRoomPeerError {
    HostedRoomPeerError::Peer(message.to_string())
}

type PeerResult<T> = Result<T, HostedRoomPeerError>;

/// Seconds since the Unix epoch, matching Python's `time.time()`.
fn now_secs() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

// ---------------------------------------------------------------------------
// Python-truthiness / coercion helpers (ported inline; the equivalents in
// hosted_room_peer are private to that module).
// ---------------------------------------------------------------------------

fn is_truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n
            .as_f64()
            .map(|f| f != 0.0)
            .unwrap_or_else(|| n.as_i64().map(|i| i != 0).unwrap_or(true)),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

/// Port of `str(value or "")`: falsy values collapse to the empty string.
fn py_str_or_empty(value: &Value) -> String {
    if !is_truthy(value) {
        return String::new();
    }
    match value {
        Value::String(s) => s.clone(),
        Value::Bool(_) => "True".to_string(),
        Value::Number(n) => n.to_string(),
        other => other.to_string(),
    }
}

/// Port of Python `float(value)` for JSON values, returning None where Python
/// would raise TypeError/ValueError.
fn py_float(value: &Value) -> Option<f64> {
    match value {
        Value::Number(n) => n.as_f64(),
        Value::Bool(b) => Some(if *b { 1.0 } else { 0.0 }),
        Value::String(s) => s.trim().parse::<f64>().ok(),
        _ => None,
    }
}

/// Port of `_short_string`: `str(value or "").strip()`, then require a non-empty
/// result of at most 256 code points. `len()` in Python counts code points, so
/// the bound is measured with `chars().count()`.
fn short_string(raw: &str, field: &str) -> PeerResult<String> {
    let normalized = raw.trim();
    if normalized.is_empty() || normalized.chars().count() > 256 {
        return Err(peer(&format!("{field} is invalid")));
    }
    Ok(normalized.to_string())
}

/// The `_short_string` variant that first coerces a JSON value like Python's
/// `str(value or "")`.
fn short_string_value(value: &Value, field: &str) -> PeerResult<String> {
    short_string(&py_str_or_empty(value), field)
}

// ---------------------------------------------------------------------------
// catalog_mapping (shared by StoredRoomLink and make_stored_link)
// ---------------------------------------------------------------------------

/// Port of `StoredRoomLink.catalog_mapping` / the inline dict in
/// `make_stored_link`: reproduce the exact catalog dict that hashed to the
/// stored `catalog_digest`, appending `endpoint` only when the catalog carries a
/// self-advertised endpoint (URL or a not-available reason).
fn catalog_mapping(catalog: &GatewayRoomCatalog) -> Value {
    let mut m = Map::new();
    m.insert(
        "installation_id".into(),
        Value::from(catalog.installation_id.clone()),
    );
    m.insert(
        "protocol_versions".into(),
        Value::from(catalog.protocol_versions.clone()),
    );
    m.insert(
        "link_modes".into(),
        Value::from(
            catalog
                .link_modes
                .iter()
                .map(|mode| Value::from(mode.as_str()))
                .collect::<Vec<_>>(),
        ),
    );
    m.insert(
        "persistent_process".into(),
        Value::Bool(catalog.persistent_process),
    );
    m.insert("text".into(), Value::Bool(catalog.text));
    m.insert("attachments".into(), Value::Bool(catalog.attachments));
    m.insert(
        "execution_policy".into(),
        catalog.execution_policy.as_mapping(),
    );
    m.insert(
        "catalog_digest".into(),
        Value::from(catalog.catalog_digest.clone()),
    );
    if catalog.endpoint_url.is_some() || catalog.endpoint_reason.is_some() {
        m.insert("endpoint".into(), catalog.endpoint_mapping());
    }
    Value::Object(m)
}

// ---------------------------------------------------------------------------
// StoredRoomLink
// ---------------------------------------------------------------------------

/// A validated, negotiated hosted-room link. Mirrors the frozen Python
/// dataclass. `grant` is skipped in the Debug output (the Python `field(repr=
/// False)`), so a `StoredRoomLink` never leaks its scoped grant through logs.
#[derive(Clone, PartialEq)]
pub struct StoredRoomLink {
    pub room_id: String,
    pub member_id: String,
    pub target_url: String,
    pub target_profile: String,
    pub grant: String,
    pub catalog: GatewayRoomCatalog,
    pub cancellation_scope_id: String,
    pub trace_id: String,
    pub transport_security: String,
    pub status: String,
    pub updated_at: f64,
}

impl std::fmt::Debug for StoredRoomLink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // grant is intentionally omitted, matching field(repr=False).
        f.debug_struct("StoredRoomLink")
            .field("room_id", &self.room_id)
            .field("member_id", &self.member_id)
            .field("target_url", &self.target_url)
            .field("target_profile", &self.target_profile)
            .field("catalog", &self.catalog)
            .field("cancellation_scope_id", &self.cancellation_scope_id)
            .field("trace_id", &self.trace_id)
            .field("transport_security", &self.transport_security)
            .field("status", &self.status)
            .field("updated_at", &self.updated_at)
            .finish()
    }
}

impl StoredRoomLink {
    /// Port of `StoredRoomLink.from_mapping`.
    pub fn from_mapping(value: &Value) -> PeerResult<StoredRoomLink> {
        let object = match value {
            Value::Object(m) => m,
            _ => return Err(peer("stored room link fields are invalid")),
        };

        // set(value) - allowed  or  not _LEGACY_FIELDS.issubset(value)
        let allowed_unknown = object
            .keys()
            .any(|k| !LEGACY_FIELDS.contains(&k.as_str()) && !EXTRA_FIELDS.contains(&k.as_str()));
        let missing_legacy = !LEGACY_FIELDS.iter().all(|f| object.contains_key(*f));
        if allowed_unknown || missing_legacy {
            return Err(peer("stored room link fields are invalid"));
        }

        let room_id = short_string_value(&object["room_id"], "room_id")?;
        let member_id = short_string_value(&object["member_id"], "member_id")?;
        let target_profile = short_string_value(&object["target_profile"], "target_profile")?;

        let (target_url, detected_security) = validate_room_link_url(&object["target_url"])?;
        let detected_str = detected_security.as_str();

        // str(value.get("transport_security") or detected_security)
        let supplied_ts = object
            .get("transport_security")
            .map(py_str_or_empty)
            .unwrap_or_default();
        let transport_security = if supplied_ts.is_empty() {
            detected_str.to_string()
        } else {
            supplied_ts
        };
        if transport_security != detected_str {
            return Err(peer("transport_security does not match target_url"));
        }

        let grant = py_str_or_empty(&object["grant"]);
        if grant.is_empty() || grant.chars().count() > MAX_GRANT_CHARS {
            return Err(peer("room grant is missing or too large"));
        }

        // str(value.get("status") or "ready")
        let supplied_status = object
            .get("status")
            .map(py_str_or_empty)
            .unwrap_or_default();
        let status = if supplied_status.is_empty() {
            "ready".to_string()
        } else {
            supplied_status
        };
        if !is_status(&status) {
            return Err(peer("stored room link status is invalid"));
        }

        let updated_at =
            py_float(&object["updated_at"]).ok_or_else(|| peer("updated_at is invalid"))?;
        // Python `not (updated_at > 0)`: reject <= 0 and NaN, but accept +inf
        // (inf > 0 is true). Spelled out to avoid a negated partial-ord compare.
        if updated_at <= 0.0 || updated_at.is_nan() {
            return Err(peer("updated_at must be positive"));
        }

        // Constructor argument order in Python evaluates the catalog first, then
        // cancellation_scope_id, then trace_id, so those validations surface in
        // that order.
        let catalog = GatewayRoomCatalog::from_mapping(&object["catalog"])?;
        let cancellation_scope_id =
            short_string_value(&object["cancellation_scope_id"], "cancellation_scope_id")?;
        let trace_id = short_string_value(&object["trace_id"], "trace_id")?;

        Ok(StoredRoomLink {
            room_id,
            member_id,
            target_url,
            target_profile,
            grant,
            catalog,
            cancellation_scope_id,
            trace_id,
            transport_security,
            status,
            updated_at,
        })
    }

    /// Port of `StoredRoomLink.from_record`: decode the opaque `catalog_json`
    /// column, then run the row through the same strict mapping validation.
    pub fn from_record(record: &RoomLinkRecord) -> PeerResult<StoredRoomLink> {
        let catalog: Value = serde_json::from_str(&record.catalog_json)
            .map_err(|_| peer("stored room link catalog is unreadable"))?;

        let mut m = Map::new();
        m.insert("room_id".into(), Value::from(record.room_id.clone()));
        m.insert("member_id".into(), Value::from(record.member_id.clone()));
        m.insert("target_url".into(), Value::from(record.target_url.clone()));
        m.insert(
            "target_profile".into(),
            Value::from(record.target_profile.clone()),
        );
        m.insert("grant".into(), Value::from(record.grant.clone()));
        m.insert("catalog".into(), catalog);
        m.insert(
            "cancellation_scope_id".into(),
            Value::from(record.cancellation_scope_id.clone()),
        );
        m.insert("trace_id".into(), Value::from(record.trace_id.clone()));
        m.insert(
            "transport_security".into(),
            Value::from(record.transport_security.clone()),
        );
        m.insert("status".into(), Value::from(record.status.clone()));
        m.insert("updated_at".into(), Value::from(record.updated_at));

        StoredRoomLink::from_mapping(&Value::Object(m))
    }

    /// Port of `StoredRoomLink.catalog_mapping`.
    pub fn catalog_mapping(&self) -> Value {
        catalog_mapping(&self.catalog)
    }

    /// Port of `StoredRoomLink.as_record`. `catalog_json` is the canonical JSON
    /// of the catalog mapping: serde_json's `Map` is a sorted `BTreeMap` and
    /// `to_string` uses compact `,`/`:` separators, so the bytes match Python's
    /// `json.dumps(catalog_mapping(), sort_keys=True, separators=(",", ":"))`
    /// (the catalog content is all ASCII, so `ensure_ascii` makes no difference).
    pub fn as_record(&self) -> RoomLinkRecord {
        RoomLinkRecord {
            room_id: self.room_id.clone(),
            member_id: self.member_id.clone(),
            target_url: self.target_url.clone(),
            target_profile: self.target_profile.clone(),
            grant: self.grant.clone(),
            catalog_json: serde_json::to_string(&self.catalog_mapping()).unwrap_or_default(),
            cancellation_scope_id: self.cancellation_scope_id.clone(),
            trace_id: self.trace_id.clone(),
            transport_security: self.transport_security.clone(),
            status: self.status.clone(),
            updated_at: self.updated_at,
        }
    }
}

// ---------------------------------------------------------------------------
// Load / save / status
// ---------------------------------------------------------------------------

/// Port of `load_room_links`: every stored row, strictly validated. Any bad row
/// aborts the whole load.
pub fn load_room_links(db_path: &Path) -> PeerResult<Vec<StoredRoomLink>> {
    let rows = list_room_link_records(db_path).map_err(|e| peer(&e.to_string()))?;
    if rows.len() > MAX_LINKS {
        return Err(peer("stored room link list is invalid"));
    }
    let mut out = Vec::with_capacity(rows.len());
    for row in &rows {
        out.push(StoredRoomLink::from_record(row)?);
    }
    Ok(out)
}

/// Port of `load_room_links_tolerant`: load healthy routes while quarantining
/// malformed rows by `room:member:invalid` identity. A missing/empty id falls
/// back to `unknown`, matching `str(row.get(...) or "unknown")`.
pub fn load_room_links_tolerant(db_path: &Path) -> PeerResult<(Vec<StoredRoomLink>, Vec<String>)> {
    let rows = list_room_link_records(db_path).map_err(|e| peer(&e.to_string()))?;
    if rows.len() > MAX_LINKS {
        return Err(peer("stored room link list is invalid"));
    }
    let mut links = Vec::new();
    let mut errors = Vec::new();
    for row in &rows {
        match StoredRoomLink::from_record(row) {
            Ok(link) => links.push(link),
            Err(_) => {
                let room = if row.room_id.is_empty() {
                    "unknown"
                } else {
                    &row.room_id
                };
                let member = if row.member_id.is_empty() {
                    "unknown"
                } else {
                    &row.member_id
                };
                errors.push(format!("{room}:{member}:invalid"));
            }
        }
    }
    Ok((links, errors))
}

/// Port of `save_room_link`: upsert the record under the capacity cap, then
/// tighten the DB file to owner-only on unix (best effort, like the Python
/// `chmod(0o600)` guarded by `except OSError`).
pub fn save_room_link(db_path: &Path, link: &StoredRoomLink) -> PeerResult<()> {
    upsert_room_link_record(db_path, &link.as_record(), MAX_LINKS as i64)
        .map_err(|e| peer(&e.to_string()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(db_path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

/// Port of `mark_room_link_status`: validate the status, normalize the identity
/// pair, and persist. Returns true only when the link existed.
pub fn mark_room_link_status(
    db_path: &Path,
    room_id: &str,
    member_id: &str,
    status: &str,
) -> PeerResult<bool> {
    if !is_status(status) {
        return Err(peer("stored room link status is invalid"));
    }
    let room = short_string(room_id, "room_id")?;
    let member = short_string(member_id, "member_id")?;
    update_room_link_status(db_path, &room, &member, status, None).map_err(|e| peer(&e.to_string()))
}

/// Port of `make_stored_link`: re-validate the target URL, then build and
/// validate a fresh ready link stamped with the current time.
#[allow(clippy::too_many_arguments)]
pub fn make_stored_link(
    room_id: &str,
    member_id: &str,
    target_url: &str,
    target_profile: &str,
    grant: &str,
    catalog: &GatewayRoomCatalog,
    cancellation_scope_id: &str,
    trace_id: &str,
) -> PeerResult<StoredRoomLink> {
    let (target_url, transport_security) = validate_room_link_url(&Value::from(target_url))?;

    let mut m = Map::new();
    m.insert("room_id".into(), Value::from(room_id));
    m.insert("member_id".into(), Value::from(member_id));
    m.insert("target_url".into(), Value::from(target_url));
    m.insert("target_profile".into(), Value::from(target_profile));
    m.insert("grant".into(), Value::from(grant));
    m.insert("catalog".into(), catalog_mapping(catalog));
    m.insert(
        "cancellation_scope_id".into(),
        Value::from(cancellation_scope_id),
    );
    m.insert("trace_id".into(), Value::from(trace_id));
    m.insert(
        "transport_security".into(),
        Value::from(transport_security.as_str()),
    );
    m.insert("status".into(), Value::from("ready"));
    m.insert("updated_at".into(), Value::from(now_secs()));

    StoredRoomLink::from_mapping(&Value::Object(m))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    // Golden fixtures reused from the hosted_room_peer tests: the digests were
    // computed by running the real Python module, so a catalog built from this
    // mapping validates and its digest survives the JSON round-trip.
    const POLICY_DIGEST: &str = "339c18cc8d1bbc7f8bf6e39a2d17bf7ab5456a2c971b6d8ba69cb71e5a0404ba";
    const CATALOG_DIGEST: &str = "10bf7ebffd15902f9c7730fffbd3e67fa91266a52f0616fde364526947560aee";

    fn catalog_mapping_value() -> Value {
        json!({
            "installation_id": "install-01",
            "protocol_versions": [2],
            "link_modes": ["direct"],
            "persistent_process": true,
            "text": true,
            "attachments": false,
            "execution_policy": {
                "version": 1,
                "target_profile": "default",
                "enabled_toolsets": ["bot_room", "web"],
                "approval_mode": "manual",
                "max_iterations": 10,
                "policy_digest": POLICY_DIGEST,
            },
            "catalog_digest": CATALOG_DIGEST,
        })
    }

    fn sample_catalog() -> GatewayRoomCatalog {
        GatewayRoomCatalog::from_mapping(&catalog_mapping_value()).unwrap()
    }

    fn sample_link(room: &str, member: &str) -> StoredRoomLink {
        make_stored_link(
            room,
            member,
            "https://peer.example/ws",
            "default",
            "opaque-grant-token",
            &sample_catalog(),
            "scope-1",
            "trace-1",
        )
        .unwrap()
    }

    // Unique temp path per test, cleaned up (DB plus -wal/-shm side files).
    struct TempDb {
        path: PathBuf,
    }

    impl TempDb {
        fn new(tag: &str) -> Self {
            static COUNTER: AtomicU64 = AtomicU64::new(0);
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            let pid = std::process::id();
            let mut path = std::env::temp_dir();
            path.push(format!("hermes_room_links_{tag}_{pid}_{n}.db"));
            let _ = std::fs::remove_file(&path);
            TempDb { path }
        }

        fn path(&self) -> &Path {
            &self.path
        }
    }

    impl Drop for TempDb {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.path);
            let _ = std::fs::remove_file(self.path.with_extension("db-wal"));
            let _ = std::fs::remove_file(self.path.with_extension("db-shm"));
        }
    }

    #[test]
    fn save_then_load_roundtrip() {
        let db = TempDb::new("roundtrip");
        let link = sample_link("room-a", "m1");
        assert_eq!(link.transport_security, "tls");
        assert_eq!(link.status, "ready");

        save_room_link(db.path(), &link).unwrap();

        let loaded = load_room_links(db.path()).unwrap();
        assert_eq!(loaded.len(), 1);
        // PartialEq covers every field, including the reconstructed catalog and
        // the exact updated_at that survived the REAL column.
        assert_eq!(loaded[0], link);
    }

    #[test]
    fn tolerant_load_skips_invalid_catalog_row() {
        let db = TempDb::new("tolerant");

        // A healthy row through the normal path.
        let good = sample_link("room-a", "m1");
        save_room_link(db.path(), &good).unwrap();

        // A row whose catalog_json is well-formed JSON but not a valid catalog,
        // written straight through the record layer to bypass validation.
        let bad = RoomLinkRecord {
            room_id: "room-b".to_string(),
            member_id: "m2".to_string(),
            target_url: "https://peer.example/ws".to_string(),
            target_profile: "default".to_string(),
            grant: "opaque".to_string(),
            catalog_json: "{}".to_string(),
            cancellation_scope_id: "scope-1".to_string(),
            trace_id: "trace-1".to_string(),
            transport_security: "tls".to_string(),
            status: "ready".to_string(),
            updated_at: 100.0,
        };
        upsert_room_link_record(db.path(), &bad, MAX_LINKS as i64).unwrap();

        // Strict load fails outright on the bad row.
        assert!(load_room_links(db.path()).is_err());

        // Tolerant load keeps the good one and quarantines the bad one.
        let (links, errors) = load_room_links_tolerant(db.path()).unwrap();
        assert_eq!(links.len(), 1);
        assert_eq!(links[0].room_id, "room-a");
        assert_eq!(errors, vec!["room-b:m2:invalid".to_string()]);
    }

    #[test]
    fn mark_status_updates_and_reports_miss() {
        let db = TempDb::new("status");
        save_room_link(db.path(), &sample_link("room-a", "m1")).unwrap();

        let hit = mark_room_link_status(db.path(), "room-a", "m1", "unavailable").unwrap();
        assert!(hit);

        let loaded = load_room_links(db.path()).unwrap();
        assert_eq!(loaded[0].status, "unavailable");

        // Missing link -> false, no error.
        let miss = mark_room_link_status(db.path(), "room-a", "missing", "ready").unwrap();
        assert!(!miss);

        // Unknown status is rejected before touching the DB.
        assert!(mark_room_link_status(db.path(), "room-a", "m1", "bogus").is_err());
    }

    #[test]
    fn from_mapping_rejects_transport_security_mismatch() {
        // A loopback URL detects "loopback"; claiming "tls" must fail.
        let mut catalog = catalog_mapping_value();
        // Reuse the catalog as-is; only the top-level link fields matter here.
        let value = json!({
            "room_id": "room-a",
            "member_id": "m1",
            "target_url": "http://localhost:8080",
            "target_profile": "default",
            "grant": "opaque",
            "catalog": catalog.take(),
            "cancellation_scope_id": "scope-1",
            "trace_id": "trace-1",
            "transport_security": "tls",
            "status": "ready",
            "updated_at": 100.0,
        });
        let err = StoredRoomLink::from_mapping(&value).unwrap_err();
        assert_eq!(
            err.message(),
            "transport_security does not match target_url"
        );
    }

    #[test]
    fn from_mapping_rejects_unknown_field() {
        let value = json!({
            "room_id": "room-a",
            "member_id": "m1",
            "target_url": "https://peer.example/ws",
            "target_profile": "default",
            "grant": "opaque",
            "catalog": catalog_mapping_value(),
            "cancellation_scope_id": "scope-1",
            "trace_id": "trace-1",
            "updated_at": 100.0,
            "surprise": 1,
        });
        let err = StoredRoomLink::from_mapping(&value).unwrap_err();
        assert_eq!(err.message(), "stored room link fields are invalid");
    }
}
