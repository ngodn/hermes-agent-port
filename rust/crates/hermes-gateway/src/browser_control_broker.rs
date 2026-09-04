//! Port of gateway/browser_control_broker.py.
//!
// Public API is ahead of its callers (wired later). The boxed send/clock
// closures read as complex types but are the natural shape here.
#![allow(dead_code, clippy::type_complexity)]
//! Transport-neutral, in-process browser-control broker core. It binds an
//! identity-scoped controller (the party that physically drives a browser) to
//! callers over any transport without knowing anything about HTTP or
//! WebSocket: callers register a `send` callback and the broker only ever hands
//! frames to that callback. It mints short-lived, single-use, cryptographically
//! random registration tickets; selects a controller only on an exact match of
//! every stable identity field plus a negotiated capability; and drives a
//! command lifecycle whose invariants (single-shot completion, scoped
//! cancellation, fail-closed detach, and recoverable disconnect) are enforced
//! structurally rather than by convention. Nothing here routes traffic; the
//! transport layers built later wrap this core.

use std::collections::{BTreeSet, HashMap};
use std::fmt;
use std::fs::File;
use std::io::Read;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{json, Map, Value};

/// Default lifetime of a minted registration ticket, in clock seconds.
pub const DEFAULT_TICKET_TTL: f64 = 30.0;
/// Default wall time a dispatch waits for the controller to complete.
pub const DEFAULT_COMMAND_TIMEOUT: f64 = 30.0;
/// Maximum cancel frames retained while a same-identity controller is offline.
pub const MAX_DEFERRED_CANCELS: usize = 512;

/// Current wire protocol version. Registration requires this exact integer;
/// booleans are rejected even though `bool` subclasses `int` in Python.
pub const BROWSER_CONTROL_PROTOCOL_VERSION: i64 = 1;

/// Exact controller capability contract shared by every transport. The broker
/// never accepts arbitrary browser methods: raw CDP, script evaluation, console
/// access, uploads, and other privileged surfaces remain outside this allowlist.
pub const BROWSER_CONTROL_CAPABILITIES: [&str; 11] = [
    "controller.noop",
    "browser_back",
    "browser_click",
    "browser_navigate",
    "browser_press",
    "browser_screenshot",
    "browser_scroll",
    "browser_snapshot",
    "browser_tab_activate",
    "browser_tabs",
    "browser_type",
];

/// Privileged capabilities that are never negotiable through the base
/// allowlist. `browser_evaluate` executes JavaScript in the page context;
/// `browser_cdp` is raw CDP. Both are fail-closed unless the broker runs in
/// Developer Mode AND the controller explicitly negotiated the capability.
pub const BROWSER_CONTROL_DEVELOPER_CAPABILITIES: [&str; 2] = ["browser_cdp", "browser_evaluate"];

/// Artifact-transport capabilities. These are regular (non-developer)
/// capabilities because upload/download of bounded, validated artifacts is a
/// safe surface; the payloads are never carried in controller frames. Artifact
/// actions are dispatched only after the broker validates the referenced
/// artifact id against the attached store ("approved artifact id only").
pub const BROWSER_CONTROL_ARTIFACT_CAPABILITIES: [&str; 2] =
    ["browser_artifact_download", "browser_artifact_upload"];

/// Wire method name for a controller command frame. Transport-neutral by
/// contract: transports carry these envelopes verbatim.
pub const FRAME_COMMAND: &str = "browser.controller.command";
/// Wire method name for a controller cancel frame.
pub const FRAME_CANCEL: &str = "browser.controller.cancel";

/// Return whether `cap` names a privileged (developer-gated) capability.
pub fn is_developer_capability(cap: &str) -> bool {
    BROWSER_CONTROL_DEVELOPER_CAPABILITIES.contains(&cap)
}

/// Return whether `cap` names an artifact-transport capability.
pub fn is_artifact_capability(cap: &str) -> bool {
    BROWSER_CONTROL_ARTIFACT_CAPABILITIES.contains(&cap)
}

/// The complete set a controller may negotiate: base + artifact + developer.
/// Developer capabilities are only admitted by
/// [`filter_browser_control_capabilities`] when Developer Mode is enabled.
pub fn browser_control_all_capabilities() -> BTreeSet<String> {
    BROWSER_CONTROL_CAPABILITIES
        .iter()
        .chain(BROWSER_CONTROL_ARTIFACT_CAPABILITIES.iter())
        .chain(BROWSER_CONTROL_DEVELOPER_CAPABILITIES.iter())
        .map(|s| s.to_string())
        .collect()
}

/// Return whether `value` names the exact supported wire version. Mirrors the
/// Python `type(value) is int` check: a JSON boolean or float is rejected even
/// though it may compare equal to 1.
pub fn browser_control_protocol_supported(value: &Value) -> bool {
    // serde_json keeps booleans distinct from numbers, and `as_i64` yields
    // None for a value with a fractional part, so this rejects true and 1.0.
    value.as_i64() == Some(BROWSER_CONTROL_PROTOCOL_VERSION)
}

/// Return the explicit Developer Mode flag (disabled by default).
///
/// Reads `browser.extension_control.developer_mode` from the passed config.
/// The live-config probe that the Python does when `config` is None is
/// deferred to a later wiring phase, so `None` here fails closed to `false`.
pub fn browser_control_developer_mode(config: Option<&Value>) -> bool {
    read_extension_control_flag(config, "developer_mode")
}

/// Return the explicit browser-control feature flag (disabled by default).
/// Same live-config caveat as [`browser_control_developer_mode`].
pub fn browser_control_enabled(config: Option<&Value>) -> bool {
    read_extension_control_flag(config, "enabled")
}

fn read_extension_control_flag(config: Option<&Value>, key: &str) -> bool {
    let Some(config) = config else {
        return false;
    };
    config
        .get("browser")
        .and_then(|b| b.get("extension_control"))
        .and_then(|e| e.get(key))
        .map(|v| v == &Value::Bool(true))
        .unwrap_or(false)
}

/// Return the permitted subset of a JSON/RPC capability list.
///
/// A malformed non-array value has no capabilities. Unknown or non-string
/// entries are ignored. Base and artifact capabilities always pass; developer
/// capabilities pass only when Developer Mode is explicitly enabled (passed in,
/// or read from config, which is fail-closed to `false` when absent here).
pub fn filter_browser_control_capabilities(
    value: &Value,
    developer_mode: Option<bool>,
) -> BTreeSet<String> {
    let Some(items) = value.as_array() else {
        return BTreeSet::new();
    };
    let mut allowed: BTreeSet<String> = BROWSER_CONTROL_CAPABILITIES
        .iter()
        .chain(BROWSER_CONTROL_ARTIFACT_CAPABILITIES.iter())
        .map(|s| s.to_string())
        .collect();
    let dev = developer_mode.unwrap_or_else(|| browser_control_developer_mode(None));
    if dev {
        for cap in BROWSER_CONTROL_DEVELOPER_CAPABILITIES {
            allowed.insert(cap.to_string());
        }
    }
    items
        .iter()
        .filter_map(|item| item.as_str())
        .filter(|s| allowed.contains(*s))
        .map(|s| s.to_string())
        .collect()
}

/// One Rust error enum covering the Python broker exception classes plus the
/// two non-broker failure paths (a transport send failure and a failed
/// deferred-cancel flush) and the fail-closed CSPRNG path the security contract
/// requires. Display messages mirror the Python messages.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BrowserControlError {
    /// `ControllerTicketInvalid`: ticket unknown, already consumed, or expired.
    TicketInvalid(String),
    /// `ControllerUnavailable`: no attached controller exactly matches.
    Unavailable(String),
    /// `ControllerCancelled`: a pending command was cancelled.
    Cancelled(String),
    /// `ControllerTimeout`: the controller did not complete in time.
    Timeout(String),
    /// `ControllerRejected`: the controller completed with ok=false, or an
    /// artifact reference failed validation.
    Rejected(String),
    /// Python's `ConnectionError` from a failed deferred-cancel flush on
    /// reconnect. Not a broker exception subclass, kept here for one error type.
    Connection(String),
    /// The `send` callback failed (Python re-raises the transport exception).
    Transport(String),
    /// No Python analog: the kernel CSPRNG was unavailable, so the broker fails
    /// closed rather than emitting a guessable ticket or command id.
    Random(String),
}

impl fmt::Display for BrowserControlError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BrowserControlError::TicketInvalid(m)
            | BrowserControlError::Unavailable(m)
            | BrowserControlError::Cancelled(m)
            | BrowserControlError::Timeout(m)
            | BrowserControlError::Rejected(m)
            | BrowserControlError::Connection(m)
            | BrowserControlError::Transport(m)
            | BrowserControlError::Random(m) => write!(f, "{m}"),
        }
    }
}

impl std::error::Error for BrowserControlError {}

/// Exact identity of a browser controller plus its capability set.
///
/// Equality is structural over all fields, so two scopes differing in any
/// single field (including `transport_family`) never match. The stable
/// identity is the six id fields; `capabilities` is negotiated, not identity.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default)]
pub struct ControllerScope {
    pub principal_id: Option<String>,
    pub profile_id: Option<String>,
    pub session_id: Option<String>,
    pub controller_id: Option<String>,
    pub browser_profile_id: Option<String>,
    pub transport_family: Option<String>,
    /// A `frozenset` in Python; a BTreeSet gives the same order-independent set
    /// equality while remaining Hash + Eq so the scope can key a map.
    pub capabilities: BTreeSet<String>,
}

impl ControllerScope {
    /// Stable controller identity, excluding negotiated capabilities.
    fn identity(
        &self,
    ) -> (
        &Option<String>,
        &Option<String>,
        &Option<String>,
        &Option<String>,
        &Option<String>,
        &Option<String>,
    ) {
        (
            &self.principal_id,
            &self.profile_id,
            &self.session_id,
            &self.controller_id,
            &self.browser_profile_id,
            &self.transport_family,
        )
    }
}

fn same_scope_identity(a: &ControllerScope, b: &ControllerScope) -> bool {
    a.identity() == b.identity()
}

/// Opaque, single-use registration credential.
#[derive(Debug, Clone, PartialEq)]
pub struct Ticket {
    pub value: String,
    pub expires_at: f64,
}

/// Opaque identity token for a transport connection ("owner"). Python compares
/// these by object identity; an integer handle makes identity and equality
/// coincide, which is the faithful behavior for opaque tokens.
pub type Owner = u64;

/// The owner argument shape Python models with its `_OWNER_UNSET` sentinel:
/// `Unset` skips the owner check entirely; `Set(o)` requires the controller's
/// owner to equal `o` (which may itself be `None`, an ownerless controller).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OwnerArg {
    Unset,
    Set(Option<Owner>),
}

/// Injectable clock returning seconds as a float. Python defaults to
/// `time.monotonic`; the default here reads epoch seconds, which is
/// interchangeable because every TTL check is relative (now vs now + ttl).
pub type Clock = Box<dyn Fn() -> f64 + Send + Sync>;

/// The `send` callback a transport registers. Takes a frame; returns `Err` to
/// model the Python callable raising (the transport failed to deliver).
pub type SendFn = dyn Fn(&Value) -> Result<(), String> + Send + Sync;

fn default_clock() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// Duck-typed artifact store contract. `validate` returns `Ok` for an approved,
/// live, scope-bound artifact reference and `Err(message)` for any problem
/// (missing id, traversal, expiry, checksum, or scope mismatch). Mirrors the
/// Python store's `validate(artifact_id, *, scope)` raising `ArtifactError`.
pub trait ArtifactValidator: Send + Sync {
    fn validate(&self, artifact_id: &str, scope: &ControllerScope) -> Result<(), String>;
}

struct TicketRecord {
    scope: ControllerScope,
    expires_at: f64,
    consumed: bool,
}

struct Controller {
    scope: ControllerScope,
    send: Arc<SendFn>,
    owner: Option<Owner>,
    connected: bool,
    deferred_cancels: Vec<Value>,
    // Serializes command/cancel writes with detach or replacement. Broker state
    // is never held while waiting for this lock, so a transport callback may
    // synchronously call complete() without deadlocking the broker.
    send_lock: Arc<Mutex<()>>,
    // Emulates Python's object identity (`is`) checks across a lock release.
    id: u64,
}

/// The waitable terminal outcome of a pending command. Every transition is
/// written while the broker lock is held (so completion is arbitrated exactly
/// once); the condvar lets a dispatcher park outside the broker lock.
struct Outcome {
    done: bool,
    cancelled: bool,
    ok: bool,
    result: Value,
}

struct Waiter {
    state: Mutex<Outcome>,
    cv: Condvar,
}

struct PendingCommand {
    scope: ControllerScope,
    command_id: String,
    tool_call_id: Option<String>,
    waiter: Arc<Waiter>,
}

/// A lightweight snapshot of a selected controller. `select` runs outside the
/// send lock; dispatch revalidates the live controller by `id` after acquiring
/// the send lock, so this snapshot never authorizes a stale send on its own.
pub struct SelectedController {
    id: u64,
    scope: ControllerScope,
    send: Arc<SendFn>,
    send_lock: Arc<Mutex<()>>,
}

impl SelectedController {
    /// The controller's negotiated scope at selection time.
    pub fn scope(&self) -> &ControllerScope {
        &self.scope
    }
}

struct InnerState {
    tickets: HashMap<String, TicketRecord>,
    controllers: HashMap<ControllerScope, Controller>,
    pending: HashMap<String, PendingCommand>,
    // Keyed by resolved profile id; None is the default/unscoped store.
    artifact_stores: HashMap<Option<String>, Arc<dyn ArtifactValidator>>,
}

/// Thread-safe broker core binding controllers to callers.
pub struct BrowserControlBroker {
    ticket_ttl: f64,
    command_timeout: f64,
    clock: Clock,
    // None defers to the live config on every selection (fail-closed to false
    // here); an explicit bool pins the gate for tests and multi-tenant hosts.
    developer_mode_pinned: Option<bool>,
    id_counter: AtomicU64,
    inner: Mutex<InnerState>,
}

impl BrowserControlBroker {
    /// Construct a broker.
    ///
    /// * `ticket_ttl` / `command_timeout` in clock seconds.
    /// * `clock` injectable time source (defaults to epoch seconds).
    /// * `developer_mode` `None` defers to the live config (fail-closed here);
    ///   `Some(b)` pins the developer gate.
    pub fn new(
        ticket_ttl: f64,
        command_timeout: f64,
        clock: Option<Clock>,
        developer_mode: Option<bool>,
    ) -> Self {
        BrowserControlBroker {
            ticket_ttl,
            command_timeout,
            clock: clock.unwrap_or_else(|| Box::new(default_clock)),
            developer_mode_pinned: developer_mode,
            id_counter: AtomicU64::new(1),
            inner: Mutex::new(InnerState {
                tickets: HashMap::new(),
                controllers: HashMap::new(),
                pending: HashMap::new(),
                artifact_stores: HashMap::new(),
            }),
        }
    }

    /// Construct with the module default TTLs and clock.
    pub fn with_defaults() -> Self {
        Self::new(DEFAULT_TICKET_TTL, DEFAULT_COMMAND_TIMEOUT, None, None)
    }

    fn next_id(&self) -> u64 {
        self.id_counter.fetch_add(1, Ordering::Relaxed)
    }

    fn developer_mode_now(&self) -> bool {
        match self.developer_mode_pinned {
            Some(b) => b,
            // Live config probe is deferred to a later wiring phase; fail closed.
            None => browser_control_developer_mode(None),
        }
    }

    /// Whether privileged capabilities may be selected/dispatched.
    pub fn developer_mode(&self) -> bool {
        self.developer_mode_now()
    }

    /// Configured lifetime for newly minted one-shot tickets.
    pub fn ticket_ttl_seconds(&self) -> f64 {
        self.ticket_ttl
    }

    /// Number of commands awaiting completion (diagnostics/tests).
    pub fn pending_count(&self) -> usize {
        self.inner.lock().unwrap().pending.len()
    }

    // ------------------------------------------------------------------
    // Artifact stores
    // ------------------------------------------------------------------

    /// Attach (or, with `None`, clear) the artifact store for "approved
    /// artifact id only". `profile_id` scopes the store to one profile on
    /// multiplex hosts; `None` registers the default store.
    pub fn attach_artifact_store(
        &self,
        store: Option<Arc<dyn ArtifactValidator>>,
        profile_id: Option<String>,
    ) {
        let mut inner = self.inner.lock().unwrap();
        match store {
            None => {
                inner.artifact_stores.remove(&profile_id);
            }
            Some(s) => {
                inner.artifact_stores.insert(profile_id, s);
            }
        }
    }

    fn artifact_store_for_scope(
        inner: &InnerState,
        scope: &ControllerScope,
    ) -> Option<Arc<dyn ArtifactValidator>> {
        // Empty profile id resolves to the default slot, matching `x or None`.
        let profile = scope.profile_id.as_ref().filter(|p| !p.is_empty()).cloned();
        if let Some(store) = inner.artifact_stores.get(&profile) {
            return Some(store.clone());
        }
        inner.artifact_stores.get(&None).cloned()
    }

    // ------------------------------------------------------------------
    // Registration tickets
    // ------------------------------------------------------------------

    /// Mint a short-lived, single-use ticket bound to `scope`.
    ///
    /// The value is 43 URL-safe base64 characters over 32 CSPRNG bytes, the
    /// exact shape of Python `secrets.token_urlsafe(32)`. Fails closed with
    /// [`BrowserControlError::Random`] if the kernel CSPRNG is unavailable.
    pub fn mint_ticket(&self, scope: ControllerScope) -> Result<Ticket, BrowserControlError> {
        let value = token_urlsafe(32).ok_or_else(|| {
            BrowserControlError::Random("CSPRNG unavailable; refusing to mint ticket".into())
        })?;
        let now = (self.clock)();
        let expires_at = now + self.ticket_ttl;
        let mut inner = self.inner.lock().unwrap();
        Self::prune_tickets(&mut inner, now);
        inner.tickets.insert(
            value.clone(),
            TicketRecord {
                scope,
                expires_at,
                consumed: false,
            },
        );
        Ok(Ticket { value, expires_at })
    }

    /// Exchange a ticket for its scope, exactly once. Raises
    /// [`BrowserControlError::TicketInvalid`] for unknown, already-consumed, or
    /// expired tickets. Expiry is checked against the live clock at consume time.
    pub fn consume_ticket(&self, value: &str) -> Result<ControllerScope, BrowserControlError> {
        let now = (self.clock)();
        let mut inner = self.inner.lock().unwrap();
        let Some(record) = inner.tickets.get_mut(value) else {
            return Err(BrowserControlError::TicketInvalid("unknown ticket".into()));
        };
        if record.consumed {
            return Err(BrowserControlError::TicketInvalid(
                "ticket already consumed".into(),
            ));
        }
        if now > record.expires_at {
            return Err(BrowserControlError::TicketInvalid("ticket expired".into()));
        }
        record.consumed = true;
        Ok(record.scope.clone())
    }

    fn prune_tickets(inner: &mut InnerState, now: f64) {
        inner.tickets.retain(|_, rec| rec.expires_at > now);
    }

    // ------------------------------------------------------------------
    // Controller registration / selection
    // ------------------------------------------------------------------

    /// Attach or refresh the controller for one stable identity.
    ///
    /// A reconnect with the same identity refreshes the send callback and
    /// negotiated capabilities without cancelling pending work (capabilities
    /// are not an identity field). A different controller or browser profile in
    /// the same authenticated session lane hard-replaces the previous identity.
    /// Returns [`BrowserControlError::Connection`] if a reconnect cannot flush
    /// its retained cancel frames through the refreshed callback.
    pub fn attach(
        &self,
        scope: ControllerScope,
        send: Box<dyn Fn(&Value) -> Result<(), String> + Send + Sync>,
        owner: Option<Owner>,
    ) -> Result<(), BrowserControlError> {
        let send: Arc<SendFn> = Arc::from(send);
        loop {
            let existing: Option<(ControllerScope, u64, Arc<Mutex<()>>)>;
            let lane_scopes: Vec<ControllerScope>;
            {
                let mut inner = self.inner.lock().unwrap();
                existing = inner
                    .controllers
                    .iter()
                    .find(|(k, _)| same_scope_identity(k, &scope))
                    .map(|(k, c)| (k.clone(), c.id, c.send_lock.clone()));
                lane_scopes = inner
                    .controllers
                    .keys()
                    .filter(|k| {
                        k.principal_id == scope.principal_id
                            && k.profile_id == scope.profile_id
                            && k.session_id == scope.session_id
                            && k.transport_family == scope.transport_family
                            && !same_scope_identity(k, &scope)
                    })
                    .cloned()
                    .collect();
                if existing.is_none() && lane_scopes.is_empty() {
                    let id = self.next_id();
                    inner.controllers.insert(
                        scope.clone(),
                        Controller {
                            scope: scope.clone(),
                            send: send.clone(),
                            owner,
                            connected: true,
                            deferred_cancels: Vec::new(),
                            send_lock: Arc::new(Mutex::new(())),
                            id,
                        },
                    );
                    return Ok(());
                }
            }

            // A different identity in the same authenticated session lane is a
            // hard replacement, not a recoverable reconnect. Terminalize it
            // before inserting the successor so session lookup stays unique.
            if !lane_scopes.is_empty() {
                for lane_scope in &lane_scopes {
                    self.detach(lane_scope, OwnerArg::Unset, false);
                }
                continue;
            }

            let (existing_scope, existing_id, existing_send_lock) = existing.unwrap();
            let _send_guard = existing_send_lock.lock().unwrap();
            let deferred: Vec<Value>;
            {
                let mut inner = self.inner.lock().unwrap();
                match inner.controllers.get(&existing_scope) {
                    Some(c) if c.id == existing_id => {}
                    _ => continue, // replaced between lock releases; retry
                }
                let mut ctrl = inner.controllers.remove(&existing_scope).unwrap();
                ctrl.scope = scope.clone();
                ctrl.send = send.clone();
                ctrl.owner = owner;
                ctrl.connected = false;
                for pending in inner.pending.values_mut() {
                    if same_scope_identity(&pending.scope, &scope) {
                        pending.scope = scope.clone();
                    }
                }
                deferred = std::mem::take(&mut ctrl.deferred_cancels);
                inner.controllers.insert(scope.clone(), ctrl);
            }

            // Flush retained cancels through the refreshed callback while still
            // holding the old generation's send lock, so a later command frame
            // can never overtake a terminal cancel frame.
            let mut unsent: Vec<Value> = Vec::new();
            for (index, frame) in deferred.iter().enumerate() {
                if (send)(frame).is_err() {
                    tracing::error!("failed to flush deferred browser-controller cancel");
                    unsent = deferred[index..].to_vec();
                    break;
                }
            }
            if !unsent.is_empty() {
                let mut inner = self.inner.lock().unwrap();
                if let Some(c) = inner.controllers.get_mut(&scope) {
                    if c.id == existing_id {
                        let start = unsent.len().saturating_sub(MAX_DEFERRED_CANCELS);
                        c.deferred_cancels = unsent[start..].to_vec();
                    }
                }
                return Err(BrowserControlError::Connection(
                    "browser controller reconnect could not flush deferred cancels".into(),
                ));
            }
            {
                let mut inner = self.inner.lock().unwrap();
                if let Some(c) = inner.controllers.get_mut(&scope) {
                    if c.id == existing_id {
                        c.connected = true;
                    }
                }
            }
            return Ok(());
        }
    }

    /// Return the connected controller matching identity and capability, or
    /// `None` on a partial match, an ambiguous set, or a developer capability
    /// while Developer Mode is off. The gate consults the live flag (unless
    /// pinned) so flipping it off revokes raw CDP/eval without a restart.
    pub fn select(&self, scope: &ControllerScope, capability: &str) -> Option<SelectedController> {
        if is_developer_capability(capability) && !self.developer_mode_now() {
            return None;
        }
        let inner = self.inner.lock().unwrap();
        let mut matches = inner.controllers.values().filter(|c| {
            same_scope_identity(&c.scope, scope)
                && c.connected
                && c.scope.capabilities.contains(capability)
        });
        let first = matches.next()?;
        if matches.next().is_some() {
            return None; // ambiguous; fail closed
        }
        Some(SelectedController {
            id: first.id,
            scope: first.scope.clone(),
            send: first.send.clone(),
            send_lock: first.send_lock.clone(),
        })
    }

    /// Whether `owner` is the exact live transport for `scope`. Ownership is
    /// independent of capabilities.
    pub fn is_owner(&self, scope: &ControllerScope, owner: Option<Owner>) -> bool {
        let inner = self.inner.lock().unwrap();
        inner
            .controllers
            .values()
            .filter(|c| same_scope_identity(&c.scope, scope) && c.connected && c.owner == owner)
            .count()
            == 1
    }

    /// Mark one exact controller transport offline without cancelling work.
    pub fn disconnect(&self, scope: &ControllerScope, owner: OwnerArg) -> bool {
        let sel = {
            let inner = self.inner.lock().unwrap();
            inner
                .controllers
                .iter()
                .find(|(k, _)| same_scope_identity(k, scope))
                .map(|(k, c)| (k.clone(), c.id, c.send_lock.clone()))
        };
        let Some((cscope, id, send_lock)) = sel else {
            return false;
        };
        let _send_guard = send_lock.lock().unwrap();
        let mut inner = self.inner.lock().unwrap();
        match inner.controllers.get(&cscope) {
            Some(c) if c.id == id => {}
            _ => return false,
        }
        let controller = inner.controllers.get_mut(&cscope).unwrap();
        if let OwnerArg::Set(o) = owner {
            if controller.owner != o {
                return false;
            }
        }
        controller.connected = false;
        controller.owner = None;
        true
    }

    /// Remove the controller for the exact `scope` and fail its pending work
    /// closed: every pending command of the scope is cancelled and resolved, so
    /// waiting dispatchers raise [`BrowserControlError::Cancelled`] and a late
    /// `complete` returns `false`.
    pub fn detach(&self, scope: &ControllerScope, owner: OwnerArg, notify_controller: bool) {
        let sel = {
            let inner = self.inner.lock().unwrap();
            inner
                .controllers
                .get(scope)
                .map(|c| (c.id, c.owner, c.send.clone(), c.send_lock.clone()))
        };
        let Some((id, cowner, send, send_lock)) = sel else {
            return;
        };
        if let OwnerArg::Set(o) = owner {
            if cowner != o {
                return;
            }
        }
        let _send_guard = send_lock.lock().unwrap();
        let targets: Vec<(String, Option<String>)> = {
            let mut inner = self.inner.lock().unwrap();
            match inner.controllers.get(scope) {
                Some(c) if c.id == id => {}
                _ => return,
            }
            if let OwnerArg::Set(o) = owner {
                if inner.controllers.get(scope).map(|c| c.owner) != Some(o) {
                    return;
                }
            }
            inner.controllers.remove(scope);
            let targets: Vec<(String, Option<String>)> = inner
                .pending
                .values()
                .filter(|p| same_scope_identity(&p.scope, scope))
                .map(|p| (p.command_id.clone(), p.tool_call_id.clone()))
                .collect();
            for (cid, _) in &targets {
                Self::resolve_pending(&mut inner, cid, true);
            }
            targets
        };
        // Keep the old generation's send lock through cancellation so a command
        // frame can never overtake its terminal cancel frame.
        if notify_controller {
            for (cid, tcid) in targets {
                let frame = cancel_frame(&cid, tcid);
                emit_frame(&send, &frame, &cid);
            }
        }
    }

    // ------------------------------------------------------------------
    // Command lifecycle
    // ------------------------------------------------------------------

    /// Send one controller command and block for its completion.
    ///
    /// Emits a `browser.controller.command` frame carrying a fresh command id,
    /// then waits up to `command_timeout`. Returns the completion result or one
    /// of `Unavailable` / `Cancelled` / `Timeout` / `Rejected`. Exactly one
    /// pending command exists per command id and `complete` is single-shot.
    ///
    /// Artifact actions additionally require an attached store and a valid,
    /// scope-bound `artifact_id` in `arguments` ("approved artifact id only");
    /// only the id travels in the frame, never the payload.
    pub fn dispatch(
        &self,
        scope: &ControllerScope,
        action: &str,
        arguments: Option<Map<String, Value>>,
        tool_call_id: Option<String>,
    ) -> Result<Value, BrowserControlError> {
        let Some(sel) = self.select(scope, action) else {
            return Err(BrowserControlError::Unavailable(format!(
                "no controller for scope {scope:?} with capability {action:?}"
            )));
        };

        let arguments = arguments.unwrap_or_default();
        if is_artifact_capability(action) {
            self.validate_artifact_reference(scope, action, &arguments)?;
        }

        let command_id = token_hex(16).ok_or_else(|| {
            BrowserControlError::Random("CSPRNG unavailable; refusing to mint command id".into())
        })?;
        let frame = json!({
            "method": FRAME_COMMAND,
            "params": {
                "command_id": command_id,
                "action": action,
                "arguments": Value::Object(arguments),
                "controller_id": scope.controller_id,
                "browser_profile_id": scope.browser_profile_id,
                "tool_call_id": tool_call_id,
            }
        });
        let waiter = Arc::new(Waiter {
            state: Mutex::new(Outcome {
                done: false,
                cancelled: false,
                ok: false,
                result: Value::Null,
            }),
            cv: Condvar::new(),
        });

        {
            let _send_guard = sel.send_lock.lock().unwrap();
            {
                let mut inner = self.inner.lock().unwrap();
                // select() intentionally ran outside the send lock; revalidate
                // the exact live controller so a disconnect or replacement
                // cannot leave a stale command waiting.
                let live = inner
                    .controllers
                    .values()
                    .find(|c| same_scope_identity(&c.scope, scope))
                    .map(|c| c.id == sel.id && c.connected)
                    .unwrap_or(false);
                if !live {
                    return Err(BrowserControlError::Unavailable(format!(
                        "controller for scope {scope:?} detached before dispatch"
                    )));
                }
                inner.pending.insert(
                    command_id.clone(),
                    PendingCommand {
                        scope: sel.scope.clone(),
                        command_id: command_id.clone(),
                        tool_call_id: tool_call_id.clone(),
                        waiter: waiter.clone(),
                    },
                );
            }
            if let Err(err) = (sel.send)(&frame) {
                // The command never left the building; unreserve the id and
                // surface the transport failure to the caller.
                let mut inner = self.inner.lock().unwrap();
                inner.pending.remove(&command_id);
                return Err(BrowserControlError::Transport(err));
            }
        }

        // Park outside every lock. A synchronous complete() from inside send()
        // has already set the outcome, so this returns immediately in that case.
        let timed_out = {
            let timeout = Duration::from_secs_f64(self.command_timeout.max(0.0));
            let deadline = Instant::now() + timeout;
            let mut guard = waiter.state.lock().unwrap();
            loop {
                if guard.done {
                    break false;
                }
                let now = Instant::now();
                if now >= deadline {
                    break true;
                }
                let (g, res) = waiter.cv.wait_timeout(guard, deadline - now).unwrap();
                guard = g;
                if guard.done {
                    break false;
                }
                if res.timed_out() {
                    break true;
                }
            }
        };

        if timed_out {
            let claimed = {
                let mut inner = self.inner.lock().unwrap();
                // wait_timeout can return timed-out at the exact boundary where a
                // completion already won and removed the pending command.
                let still_pending = inner
                    .pending
                    .get(&command_id)
                    .map(|p| !p.waiter.state.lock().unwrap().done)
                    .unwrap_or(false);
                if still_pending {
                    let pending = inner.pending.remove(&command_id).unwrap();
                    pending.waiter.state.lock().unwrap().done = true;
                    true
                } else {
                    false
                }
            };
            if claimed {
                let cancel = cancel_frame(&command_id, tool_call_id.clone());
                let _send_guard = sel.send_lock.lock().unwrap();
                let emit_via = {
                    let mut inner = self.inner.lock().unwrap();
                    let live_scope = inner
                        .controllers
                        .iter()
                        .find(|(_, c)| same_scope_identity(&c.scope, scope))
                        .map(|(k, _)| k.clone());
                    match live_scope {
                        Some(k) => {
                            let connected = inner.controllers.get(&k).unwrap().connected;
                            if connected {
                                Some(inner.controllers.get(&k).unwrap().send.clone())
                            } else {
                                let c = inner.controllers.get_mut(&k).unwrap();
                                c.deferred_cancels.push(cancel.clone());
                                if c.deferred_cancels.len() > MAX_DEFERRED_CANCELS {
                                    let overflow = c.deferred_cancels.len() - MAX_DEFERRED_CANCELS;
                                    c.deferred_cancels.drain(0..overflow);
                                }
                                None
                            }
                        }
                        // No live controller (detached); best-effort emit via
                        // the original transport handle.
                        None => Some(sel.send.clone()),
                    }
                };
                if let Some(send) = emit_via {
                    emit_frame(&send, &cancel, &command_id);
                }
            }
            return Err(BrowserControlError::Timeout(format!(
                "controller did not complete command {command_id:?} within {}s",
                self.command_timeout
            )));
        }

        let (cancelled, ok, result) = {
            let outcome = waiter.state.lock().unwrap();
            (outcome.cancelled, outcome.ok, outcome.result.clone())
        };
        if cancelled {
            return Err(BrowserControlError::Cancelled(format!(
                "command {command_id:?} was cancelled"
            )));
        }
        if !ok {
            return Err(BrowserControlError::Rejected(format!(
                "controller rejected command {command_id:?}: {result:?}"
            )));
        }
        Ok(result)
    }

    /// Resolve a pending command by id; `false` when none is pending. Safe to
    /// call from inside the controller's own `send` callback (the broker never
    /// holds its lock across a send). Late completions after cancel/detach are
    /// ignored and report `false`.
    pub fn complete(
        &self,
        command_id: &str,
        scope: Option<&ControllerScope>,
        ok: bool,
        result: Value,
    ) -> bool {
        let mut inner = self.inner.lock().unwrap();
        let admissible = match inner.pending.get(command_id) {
            None => false,
            Some(p) => {
                let done = p.waiter.state.lock().unwrap().done;
                if done {
                    false
                } else {
                    scope.map(|s| &p.scope == s).unwrap_or(true)
                }
            }
        };
        if !admissible {
            return false;
        }
        let pending = inner.pending.remove(command_id).unwrap();
        {
            let mut outcome = pending.waiter.state.lock().unwrap();
            outcome.done = true;
            outcome.ok = ok;
            outcome.result = result;
        }
        pending.waiter.cv.notify_all();
        true
    }

    /// Cancel exactly the pending command matching `scope` + `tool_call_id`,
    /// emitting one `browser.controller.cancel` frame naming its command id.
    /// Returns `false` when nothing matched, so transports can answer
    /// idempotently.
    pub fn cancel(&self, scope: &ControllerScope, tool_call_id: Option<&str>) -> bool {
        let sel = {
            let inner = self.inner.lock().unwrap();
            inner
                .controllers
                .values()
                .find(|c| same_scope_identity(&c.scope, scope) && c.connected)
                .map(|c| (c.id, c.send.clone(), c.send_lock.clone()))
        };
        let Some((id, send, send_lock)) = sel else {
            return false;
        };
        let _send_guard = send_lock.lock().unwrap();
        let frame = {
            let mut inner = self.inner.lock().unwrap();
            let live = inner
                .controllers
                .values()
                .find(|c| same_scope_identity(&c.scope, scope))
                .map(|c| c.id == id && c.connected)
                .unwrap_or(false);
            if !live {
                return false;
            }
            let target = inner
                .pending
                .values()
                .find(|p| {
                    same_scope_identity(&p.scope, scope)
                        && p.tool_call_id.as_deref() == tool_call_id
                        && !p.waiter.state.lock().unwrap().done
                })
                .map(|p| (p.command_id.clone(), p.tool_call_id.clone()));
            let Some((cid, tcid)) = target else {
                return false;
            };
            Self::resolve_pending(&mut inner, &cid, true);
            cancel_frame(&cid, tcid)
        };
        emit_frame(&send, &frame, "");
        true
    }

    // ------------------------------------------------------------------
    // Server-owned session lookup
    // ------------------------------------------------------------------

    /// Return one unambiguous attached scope for a server-owned session. A
    /// public session id is only a lookup hint; the caller must also supply its
    /// server-derived principal and transport family. Missing identity, no
    /// match, or multiple matches fail closed.
    pub fn scope_for_session(
        &self,
        session_id: Option<&str>,
        task_id: Option<&str>,
        principal_id: Option<&str>,
        transport_family: Option<&str>,
    ) -> Option<ControllerScope> {
        let target = session_id.or(task_id).unwrap_or("").trim().to_string();
        let principal = principal_id.unwrap_or("").trim().to_string();
        let family = transport_family.unwrap_or("").trim().to_string();
        if target.is_empty() || principal.is_empty() || family.is_empty() {
            return None;
        }
        let inner = self.inner.lock().unwrap();
        let mut matches = inner.controllers.keys().filter(|s| {
            s.session_id.as_deref() == Some(target.as_str())
                && s.principal_id.as_deref() == Some(principal.as_str())
                && s.transport_family.as_deref() == Some(family.as_str())
        });
        let first = matches.next()?.clone();
        if matches.next().is_some() {
            return None;
        }
        Some(first)
    }

    /// Whether ANY controller (even offline) registered for this lane.
    /// Distinguishes "bound but currently unavailable" (fail closed) from "no
    /// controller ever registered here". Ambiguous lanes report `true`.
    pub fn lane_registered(
        &self,
        session_id: Option<&str>,
        task_id: Option<&str>,
        principal_id: Option<&str>,
        transport_family: Option<&str>,
    ) -> bool {
        let target = session_id.or(task_id).unwrap_or("").trim().to_string();
        let principal = principal_id.unwrap_or("").trim().to_string();
        let family = transport_family.unwrap_or("").trim().to_string();
        if target.is_empty() || principal.is_empty() || family.is_empty() {
            return false;
        }
        let inner = self.inner.lock().unwrap();
        inner.controllers.keys().any(|s| {
            s.session_id.as_deref() == Some(target.as_str())
                && s.principal_id.as_deref() == Some(principal.as_str())
                && s.transport_family.as_deref() == Some(family.as_str())
        })
    }

    /// Mark every controller owned by one lost transport offline.
    pub fn disconnect_owner(&self, owner: Option<Owner>) -> usize {
        let scopes: Vec<ControllerScope> = {
            let inner = self.inner.lock().unwrap();
            inner
                .controllers
                .iter()
                .filter(|(_, c)| c.owner == owner)
                .map(|(k, _)| k.clone())
                .collect()
        };
        let mut disconnected = 0;
        for scope in scopes {
            if self.disconnect(&scope, OwnerArg::Set(owner)) {
                disconnected += 1;
            }
        }
        disconnected
    }

    /// Hard-detach every controller owned by one transport connection.
    pub fn detach_owner(&self, owner: Option<Owner>, notify_controller: bool) -> usize {
        let scopes: Vec<ControllerScope> = {
            let inner = self.inner.lock().unwrap();
            inner
                .controllers
                .iter()
                .filter(|(_, c)| c.owner == owner)
                .map(|(k, _)| k.clone())
                .collect()
        };
        for scope in &scopes {
            self.detach(scope, OwnerArg::Set(owner), notify_controller);
        }
        scopes.len()
    }

    /// Fail all live work closed and clear tickets (tests/shutdown).
    pub fn reset(&self) {
        let scopes: Vec<ControllerScope> = {
            let inner = self.inner.lock().unwrap();
            inner.controllers.keys().cloned().collect()
        };
        for scope in &scopes {
            self.detach(scope, OwnerArg::Unset, true);
        }
        let mut inner = self.inner.lock().unwrap();
        inner.tickets.clear();
        // Defensive cleanup for any pending entry whose controller was
        // concurrently removed by a transport teardown.
        let ids: Vec<String> = inner.pending.keys().cloned().collect();
        for id in ids {
            Self::resolve_pending(&mut inner, &id, true);
        }
    }

    // ------------------------------------------------------------------
    // Internals
    // ------------------------------------------------------------------

    fn resolve_pending(inner: &mut InnerState, command_id: &str, cancelled: bool) {
        if let Some(pending) = inner.pending.remove(command_id) {
            {
                let mut outcome = pending.waiter.state.lock().unwrap();
                outcome.cancelled = cancelled;
                outcome.done = true;
            }
            pending.waiter.cv.notify_all();
        }
    }

    fn validate_artifact_reference(
        &self,
        scope: &ControllerScope,
        action: &str,
        arguments: &Map<String, Value>,
    ) -> Result<(), BrowserControlError> {
        let store = {
            let inner = self.inner.lock().unwrap();
            Self::artifact_store_for_scope(&inner, scope)
        };
        let Some(store) = store else {
            return Err(BrowserControlError::Rejected(format!(
                "{action} requires an attached artifact store"
            )));
        };
        let artifact_id = arguments
            .get("artifact_id")
            .and_then(|v| v.as_str())
            .map(|s| s.trim())
            .filter(|s| !s.is_empty());
        let Some(artifact_id) = artifact_id else {
            return Err(BrowserControlError::Rejected(format!(
                "{action} requires a non-empty artifact_id"
            )));
        };
        store.validate(artifact_id, scope).map_err(|exc| {
            BrowserControlError::Rejected(format!(
                "{action} rejected artifact reference {artifact_id:?}: {exc}"
            ))
        })
    }
}

fn cancel_frame(command_id: &str, tool_call_id: Option<String>) -> Value {
    json!({
        "method": FRAME_CANCEL,
        "params": {
            "command_id": command_id,
            "tool_call_id": tool_call_id,
        }
    })
}

fn emit_frame(send: &Arc<SendFn>, frame: &Value, command_id: &str) {
    if send(frame).is_err() {
        tracing::error!("failed to emit cancel frame for command {command_id:?}");
    }
}

// ----------------------------------------------------------------------
// CSPRNG-backed token minting
// ----------------------------------------------------------------------

const B64URL_ALPHABET: &[u8; 64] =
    b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

/// URL-safe base64 without padding, matching Python `secrets.token_urlsafe`.
/// 32 input bytes produce 43 characters.
fn token_urlsafe(n_bytes: usize) -> Option<String> {
    let mut buf = vec![0u8; n_bytes];
    if !fill_random(&mut buf) {
        return None;
    }
    Some(b64url_nopad(&buf))
}

/// Lowercase hex over `n_bytes` CSPRNG bytes, matching `secrets.token_hex`.
fn token_hex(n_bytes: usize) -> Option<String> {
    let mut buf = vec![0u8; n_bytes];
    if !fill_random(&mut buf) {
        return None;
    }
    Some(to_hex(&buf))
}

fn b64url_nopad(input: &[u8]) -> String {
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(B64URL_ALPHABET[((n >> 18) & 63) as usize] as char);
        out.push(B64URL_ALPHABET[((n >> 12) & 63) as usize] as char);
        if chunk.len() > 1 {
            out.push(B64URL_ALPHABET[((n >> 6) & 63) as usize] as char);
        }
        if chunk.len() > 2 {
            out.push(B64URL_ALPHABET[(n & 63) as usize] as char);
        }
    }
    out
}

fn to_hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(char::from_digit((b >> 4) as u32, 16).unwrap());
        out.push(char::from_digit((b & 0x0f) as u32, 16).unwrap());
    }
    out
}

/// Fill `buf` with kernel CSPRNG bytes. Reads /dev/urandom first, then the
/// getrandom(2) syscall on Linux. Returns false if neither is available, so the
/// caller fails closed rather than emitting a guessable value.
fn fill_random(buf: &mut [u8]) -> bool {
    if fill_from_urandom(buf).is_ok() {
        return true;
    }
    fill_from_getrandom(buf)
}

fn fill_from_urandom(buf: &mut [u8]) -> std::io::Result<()> {
    let mut f = File::open("/dev/urandom")?;
    f.read_exact(buf)
}

#[cfg(all(unix, target_os = "linux"))]
fn fill_from_getrandom(buf: &mut [u8]) -> bool {
    let mut filled = 0usize;
    while filled < buf.len() {
        let ret = unsafe {
            libc::getrandom(
                buf[filled..].as_mut_ptr() as *mut libc::c_void,
                buf.len() - filled,
                0,
            )
        };
        if ret < 0 {
            let err = std::io::Error::last_os_error();
            if err.raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            return false;
        }
        if ret == 0 {
            return false;
        }
        filled += ret as usize;
    }
    true
}

#[cfg(not(all(unix, target_os = "linux")))]
fn fill_from_getrandom(_buf: &mut [u8]) -> bool {
    false
}

// ----------------------------------------------------------------------
// Process-local global broker
// ----------------------------------------------------------------------

static GLOBAL_BROKER: OnceLock<Arc<BrowserControlBroker>> = OnceLock::new();

/// Process-local broker shared by API and dashboard Gateway transports.
pub fn get_browser_control_broker() -> Arc<BrowserControlBroker> {
    GLOBAL_BROKER
        .get_or_init(|| Arc::new(BrowserControlBroker::with_defaults()))
        .clone()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::thread;

    fn scope_with(principal: &str, controller: &str, caps: &[&str]) -> ControllerScope {
        ControllerScope {
            principal_id: Some(principal.into()),
            profile_id: Some("prof".into()),
            session_id: Some("sess".into()),
            controller_id: Some(controller.into()),
            browser_profile_id: Some("bp".into()),
            transport_family: Some("local".into()),
            capabilities: caps.iter().map(|s| s.to_string()).collect(),
        }
    }

    fn fixed_clock(value: Arc<Mutex<f64>>) -> Clock {
        Box::new(move || *value.lock().unwrap())
    }

    fn noop_send() -> Box<dyn Fn(&Value) -> Result<(), String> + Send + Sync> {
        Box::new(|_frame: &Value| Ok(()))
    }

    // --- Registration tickets ---------------------------------------------

    #[test]
    fn ticket_mint_and_consume_exactly_once() {
        let clock = Arc::new(Mutex::new(1000.0));
        let broker = BrowserControlBroker::new(30.0, 30.0, Some(fixed_clock(clock)), Some(false));
        let scope = scope_with("p", "c", &["browser_click"]);

        let ticket = broker.mint_ticket(scope.clone()).unwrap();
        assert!(ticket.value.len() >= 32);
        assert_eq!(broker.consume_ticket(&ticket.value).unwrap(), scope);

        // Second consume of the same value is rejected as already consumed.
        assert!(matches!(
            broker.consume_ticket(&ticket.value),
            Err(BrowserControlError::TicketInvalid(_))
        ));
    }

    #[test]
    fn ticket_unknown_is_invalid() {
        let broker = BrowserControlBroker::with_defaults();
        assert!(matches!(
            broker.consume_ticket("never-minted"),
            Err(BrowserControlError::TicketInvalid(_))
        ));
    }

    #[test]
    fn ticket_expires_against_live_clock() {
        let clock = Arc::new(Mutex::new(1000.0));
        let broker =
            BrowserControlBroker::new(30.0, 30.0, Some(fixed_clock(clock.clone())), Some(false));
        let ticket = broker
            .mint_ticket(scope_with("p", "c", &["browser_click"]))
            .unwrap();
        // Just past the TTL boundary.
        *clock.lock().unwrap() = 1030.001;
        assert!(matches!(
            broker.consume_ticket(&ticket.value),
            Err(BrowserControlError::TicketInvalid(_))
        ));
    }

    #[test]
    fn ticket_shape_is_csprng_urlsafe() {
        let broker = BrowserControlBroker::with_defaults();
        let a = broker
            .mint_ticket(scope_with("p", "c", &["browser_click"]))
            .unwrap();
        let b = broker
            .mint_ticket(scope_with("p", "c", &["browser_click"]))
            .unwrap();
        assert_ne!(a.value, b.value, "two mints must differ");
        assert_eq!(a.value.len(), 43, "token_urlsafe(32) shape");
        assert!(a
            .value
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_'));
    }

    #[test]
    fn csprng_is_available_on_this_host() {
        let mut buf = [0u8; 16];
        assert!(fill_random(&mut buf), "kernel CSPRNG must be reachable");
    }

    // --- Selection: exact identity and capability -------------------------

    #[test]
    fn select_matches_exact_identity_and_capability() {
        let broker = BrowserControlBroker::with_defaults();
        let scope = scope_with("alice", "c1", &["browser_click", "browser_navigate"]);
        broker.attach(scope.clone(), noop_send(), Some(7)).unwrap();

        assert!(broker.select(&scope, "browser_click").is_some());

        // A capability the controller did not negotiate is not selectable.
        assert!(broker.select(&scope, "browser_type").is_none());

        // A different principal is a different identity: no partial match.
        let other = scope_with("bob", "c1", &["browser_click"]);
        assert!(broker.select(&other, "browser_click").is_none());
    }

    #[test]
    fn developer_capability_gated_on_developer_mode() {
        let scope = scope_with("p", "c", &["browser_evaluate"]);

        let gated = BrowserControlBroker::new(30.0, 30.0, None, Some(false));
        gated.attach(scope.clone(), noop_send(), None).unwrap();
        assert!(
            gated.select(&scope, "browser_evaluate").is_none(),
            "developer capability must be unselectable with the gate off"
        );

        let open = BrowserControlBroker::new(30.0, 30.0, None, Some(true));
        open.attach(scope.clone(), noop_send(), None).unwrap();
        assert!(open.select(&scope, "browser_evaluate").is_some());
    }

    // --- Command lifecycle: single-shot completion ------------------------

    #[test]
    fn dispatch_completes_once_via_synchronous_send() {
        let broker = Arc::new(BrowserControlBroker::with_defaults());
        let scope = scope_with("p", "c", &["browser_click"]);

        // The controller completes synchronously from inside its own send.
        let seen: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        let b2 = broker.clone();
        let seen2 = seen.clone();
        let send = Box::new(move |frame: &Value| -> Result<(), String> {
            if frame["method"] == FRAME_COMMAND {
                let cid = frame["params"]["command_id"].as_str().unwrap().to_string();
                *seen2.lock().unwrap() = Some(cid.clone());
                b2.complete(&cid, None, true, json!({"echo": true}));
            }
            Ok(())
        });
        broker.attach(scope.clone(), send, None).unwrap();

        let result = broker
            .dispatch(&scope, "browser_click", None, Some("tc1".into()))
            .unwrap();
        assert_eq!(result, json!({"echo": true}));

        // A late complete for the same id is ignored (already resolved).
        let cid = seen.lock().unwrap().clone().unwrap();
        assert!(!broker.complete(&cid, None, true, json!({"late": true})));
        assert_eq!(broker.pending_count(), 0);
    }

    #[test]
    fn dispatch_rejected_when_controller_reports_not_ok() {
        let broker = Arc::new(BrowserControlBroker::with_defaults());
        let scope = scope_with("p", "c", &["browser_click"]);
        let b2 = broker.clone();
        let send = Box::new(move |frame: &Value| -> Result<(), String> {
            if frame["method"] == FRAME_COMMAND {
                let cid = frame["params"]["command_id"].as_str().unwrap().to_string();
                b2.complete(&cid, None, false, json!("boom"));
            }
            Ok(())
        });
        broker.attach(scope.clone(), send, None).unwrap();
        assert!(matches!(
            broker.dispatch(&scope, "browser_click", None, None),
            Err(BrowserControlError::Rejected(_))
        ));
    }

    #[test]
    fn dispatch_without_controller_is_unavailable() {
        let broker = BrowserControlBroker::with_defaults();
        let scope = scope_with("p", "c", &["browser_click"]);
        assert!(matches!(
            broker.dispatch(&scope, "browser_click", None, None),
            Err(BrowserControlError::Unavailable(_))
        ));
    }

    #[test]
    fn dispatch_times_out_when_never_completed() {
        // Tiny wall-clock timeout; the injected clock only drives ticket expiry.
        let broker = Arc::new(BrowserControlBroker::new(30.0, 0.05, None, Some(false)));
        let scope = scope_with("p", "c", &["browser_click"]);
        broker.attach(scope.clone(), noop_send(), None).unwrap();
        assert!(matches!(
            broker.dispatch(&scope, "browser_click", None, None),
            Err(BrowserControlError::Timeout(_))
        ));
        assert_eq!(broker.pending_count(), 0);
    }

    // --- Scoped cancellation and fail-closed detach -----------------------

    // Spawn a dispatch that parks (its send never completes) and report the
    // minted command id over a channel once the frame is emitted.
    fn spawn_parked_dispatch(
        broker: Arc<BrowserControlBroker>,
        scope: ControllerScope,
        tool_call_id: Option<String>,
    ) -> (
        thread::JoinHandle<Result<Value, BrowserControlError>>,
        String,
    ) {
        let (tx, rx) = mpsc::channel::<String>();
        let tx = Arc::new(Mutex::new(tx));
        let send = {
            let tx = tx.clone();
            Box::new(move |frame: &Value| -> Result<(), String> {
                if frame["method"] == FRAME_COMMAND {
                    let cid = frame["params"]["command_id"].as_str().unwrap().to_string();
                    let _ = tx.lock().unwrap().send(cid);
                }
                Ok(())
            })
        };
        broker.attach(scope.clone(), send, Some(1)).unwrap();
        let bt = broker.clone();
        let sc = scope.clone();
        let handle = thread::spawn(move || bt.dispatch(&sc, "browser_click", None, tool_call_id));
        let cid = rx.recv().unwrap();
        (handle, cid)
    }

    #[test]
    fn cancel_matches_scope_and_tool_call_id() {
        let broker = Arc::new(BrowserControlBroker::new(30.0, 5.0, None, Some(false)));
        let scope = scope_with("p", "c", &["browser_click"]);
        let (handle, _cid) =
            spawn_parked_dispatch(broker.clone(), scope.clone(), Some("tc-9".into()));

        // A stranger scope cancels nothing.
        let other = scope_with("mallory", "c", &["browser_click"]);
        assert!(!broker.cancel(&other, Some("tc-9")));
        // A wrong tool_call_id in the right scope cancels nothing.
        assert!(!broker.cancel(&scope, Some("tc-nope")));

        assert!(broker.cancel(&scope, Some("tc-9")));
        let outcome = handle.join().unwrap();
        assert!(matches!(outcome, Err(BrowserControlError::Cancelled(_))));
    }

    #[test]
    fn detach_fails_pending_closed_and_ignores_late_complete() {
        let broker = Arc::new(BrowserControlBroker::new(30.0, 5.0, None, Some(false)));
        let scope = scope_with("p", "c", &["browser_click"]);
        let (handle, cid) = spawn_parked_dispatch(broker.clone(), scope.clone(), None);

        broker.detach(&scope, OwnerArg::Unset, true);
        let outcome = handle.join().unwrap();
        assert!(matches!(outcome, Err(BrowserControlError::Cancelled(_))));

        // The command id is no longer pending; a late complete returns false.
        assert!(!broker.complete(&cid, None, true, json!({})));
        assert!(broker.select(&scope, "browser_click").is_none());
    }

    // --- Disconnect is recoverable; observe gated on live owner -----------

    #[test]
    fn disconnect_marks_offline_and_reattach_delivers_original_result() {
        let broker = Arc::new(BrowserControlBroker::new(30.0, 5.0, None, Some(false)));
        let scope = scope_with("p", "c", &["browser_click"]);
        let (handle, cid) = spawn_parked_dispatch(broker.clone(), scope.clone(), None);

        // Ownership is observable only while the transport is live.
        assert!(broker.is_owner(&scope, Some(1)));

        // An unexpected disconnect takes the controller offline without
        // cancelling the in-flight command.
        assert!(broker.disconnect(&scope, OwnerArg::Unset));
        assert!(broker.select(&scope, "browser_click").is_none());
        assert!(!broker.is_owner(&scope, Some(1)));
        assert!(broker.lane_registered(Some("sess"), None, Some("p"), Some("local")));

        // Re-attaching the same stable identity refreshes the callback; the
        // original command can then be completed.
        broker.attach(scope.clone(), noop_send(), Some(2)).unwrap();
        assert!(broker.complete(&cid, None, true, json!({"recovered": true})));
        assert_eq!(handle.join().unwrap().unwrap(), json!({"recovered": true}));
    }

    // --- Artifact reference validation ------------------------------------

    struct AllowValidator;
    impl ArtifactValidator for AllowValidator {
        fn validate(&self, _artifact_id: &str, _scope: &ControllerScope) -> Result<(), String> {
            Ok(())
        }
    }

    #[test]
    fn artifact_dispatch_requires_store() {
        let broker = Arc::new(BrowserControlBroker::with_defaults());
        let scope = scope_with("p", "c", &["browser_artifact_upload"]);
        broker.attach(scope.clone(), noop_send(), None).unwrap();
        let mut args = Map::new();
        args.insert("artifact_id".into(), json!("abc"));
        assert!(matches!(
            broker.dispatch(&scope, "browser_artifact_upload", Some(args), None),
            Err(BrowserControlError::Rejected(_))
        ));
    }

    #[test]
    fn artifact_dispatch_requires_non_empty_id() {
        let broker = Arc::new(BrowserControlBroker::with_defaults());
        broker.attach_artifact_store(Some(Arc::new(AllowValidator)), None);
        let scope = scope_with("p", "c", &["browser_artifact_upload"]);
        broker.attach(scope.clone(), noop_send(), None).unwrap();
        assert!(matches!(
            broker.dispatch(&scope, "browser_artifact_upload", Some(Map::new()), None),
            Err(BrowserControlError::Rejected(_))
        ));
    }

    // --- Free functions ---------------------------------------------------

    #[test]
    fn protocol_supported_rejects_bool_and_float() {
        assert!(browser_control_protocol_supported(&json!(1)));
        assert!(!browser_control_protocol_supported(&json!(true)));
        assert!(!browser_control_protocol_supported(&json!(1.0)));
        assert!(!browser_control_protocol_supported(&json!(2)));
    }

    #[test]
    fn filter_capabilities_honors_developer_gate() {
        let input = json!(["browser_click", "bogus", "browser_evaluate", 5]);
        let base = filter_browser_control_capabilities(&input, Some(false));
        assert!(base.contains("browser_click"));
        assert!(!base.contains("browser_evaluate"));
        assert!(!base.contains("bogus"));

        let dev = filter_browser_control_capabilities(&input, Some(true));
        assert!(dev.contains("browser_evaluate"));

        // Artifact caps pass without developer mode.
        let art =
            filter_browser_control_capabilities(&json!(["browser_artifact_upload"]), Some(false));
        assert!(art.contains("browser_artifact_upload"));

        // A non-array value has no capabilities.
        assert!(filter_browser_control_capabilities(&json!("nope"), Some(true)).is_empty());
    }
}
