//! Port of gateway/relay/transport.py.
//!
// Callers (the RelayAdapter and the concrete WebSocket/in-memory transports)
// are not ported yet, so the trait and its aliases are ahead of their users.
#![allow(dead_code)]
//!
//! The relay transport protocol: the gateway<->connector wire contract
//! (EXPERIMENTAL). The `RelayAdapter` (gateway side) delegates all wire I/O to a
//! `RelayTransport`. The gateway dials OUT to the connector, so a production
//! transport is a WebSocket client; in tests it is an in-memory stub. This
//! module defines the protocol surface only, no concrete transport.
//!
//! The contract has four concerns:
//!   1. Lifecycle: `connect` / `disconnect`.
//!   2. Handshake: `handshake` returns the [`CapabilityDescriptor`] the
//!      connector advertises for the platform this adapter fronts.
//!   3. Inbound: `set_inbound_handler` registers a callback the transport
//!      invokes with each normalized [`MessageEvent`] the connector delivers.
//!   4. Outbound: `send_outbound` carries send/edit/typing actions back to the
//!      connector; `get_chat_info` proxies a chat-info lookup; `send_interrupt`
//!      routes a mid-turn /stop down the socket that owns the session_key.
//!
//! Faithfulness notes:
//!
//! - Python defines `RelayTransport` as a `typing.Protocol` (a structural
//!   interface: anything with the right methods satisfies it, no explicit
//!   subclassing). The Rust analog is a trait, [`RelayTransport`], which
//!   concrete transports `impl`. This is nominal rather than structural, so a
//!   type has to name the trait; that is the closest Rust idiom.
//! - The Python Protocol is `@runtime_checkable`, meaning `isinstance(x,
//!   RelayTransport)` works at runtime (checking only that the method names
//!   exist). Rust has no equivalent runtime check; the compiler enforces the
//!   contract at build time instead, so there is nothing to port for the
//!   `runtime_checkable` decorator itself.
//! - The async methods (`connect`, `disconnect`, `handshake`, `send_outbound`,
//!   `get_chat_info`, `send_interrupt`, `go_idle`, `send_follow_up`) are
//!   modeled with `#[async_trait]`, matching how the other adapter traits in
//!   this crate (`platform::PlatformAdapter`, `agent`, ...) express async trait
//!   methods. The two callback registration methods (`set_inbound_handler`,
//!   `set_passthrough_handler`) stay synchronous, as in Python.
//! - The handler type aliases `InboundHandler` and `PassthroughHandler` are
//!   Python `Callable[..., Awaitable[None]]`. They are modeled as reference
//!   counted boxed closures returning a boxed future (see [`BoxFuture`]), so a
//!   transport can store and re-invoke them.
//! - Python's `Dict[str, Any]` action/result payloads map to
//!   `serde_json::Map<String, Value>` (a JSON object). `send_outbound` /
//!   `send_follow_up` / `get_chat_info` therefore take and return JSON objects.
//! - The `PassthroughForward` first argument is typed `Any` in Python on
//!   purpose, to keep this protocol module free of a concrete-transport import
//!   (the concrete `PassthroughForward` lives in ws_transport.py, which imports
//!   FROM this module and is not ported). It is carried as a `serde_json::Value`
//!   here, standing in for that untyped payload.
//! - Python's default arguments (`platform=None`, `reason=None`,
//!   `timeout_s=10.0`) cannot live in a Rust trait signature. `platform` and
//!   `reason` are `Option<_>` (pass `None` for the default); the `go_idle`
//!   timeout default is exposed as [`DEFAULT_GO_IDLE_TIMEOUT_S`].
//! - Trait methods take `&self`; a transport that mutates on
//!   `set_*_handler`/`connect` uses interior mutability, consistent with the
//!   other `&self` adapter traits in this crate and keeping `dyn RelayTransport`
//!   usable behind a shared reference.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Map, Value};

use crate::platform_base_types::MessageEvent;
use crate::relay_descriptor::CapabilityDescriptor;

/// The `go_idle` timeout default (Python `timeout_s: float = 10.0`). Exposed as
/// a constant because a Rust trait method cannot carry a default argument value.
pub const DEFAULT_GO_IDLE_TIMEOUT_S: f64 = 10.0;

/// A boxed, `Send` future with a `'static` payload. The return of the handler
/// closures below, standing in for Python's `Awaitable[None]`.
pub type BoxFuture = Pin<Box<dyn Future<Output = ()> + Send + 'static>>;

/// Callback the transport invokes for each inbound normalized event.
///
/// Python: `InboundHandler = Callable[[MessageEvent], Awaitable[None]]`. The
/// `Arc` makes it cheap to clone and store on the transport for repeated
/// invocation.
pub type InboundHandler = Arc<dyn Fn(MessageEvent) -> BoxFuture + Send + Sync>;

/// Callback the transport invokes for each forwarded passthrough request (§5.1).
///
/// Python: `PassthroughHandler = Callable[[Any, Optional[str]], Awaitable[None]]`.
/// The first arg is a `PassthroughForward` (concrete type in ws_transport.py),
/// typed as `Any` in Python to keep this module free of a concrete-transport
/// import; it is carried here as a [`serde_json::Value`]. The second is an
/// optional buffer id (Phase 5 §5.3 buffered flip) the handler acks after a
/// durable handoff.
pub type PassthroughHandler = Arc<dyn Fn(Value, Option<String>) -> BoxFuture + Send + Sync>;

/// Full gateway<->connector transport contract.
///
/// Port of the `RelayTransport` `Protocol`. See the module docs for how the
/// structural Protocol and its `runtime_checkable` decorator map onto a Rust
/// trait.
#[async_trait]
pub trait RelayTransport: Send + Sync {
    /// Open the connection to the connector; return true on success.
    async fn connect(&self) -> bool;

    /// Close the connection.
    async fn disconnect(&self);

    /// Return the capability descriptor the connector advertises.
    async fn handshake(&self) -> CapabilityDescriptor;

    /// Register the callback invoked with each inbound [`MessageEvent`].
    fn set_inbound_handler(&self, handler: InboundHandler);

    /// Register the callback invoked with each forwarded passthrough request.
    ///
    /// Phase 5 §5.1: the passthrough plane (Discord interactions, Twilio, ...)
    /// answers the provider's edge ACK at the connector, then forwards the real
    /// request to the gateway over this same outbound socket (a hosted gateway
    /// has no public inbound port). The transport invokes `handler(forward,
    /// buffer_id)` for each `passthrough_forward` frame. Optional on a transport
    /// (an in-memory stub may not implement it, i.e. its impl may be a no-op).
    fn set_passthrough_handler(&self, handler: PassthroughHandler);

    /// Carry an outbound action (send/edit/typing) to the connector.
    ///
    /// Returns a result object; for `op == "send"` it carries `success` and
    /// optionally `message_id` / `error`.
    ///
    /// `platform` (Phase 1.5) tags WHICH fronted platform this reply targets,
    /// carried on the OutboundFrame envelope so a gateway fronting N platforms
    /// egresses each reply through the right sender (the transport resolves the
    /// matching advertised botId). `None` means the connector falls back to the
    /// session's default platform (single-platform deploys unchanged).
    async fn send_outbound(
        &self,
        action: Map<String, Value>,
        platform: Option<String>,
    ) -> Map<String, Value>;

    /// Proxy a chat-info lookup to the connector.
    async fn get_chat_info(&self, chat_id: &str) -> Map<String, Value>;

    /// Route a mid-turn /stop to the connector for `session_key`.
    ///
    /// The connector forwards it down the socket owned by the gateway instance
    /// running that session (the /stop routing invariant). On the gateway side
    /// this is the OUTBOUND direction; the actual task cancellation happens when
    /// the connector echoes an interrupt inbound.
    async fn send_interrupt(&self, session_key: &str, reason: Option<String>);

    /// Ask the connector to flip this instance to buffered-only (Phase 5 §5.3).
    ///
    /// Sends `going_idle` and awaits the connector's `going_idle_ack`, the
    /// connector-authoritative confirmation that live delivery stopped and
    /// inbound now buffers durably for replay on reconnect (Q-5.3c). Returns true
    /// on ack, false on timeout / not-connected (the caller proceeds to close
    /// regardless; without §5.3 wiring there is simply no buffering). Optional on
    /// a transport (an in-memory stub may not implement it). Emitted as part of
    /// the gateway's EXISTING drain transition, not a new idle path. Pass
    /// [`DEFAULT_GO_IDLE_TIMEOUT_S`] for the Python default of 10.0s.
    async fn go_idle(&self, timeout_s: f64) -> bool;

    /// Act on a shared-identity capability bound to a session (A2 outbound).
    ///
    /// Some platforms hand the connector a credential that acts on the SHARED
    /// bot identity (e.g. a Discord interaction follow-up token, valid ~15min).
    /// Under A2 that credential NEVER reaches the gateway: the connector
    /// stripped it at the edge and bound it in its capability vault keyed by the
    /// session. To use it, the gateway issues a SEMANTIC action against the
    /// session it is already in; it never names or holds a token.
    ///
    /// The action object carries:
    ///   `op`          == `"follow_up"`
    ///   `session_key` the session whose bound capability to wield
    ///   `kind`        the capability kind (e.g. `"discord.interaction_token"`)
    ///   `content`     the message content to send via that capability
    ///   `metadata?`   optional extras
    ///
    /// The connector resolves the real capability (`resolveOutboundCapability`
    /// on its side), enforces the tenant match (tenant B can never wield tenant
    /// A's capability), and egresses. Returns `{success, message_id?, error?}`;
    /// `success` is false when the capability is absent/expired or the tenant
    /// does not match, and the gateway then has nothing to retry with (by
    /// design: a leaked gateway holds zero capability material).
    async fn send_follow_up(
        &self,
        action: Map<String, Value>,
        platform: Option<String>,
    ) -> Map<String, Value>;
}

#[cfg(test)]
mod tests {
    // This module is a Protocol/trait definition with no concrete logic to
    // pin against golden Python values (there are no parsers or pure functions
    // here, only the interface). The test instead builds a minimal in-memory
    // stub transport, which locks the trait's shape: object safety behind
    // `dyn`, the handler alias types being storable and re-invocable, and the
    // async method signatures all lining up.
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    #[derive(Default)]
    struct StubTransport {
        connected: AtomicUsize,
        inbound: Mutex<Option<InboundHandler>>,
        passthrough: Mutex<Option<PassthroughHandler>>,
    }

    #[async_trait]
    impl RelayTransport for StubTransport {
        async fn connect(&self) -> bool {
            self.connected.fetch_add(1, Ordering::SeqCst);
            true
        }

        async fn disconnect(&self) {
            self.connected.store(0, Ordering::SeqCst);
        }

        async fn handshake(&self) -> CapabilityDescriptor {
            CapabilityDescriptor::new(
                1, "stub", "Stub", 4096, false, false, false, "plain", "chars",
            )
        }

        fn set_inbound_handler(&self, handler: InboundHandler) {
            *self.inbound.lock().unwrap() = Some(handler);
        }

        fn set_passthrough_handler(&self, handler: PassthroughHandler) {
            *self.passthrough.lock().unwrap() = Some(handler);
        }

        async fn send_outbound(
            &self,
            action: Map<String, Value>,
            platform: Option<String>,
        ) -> Map<String, Value> {
            let mut out = Map::new();
            out.insert("success".into(), Value::Bool(true));
            out.insert(
                "op".into(),
                action.get("op").cloned().unwrap_or(Value::Null),
            );
            out.insert(
                "platform".into(),
                platform.map(Value::String).unwrap_or(Value::Null),
            );
            out
        }

        async fn get_chat_info(&self, chat_id: &str) -> Map<String, Value> {
            let mut out = Map::new();
            out.insert("chat_id".into(), Value::String(chat_id.to_string()));
            out
        }

        async fn send_interrupt(&self, _session_key: &str, _reason: Option<String>) {}

        async fn go_idle(&self, _timeout_s: f64) -> bool {
            true
        }

        async fn send_follow_up(
            &self,
            _action: Map<String, Value>,
            _platform: Option<String>,
        ) -> Map<String, Value> {
            let mut out = Map::new();
            out.insert("success".into(), Value::Bool(false));
            out
        }
    }

    #[tokio::test]
    async fn stub_transport_is_object_safe_and_drives_the_contract() {
        let stub: Box<dyn RelayTransport> = Box::new(StubTransport::default());

        assert!(stub.connect().await);
        let desc = stub.handshake().await;
        assert_eq!(desc.platform, "stub");

        // send_outbound echoes op and platform tag.
        let mut action = Map::new();
        action.insert("op".into(), Value::String("send".into()));
        let res = stub.send_outbound(action, Some("discord".into())).await;
        assert_eq!(res.get("success"), Some(&Value::Bool(true)));
        assert_eq!(res.get("op"), Some(&Value::String("send".into())));
        assert_eq!(res.get("platform"), Some(&Value::String("discord".into())));

        // platform=None falls through to the null default.
        let res = stub.send_outbound(Map::new(), None).await;
        assert_eq!(res.get("platform"), Some(&Value::Null));

        let info = stub.get_chat_info("c1").await;
        assert_eq!(info.get("chat_id"), Some(&Value::String("c1".into())));

        stub.send_interrupt("sess", None).await;
        assert!(stub.go_idle(DEFAULT_GO_IDLE_TIMEOUT_S).await);

        let follow = stub.send_follow_up(Map::new(), None).await;
        assert_eq!(follow.get("success"), Some(&Value::Bool(false)));

        stub.disconnect().await;
    }

    #[tokio::test]
    async fn handlers_are_storable_and_invocable() {
        let stub = StubTransport::default();

        // Inbound handler: records the event text into a shared counter.
        let seen = Arc::new(AtomicUsize::new(0));
        let seen_cl = seen.clone();
        let handler: InboundHandler = Arc::new(move |_ev: MessageEvent| {
            let seen = seen_cl.clone();
            Box::pin(async move {
                seen.fetch_add(1, Ordering::SeqCst);
            }) as BoxFuture
        });
        stub.set_inbound_handler(handler);

        // Pull it back out and invoke it, mimicking what a transport does per
        // inbound frame.
        let stored = stub.inbound.lock().unwrap().clone();
        let stored = stored.expect("inbound handler was registered");
        stored(MessageEvent::new("hi")).await;
        assert_eq!(seen.load(Ordering::SeqCst), 1);

        // Passthrough handler: takes a JSON forward plus an optional buffer id.
        let acked: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        let acked_cl = acked.clone();
        let pt: PassthroughHandler = Arc::new(move |_fwd: Value, buffer_id: Option<String>| {
            let acked = acked_cl.clone();
            Box::pin(async move {
                *acked.lock().unwrap() = buffer_id;
            }) as BoxFuture
        });
        stub.set_passthrough_handler(pt);

        let stored = stub.passthrough.lock().unwrap().clone();
        let stored = stored.expect("passthrough handler was registered");
        stored(Value::Null, Some("buf-7".into())).await;
        assert_eq!(acked.lock().unwrap().as_deref(), Some("buf-7"));
    }
}
