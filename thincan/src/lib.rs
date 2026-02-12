//! thincan: application-layer messages over ISO-TP.
//!
//! `thincan` is a tiny application-layer protocol intended to sit *above* ISO-TP: once an
//! ISO-TP implementation has delivered a payload, `thincan` defines how that payload is
//! interpreted as an application message and routed to a handler.
//!
//! The crate provides:
//! - A tiny wire format (`u16` message id + body bytes).
//! - Macros to declare a message registry (**atlas**) via [`bus_atlas!`].
//! - Macros to group message handlers into reusable (**bundle**) modules via [`bundle!`].
//! - Macros to compose bundles + application handlers into per-device (**maplet**) routers via
//!   [`bus_maplet!`].
//! - Optional send/receive helpers ([`Interface`]) that integrate with an ISO-TP node/transport.
//!
//! `thincan` intentionally does **not** implement ISO-TP framing/flow-control. Any ISO-TP
//! implementation (including the sibling `can-iso-tp` crate) can be used as the transport layer.
//!
//! # Concepts
//! - **Atlas**: the global registry of message ids and names for a "bus".
//! - **Bundle**: a reusable unit of parsing + handler methods for a subset of atlas messages.
//! - **Maplet**: a per-firmware/per-device view of the bus: which messages exist, which are
//!   handled by the application, which are handled by bundles, and which should be ignored.
//!
//! # Quick start (atlas → bundle → maplet → dispatch)
//! ```rust
//! use thincan::{bus_atlas, bus_maplet, bundle, DispatchOutcome, Message, RawMessage, RouterDispatch};
//!
//! bus_atlas! {
//!     pub mod atlas {
//!         0x0100 => Ping(len = 1);
//!         0x0101 => Pong(len = 2);
//!     }
//! }
//!
//! bundle! {
//!     pub mod demo_bundle(atlas) {
//!         parser: thincan::DefaultParser;
//!         use msgs [Ping, Pong];
//!         handles { Ping => on_ping }
//!     }
//! }
//!
//! bus_maplet! {
//!     pub mod demo_maplet: atlas {
//!         bundles [demo_bundle];
//!         parser: thincan::DefaultParser;
//!         use msgs [Ping, Pong];
//!         handles { Pong => on_pong }
//!         unhandled_by_default = true;
//!         ignore [];
//!     }
//! }
//!
//! #[derive(Default)]
//! struct Handlers;
//!
//! impl<'a> demo_bundle::Handlers<'a> for Handlers {
//!     type Error = ();
//!     fn on_ping(&mut self, _msg: &'a [u8; atlas::Ping::BODY_LEN]) -> Result<(), Self::Error> {
//!         Ok(())
//!     }
//! }
//!
//! impl<'a> demo_maplet::Handlers<'a> for Handlers {
//!     type Error = ();
//!     fn on_pong(&mut self, _msg: &'a [u8; atlas::Pong::BODY_LEN]) -> Result<(), Self::Error> {
//!         Ok(())
//!     }
//! }
//!
//! fn main() -> Result<(), thincan::Error> {
//!     let mut router = demo_maplet::Router::new();
//!     let mut handlers = demo_maplet::HandlersImpl {
//!         app: Handlers::default(),
//!         demo_bundle: Handlers::default(),
//!     };
//!
//!     let msg = RawMessage {
//!         id: <atlas::Ping as Message>::ID,
//!         body: &[0u8; atlas::Ping::BODY_LEN],
//!     };
//!     assert_eq!(router.dispatch(&mut handlers, msg)?, DispatchOutcome::Handled);
//!     Ok(())
//! }
//! ```
//!
//! # Wire format
//! - 2 bytes: message id (`u16`, little endian)
//! - N bytes: message body (fixed-length or schema-encoded)
//!
//! The helper [`decode_wire()`] parses an incoming payload into a [`RawMessage`]. When sending,
//! [`Interface::send_msg()`] and [`Interface::send_encoded()`] take care of writing the header.
//!
//! # Feature flags
//! - `std` (default): enables `std::error::Error` and `Display` for [`Error`].
//! - `async`: enables async helpers for `can-iso-tp`'s async node.
//! - `capnp`: enables Cap'n Proto decode helpers (`Capnp` / `CapnpTyped`).
//! - `isotp-uds`: enables UDS 29-bit ISO-TP demux transport helpers (reply-to metadata).
//!
//! # More examples
//! ### `no_std` wiring example (from `tests/no_std_smoke.rs`)
#![doc = "```rust,ignore"]
#![doc = include_str!("../tests/no_std_smoke.rs")]
#![doc = "```"]
//!
//! ### Cap'n Proto round-trip example (from `tests/capnp_person.rs`)
#![doc = "```rust,ignore"]
#![doc = include_str!("../tests/capnp_person.rs")]
#![doc = "```"]
#![cfg_attr(not(feature = "std"), no_std)]

use core::marker::PhantomData;
use core::time::Duration;

/// Simple message metadata (id + body length).
///
/// This is a lightweight helper for tooling and documentation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MessageSpec {
    /// 16-bit message id.
    pub id: u16,
    /// Message body size in bytes.
    pub body_size: usize,
}

/// Marker trait for message types declared in a bus atlas.
///
/// Each message has a globally unique `ID` within the atlas, and an optional
/// fixed-length `BODY_LEN` for validation.
pub trait Message {
    /// 16-bit message id written to the wire (little endian).
    const ID: u16;
    /// Fixed body length in bytes, if known at compile time.
    ///
    /// Messages declared via `bus_atlas!(... len = N ...)` set this to `Some(N)`.
    /// Messages declared via `bus_atlas!(... capnp = ... )` leave this as `None`.
    const BODY_LEN: Option<usize> = None;
}

/// Simple parse trait for converting a raw body into a typed view.
///
/// Most code uses [`MessageParser`] + [`DefaultParser`] instead. This is kept
/// for convenience when you want a message-local parser.
pub trait Parse<'a>: Message {
    /// Parsed representation of the message body.
    type Parsed;
    /// Parse a body into a typed representation.
    fn parse(body: &'a [u8]) -> Result<Self::Parsed, Error>;
}

/// Default parser used by `bus_atlas!` for `len = N` and `capnp = ...` messages.
#[derive(Debug, Clone, Copy, Default)]
pub struct DefaultParser;

/// Parser abstraction used by bundles and maplets.
///
/// The parser is generic over a message type and can return a borrowed view
/// (via the GAT `Parsed<'a>`).
pub trait MessageParser<M: Message> {
    /// Parsed representation of `M` that may borrow the input buffer.
    type Parsed<'a>;
    /// Parse a raw body into a typed representation.
    fn parse<'a>(&self, body: &'a [u8]) -> Result<Self::Parsed<'a>, Error>;
}

/// Convenience extension trait for calling `parse_msg` on any parser.
pub trait ParseExt {
    /// Parse a message body using the parser implementation for `M`.
    fn parse_msg<'a, M: Message>(
        &self,
        body: &'a [u8],
    ) -> Result<<Self as MessageParser<M>>::Parsed<'a>, Error>
    where
        Self: MessageParser<M>,
    {
        <Self as MessageParser<M>>::parse(self, body)
    }
}

impl<T> ParseExt for T {}

/// Error classification returned by `thincan` operations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    /// Transport timed out waiting for a payload.
    Timeout,
    /// A caller-provided buffer was too small for the operation.
    BufferTooSmall {
        /// Needed length in bytes.
        needed: usize,
        /// Available length in bytes.
        got: usize,
    },
    /// Body length did not match the expected fixed-length message size.
    InvalidBodyLen {
        /// Expected body length in bytes.
        expected: usize,
        /// Received body length in bytes.
        got: usize,
    },
    /// Catch-all for other errors (parsing, protocol, or transport).
    Other,
}

/// Error type returned by most `thincan` operations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Error {
    /// Structured error classification.
    pub kind: ErrorKind,
}

impl Error {
    /// Convenience constructor for a timeout error.
    pub const fn timeout() -> Self {
        Self {
            kind: ErrorKind::Timeout,
        }
    }
}

#[cfg(feature = "std")]
impl std::error::Error for Error {}

#[cfg(feature = "std")]
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}", self.kind)
    }
}

/// A raw message as received on the wire (already framed/deframed at the ISO-TP layer).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RawMessage<'a> {
    /// Message id.
    pub id: u16,
    /// Raw body bytes (without the 2-byte id header).
    pub body: &'a [u8],
}

/// A convenient wrapper for "typed decode helpers" (e.g. Cap'n Proto readers) that borrow a buffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Capnp<'a> {
    /// Raw message bytes.
    pub bytes: &'a [u8],
}

/// A schema-typed Cap'n Proto decode helper that borrows a buffer.
///
/// The primary DX goal is that RX handlers can receive a *typed* value, while still supporting
/// Cap'n Proto's zero-copy reading model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CapnpTyped<'a, Schema>(
    /// Raw bytes wrapped as a Cap'n Proto helper.
    pub Capnp<'a>,
    PhantomData<Schema>,
);

impl<'a, Schema> CapnpTyped<'a, Schema> {
    /// Wrap raw bytes in a schema-typed Cap'n Proto reader.
    pub const fn new(bytes: &'a [u8]) -> Self {
        Self(Capnp::new(bytes), PhantomData)
    }

    #[cfg(feature = "capnp")]
    /// Read the typed root with the provided reader options.
    pub fn with_root<R>(
        &self,
        options: capnp::message::ReaderOptions,
        f: impl FnOnce(<Schema as capnp::traits::Owned>::Reader<'_>) -> R,
    ) -> Result<R, capnp::Error>
    where
        Schema: capnp::traits::Owned,
    {
        self.0.with_root::<Schema, R>(options, f)
    }
}

impl<'a> Capnp<'a> {
    /// Wrap raw bytes in a Cap'n Proto reader helper.
    pub const fn new(bytes: &'a [u8]) -> Self {
        Self { bytes }
    }

    #[cfg(feature = "capnp")]
    /// Construct a `capnp::message::Reader` over a single segment.
    pub fn with_reader<R>(
        &self,
        options: capnp::message::ReaderOptions,
        f: impl FnOnce(capnp::message::Reader<&[&[u8]]>) -> R,
    ) -> R {
        let segments: [&[u8]; 1] = [self.bytes];
        let reader = capnp::message::Reader::new(&segments[..], options);
        f(reader)
    }

    #[cfg(feature = "capnp")]
    /// Read the typed root with the provided reader options.
    pub fn with_root<O, R>(
        &self,
        options: capnp::message::ReaderOptions,
        f: impl FnOnce(<O as capnp::traits::Owned>::Reader<'_>) -> R,
    ) -> Result<R, capnp::Error>
    where
        O: capnp::traits::Owned,
    {
        self.with_reader(options, |reader| {
            let typed = capnp::message::TypedReader::<_, O>::new(reader);
            let root = typed.get()?;
            Ok(f(root))
        })
    }
}

/// Result of dispatching a decoded message to a maplet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DispatchOutcome<'a, Unhandled = RawMessage<'a>> {
    /// A message was decoded and routed to its unique handler.
    Handled,
    /// The message ID was known and decodable in this maplet, but intentionally not routed.
    Unhandled(Unhandled),
    /// The message ID was not recognized for this maplet (or not supported by this firmware).
    Unknown { id: u16, body: &'a [u8] },
}

#[doc(hidden)]
pub const fn __assert_unique_u16_slices<const N: usize>(slices: [&[u16]; N]) {
    let mut s1 = 0usize;
    while s1 < N {
        let a = slices[s1];
        let mut i = 0usize;
        while i < a.len() {
            let v = a[i];

            let mut s2 = s1;
            let mut j = i + 1;
            while s2 < N {
                let b = slices[s2];
                while j < b.len() {
                    if b[j] == v {
                        panic!("duplicate handled message id");
                    }
                    j += 1;
                }
                s2 += 1;
                j = 0;
            }

            i += 1;
        }
        s1 += 1;
    }
}

/// Number of bytes used for the message id header.
pub const HEADER_LEN: usize = 2;

/// Decode a raw ISO-TP payload into a [`RawMessage`].
///
/// Expects the payload to start with a 2-byte little-endian message id.
pub fn decode_wire(payload: &[u8]) -> Result<RawMessage<'_>, Error> {
    if payload.len() < HEADER_LEN {
        return Err(Error {
            kind: ErrorKind::Other,
        });
    }
    let id = u16::from_le_bytes([payload[0], payload[1]]);
    Ok(RawMessage {
        id,
        body: &payload[HEADER_LEN..],
    })
}

/// A trait for message bodies that can be encoded for transmission.
///
/// This is intentionally implemented for *value types* (often local to a bundle),
/// rather than for the atlas message type itself, so bundles can be composed
/// without requiring orphan-rule-breaking impls.
pub trait Encode<M: Message> {
    /// Upper bound on the number of bytes `encode()` may write.
    ///
    /// Implementations must ensure `encode()` never returns a value greater than this.
    fn max_encoded_len(&self) -> usize;

    /// Encodes into `out` and returns the number of bytes written.
    ///
    /// The returned length is the body length (it does not include the 2-byte header).
    fn encode(&self, out: &mut [u8]) -> Result<usize, Error>;
}

impl<M: Message> Encode<M> for &[u8] {
    fn max_encoded_len(&self) -> usize {
        self.len()
    }

    fn encode(&self, out: &mut [u8]) -> Result<usize, Error> {
        if out.len() < self.len() {
            return Err(Error {
                kind: ErrorKind::Other,
            });
        }
        out[..self.len()].copy_from_slice(self);
        Ok(self.len())
    }
}

impl<const N: usize, M: Message> Encode<M> for &[u8; N] {
    fn max_encoded_len(&self) -> usize {
        N
    }

    fn encode(&self, out: &mut [u8]) -> Result<usize, Error> {
        if out.len() < N {
            return Err(Error {
                kind: ErrorKind::Other,
            });
        }
        out[..N].copy_from_slice(&self[..]);
        Ok(N)
    }
}

/// Receive control returned by transport callbacks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecvControl {
    /// Continue normal operation.
    Continue,
    /// Stop early (transport may ignore this for single-delivery APIs).
    Stop,
}

/// Status returned by receive operations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecvStatus {
    /// A single payload was delivered.
    DeliveredOne,
    /// No payload arrived before the timeout.
    TimedOut,
}

/// Event emitted during receive+dispatch.
///
/// All borrowed data is only valid for the duration of the callback.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DispatchEvent<'a> {
    /// The received payload could not be decoded as a `thincan` message (missing/short header).
    MalformedPayload { payload: &'a [u8] },
    /// A payload was decoded and dispatched through the router.
    Dispatched {
        raw: RawMessage<'a>,
        outcome: DispatchOutcome<'a>,
    },
}

/// Event emitted during receive+dispatch when the transport provides reply-to metadata.
///
/// All borrowed data is only valid for the duration of the callback.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DispatchEventMeta<'a, A>
where
    A: Copy,
{
    /// The received payload could not be decoded as a `thincan` message (missing/short header).
    MalformedPayload {
        meta: RecvMeta<A>,
        payload: &'a [u8],
    },
    /// A payload was decoded and dispatched through the router.
    Dispatched {
        meta: RecvMeta<A>,
        raw: RawMessage<'a>,
        outcome: DispatchOutcome<'a>,
    },
}

/// Receive metadata provided by transports that can identify the sender.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RecvMeta<A> {
    /// Address to use when replying to this payload.
    pub reply_to: A,
}

fn map_isotp_send_error<E>(err: can_isotp_interface::SendError<E>) -> Error {
    Error {
        kind: match err {
            can_isotp_interface::SendError::Timeout => ErrorKind::Timeout,
            can_isotp_interface::SendError::Backend(_) => ErrorKind::Other,
        },
    }
}

fn map_isotp_recv_error<E>(_: can_isotp_interface::RecvError<E>) -> Error {
    Error {
        kind: ErrorKind::Other,
    }
}

fn map_isotp_control(control: RecvControl) -> can_isotp_interface::RecvControl {
    match control {
        RecvControl::Continue => can_isotp_interface::RecvControl::Continue,
        RecvControl::Stop => can_isotp_interface::RecvControl::Stop,
    }
}

/// Optional extension trait: configure ISO-TP receive-side FlowControl (BS/STmin) for backpressure.
pub trait RxFlowControlConfig {
    fn set_rx_flow_control(&mut self, fc: can_isotp_interface::RxFlowControl) -> Result<(), Error>;
}

impl<T> RxFlowControlConfig for T
where
    T: can_isotp_interface::IsoTpRxFlowControlConfig,
{
    fn set_rx_flow_control(&mut self, fc: can_isotp_interface::RxFlowControl) -> Result<(), Error> {
        can_isotp_interface::IsoTpRxFlowControlConfig::set_rx_flow_control(self, fc).map_err(|_| {
            Error {
                kind: ErrorKind::Other,
            }
        })
    }
}

/// Dispatch raw messages into bundle/app handlers.
pub trait RouterDispatch<Handlers> {
    /// Decode and route a raw message into the appropriate handler.
    fn dispatch<'a>(
        &mut self,
        handlers: &mut Handlers,
        msg: RawMessage<'a>,
    ) -> Result<DispatchOutcome<'a>, Error>;
}

/// Bundle metadata independent of the handler implementation.
///
/// This allows `bus_maplet!` to reason about IDs (e.g. duplicate detection) without needing to
/// know the concrete handler types.
pub trait BundleMeta<Atlas> {
    /// Parser used for this bundle.
    type Parser: Default;
    /// Message IDs handled by this bundle (must be unique across bundles).
    const HANDLED_IDS: &'static [u16];
    /// Message IDs used by this bundle (handled + referenced).
    const USED_IDS: &'static [u16];
    /// Returns true if this bundle recognizes the given message id.
    fn is_known(id: u16) -> bool;
}

/// Bundle dispatch for a specific handler type.
pub trait BundleDispatch<Atlas, Handlers>: BundleMeta<Atlas> {
    /// Attempt to dispatch a raw message; return `None` if this bundle doesn't handle it.
    fn try_dispatch<'a>(
        parser: &Self::Parser,
        handlers: &mut Handlers,
        msg: RawMessage<'a>,
    ) -> Option<Result<(), Error>>;
}

/// Summary of a single receive+dispatch cycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecvDispatch {
    /// No payload arrived before timeout.
    TimedOut,
    /// A payload arrived but was not a valid `thincan` wire message.
    MalformedPayload,
    /// A message was decoded and dispatched.
    Dispatched { id: u16, kind: RecvDispatchKind },
}

/// Minimal classification of dispatch results (no borrowed body).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecvDispatchKind {
    Handled,
    Unhandled,
    Unknown,
}

/// High-level interface that couples a transport node and a generated maplet router.
///
/// This interface is designed to be allocation-free: callers supply a TX scratch buffer and
/// receive-side dispatch happens directly inside the transport's receive callback.
#[derive(Debug)]
pub struct Interface<Node, Router, TxBuf> {
    node: Node,
    router: Router,
    tx: TxBuf,
}

impl<Node, Router, TxBuf> Interface<Node, Router, TxBuf>
where
    TxBuf: AsMut<[u8]>,
{
    /// Create a new interface from a transport node, a maplet router, and a caller-provided TX buffer.
    pub fn new(node: Node, router: Router, tx: TxBuf) -> Self {
        Self { node, router, tx }
    }

    /// Borrow the transport node.
    pub fn node(&self) -> &Node {
        &self.node
    }

    /// Mutably borrow the transport node (e.g. to reconfigure).
    pub fn node_mut(&mut self) -> &mut Node {
        &mut self.node
    }

    /// Borrow the router.
    pub fn router(&self) -> &Router {
        &self.router
    }

    /// Mutably borrow the router.
    pub fn router_mut(&mut self) -> &mut Router {
        &mut self.router
    }

    fn encode_msg_into_buf<'b, M: Message>(
        buf: &'b mut [u8],
        body: &[u8],
    ) -> Result<&'b [u8], Error> {
        if let Some(expected) = M::BODY_LEN
            && body.len() != expected
        {
            return Err(Error {
                kind: ErrorKind::InvalidBodyLen {
                    expected,
                    got: body.len(),
                },
            });
        }

        let needed = HEADER_LEN + body.len();
        if buf.len() < needed {
            return Err(Error {
                kind: ErrorKind::BufferTooSmall {
                    needed,
                    got: buf.len(),
                },
            });
        }
        buf[..HEADER_LEN].copy_from_slice(&M::ID.to_le_bytes());
        buf[HEADER_LEN..needed].copy_from_slice(body);
        Ok(&buf[..needed])
    }

    fn encode_value_into_buf<'b, M: Message, V: Encode<M>>(
        buf: &'b mut [u8],
        value: &V,
    ) -> Result<&'b [u8], Error> {
        let max_len = value.max_encoded_len();
        let needed = HEADER_LEN + max_len;
        if buf.len() < needed {
            return Err(Error {
                kind: ErrorKind::BufferTooSmall {
                    needed,
                    got: buf.len(),
                },
            });
        }

        buf[..HEADER_LEN].copy_from_slice(&M::ID.to_le_bytes());
        let used = value.encode(&mut buf[HEADER_LEN..HEADER_LEN + max_len])?;
        if used > max_len {
            return Err(Error {
                kind: ErrorKind::Other,
            });
        }
        if let Some(expected) = M::BODY_LEN
            && used != expected
        {
            return Err(Error {
                kind: ErrorKind::InvalidBodyLen {
                    expected,
                    got: used,
                },
            });
        }
        Ok(&buf[..HEADER_LEN + used])
    }

    /// Encode a raw body for message `M` into the caller-provided TX buffer.
    pub fn encode_msg_into<M: Message>(&mut self, body: &[u8]) -> Result<&[u8], Error> {
        Self::encode_msg_into_buf::<M>(self.tx.as_mut(), body)
    }

    /// Encode a typed value for message `M` into the caller-provided TX buffer.
    pub fn encode_value_into<M: Message, V: Encode<M>>(
        &mut self,
        value: &V,
    ) -> Result<&[u8], Error> {
        Self::encode_value_into_buf::<M, V>(self.tx.as_mut(), value)
    }

    /// Send a raw body for message `M` (validates `BODY_LEN` if present).
    pub fn send_msg<M: Message>(&mut self, body: &[u8], timeout: Duration) -> Result<(), Error>
    where
        Node: can_isotp_interface::IsoTpEndpoint,
    {
        let (node, tx) = (&mut self.node, &mut self.tx);
        let payload = Self::encode_msg_into_buf::<M>(tx.as_mut(), body)?;
        node.send(payload, timeout).map_err(map_isotp_send_error)
    }

    /// Encode and send a typed value for message `M`.
    pub fn send_encoded<M: Message, V: Encode<M>>(
        &mut self,
        value: &V,
        timeout: Duration,
    ) -> Result<(), Error>
    where
        Node: can_isotp_interface::IsoTpEndpoint,
    {
        let (node, tx) = (&mut self.node, &mut self.tx);
        let payload = Self::encode_value_into_buf::<M, V>(tx.as_mut(), value)?;
        node.send(payload, timeout).map_err(map_isotp_send_error)
    }

    /// Send a raw body for message `M` to a specific transport address.
    pub fn send_msg_to<M: Message>(
        &mut self,
        to: Node::ReplyTo,
        body: &[u8],
        timeout: Duration,
    ) -> Result<(), Error>
    where
        Node: can_isotp_interface::IsoTpEndpointMeta,
    {
        let (node, tx) = (&mut self.node, &mut self.tx);
        let payload = Self::encode_msg_into_buf::<M>(tx.as_mut(), body)?;
        node.send_to(to, payload, timeout)
            .map_err(map_isotp_send_error)
    }

    /// Encode and send a typed value for message `M` to a specific transport address.
    pub fn send_encoded_to<M: Message, V: Encode<M>>(
        &mut self,
        to: Node::ReplyTo,
        value: &V,
        timeout: Duration,
    ) -> Result<(), Error>
    where
        Node: can_isotp_interface::IsoTpEndpointMeta,
    {
        let (node, tx) = (&mut self.node, &mut self.tx);
        let payload = Self::encode_value_into_buf::<M, V>(tx.as_mut(), value)?;
        node.send_to(to, payload, timeout)
            .map_err(map_isotp_send_error)
    }

    /// Receive at most one payload and dispatch it through the router.
    ///
    /// This returns a small owned summary. Use [`Interface::recv_one_dispatch_with`] if you need
    /// borrowed access to bodies (unknown/unhandled payload bytes) for logging or tooling.
    pub fn recv_one_dispatch<Handlers>(
        &mut self,
        handlers: &mut Handlers,
        timeout: Duration,
    ) -> Result<RecvDispatch, Error>
    where
        Node: can_isotp_interface::IsoTpEndpoint,
        Router: RouterDispatch<Handlers>,
    {
        self.recv_one_dispatch_with(handlers, timeout, |_| Ok(RecvControl::Continue))
    }

    /// Receive at most one payload, dispatch it, and emit structured events.
    pub fn recv_one_dispatch_with<Handlers, F>(
        &mut self,
        handlers: &mut Handlers,
        timeout: Duration,
        mut on_event: F,
    ) -> Result<RecvDispatch, Error>
    where
        Node: can_isotp_interface::IsoTpEndpoint,
        Router: RouterDispatch<Handlers>,
        F: for<'a> FnMut(DispatchEvent<'a>) -> Result<RecvControl, Error>,
    {
        let (node, router) = (&mut self.node, &mut self.router);

        let mut summary = RecvDispatch::TimedOut;
        let mut cb_err: Option<Error> = None;
        let status = node
            .recv_one(timeout, |payload| {
                if cb_err.is_some() {
                    return Ok(can_isotp_interface::RecvControl::Stop);
                }
                let res: Result<RecvControl, Error> = match decode_wire(payload) {
                    Ok(raw) => match router.dispatch(handlers, raw) {
                        Ok(outcome) => {
                            summary = RecvDispatch::Dispatched {
                                id: raw.id,
                                kind: match outcome {
                                    DispatchOutcome::Handled => RecvDispatchKind::Handled,
                                    DispatchOutcome::Unhandled(_) => RecvDispatchKind::Unhandled,
                                    DispatchOutcome::Unknown { .. } => RecvDispatchKind::Unknown,
                                },
                            };
                            on_event(DispatchEvent::Dispatched { raw, outcome })
                        }
                        Err(err) => Err(err),
                    },
                    Err(_) => {
                        summary = RecvDispatch::MalformedPayload;
                        on_event(DispatchEvent::MalformedPayload { payload })
                    }
                };
                match res {
                    Ok(control) => Ok(map_isotp_control(control)),
                    Err(err) => {
                        cb_err = Some(err);
                        Ok(can_isotp_interface::RecvControl::Stop)
                    }
                }
            })
            .map_err(map_isotp_recv_error)?;

        if let Some(err) = cb_err {
            return Err(err);
        }

        Ok(match status {
            can_isotp_interface::RecvStatus::TimedOut => RecvDispatch::TimedOut,
            can_isotp_interface::RecvStatus::DeliveredOne => summary,
        })
    }

    /// Receive at most one payload and dispatch it through the router, with reply-to metadata.
    pub fn recv_one_dispatch_meta<Handlers>(
        &mut self,
        handlers: &mut Handlers,
        timeout: Duration,
    ) -> Result<RecvDispatch, Error>
    where
        Node: can_isotp_interface::IsoTpEndpointMeta,
        Router: RouterDispatch<Handlers>,
    {
        self.recv_one_dispatch_with_meta(handlers, timeout, |_| Ok(RecvControl::Continue))
    }

    /// Receive at most one payload, dispatch it, and emit structured events with reply-to metadata.
    pub fn recv_one_dispatch_with_meta<Handlers, F>(
        &mut self,
        handlers: &mut Handlers,
        timeout: Duration,
        mut on_event: F,
    ) -> Result<RecvDispatch, Error>
    where
        Node: can_isotp_interface::IsoTpEndpointMeta,
        Router: RouterDispatch<Handlers>,
        F: for<'a> FnMut(DispatchEventMeta<'a, Node::ReplyTo>) -> Result<RecvControl, Error>,
    {
        let (node, router) = (&mut self.node, &mut self.router);

        let mut summary = RecvDispatch::TimedOut;
        let mut cb_err: Option<Error> = None;
        let status = node
            .recv_one_meta(timeout, |meta, payload| {
                if cb_err.is_some() {
                    return Ok(can_isotp_interface::RecvControl::Stop);
                }
                let meta = RecvMeta {
                    reply_to: meta.reply_to,
                };
                let res: Result<RecvControl, Error> = match decode_wire(payload) {
                    Ok(raw) => match router.dispatch(handlers, raw) {
                        Ok(outcome) => {
                            summary = RecvDispatch::Dispatched {
                                id: raw.id,
                                kind: match outcome {
                                    DispatchOutcome::Handled => RecvDispatchKind::Handled,
                                    DispatchOutcome::Unhandled(_) => RecvDispatchKind::Unhandled,
                                    DispatchOutcome::Unknown { .. } => RecvDispatchKind::Unknown,
                                },
                            };
                            on_event(DispatchEventMeta::Dispatched { meta, raw, outcome })
                        }
                        Err(err) => Err(err),
                    },
                    Err(_) => {
                        summary = RecvDispatch::MalformedPayload;
                        on_event(DispatchEventMeta::MalformedPayload { meta, payload })
                    }
                };
                match res {
                    Ok(control) => Ok(map_isotp_control(control)),
                    Err(err) => {
                        cb_err = Some(err);
                        Ok(can_isotp_interface::RecvControl::Stop)
                    }
                }
            })
            .map_err(map_isotp_recv_error)?;

        if let Some(err) = cb_err {
            return Err(err);
        }

        Ok(match status {
            can_isotp_interface::RecvStatus::TimedOut => RecvDispatch::TimedOut,
            can_isotp_interface::RecvStatus::DeliveredOne => summary,
        })
    }
}

#[cfg(feature = "async")]
impl<Node, Router, TxBuf> Interface<Node, Router, TxBuf>
where
    Node: can_isotp_interface::IsoTpAsyncEndpoint,
    TxBuf: AsMut<[u8]>,
{
    /// Async send helper for ISO-TP async nodes.
    pub async fn send_msg_async<M: Message>(
        &mut self,
        body: &[u8],
        timeout: Duration,
    ) -> Result<(), Error> {
        let (node, tx) = (&mut self.node, &mut self.tx);
        let payload = Self::encode_msg_into_buf::<M>(tx.as_mut(), body)?;
        node.send(payload, timeout)
            .await
            .map_err(map_isotp_send_error)
    }

    /// Async send-encoded helper for ISO-TP async nodes.
    pub async fn send_encoded_async<M: Message, V: Encode<M>>(
        &mut self,
        value: &V,
        timeout: Duration,
    ) -> Result<(), Error> {
        let (node, tx) = (&mut self.node, &mut self.tx);
        let payload = Self::encode_value_into_buf::<M, V>(tx.as_mut(), value)?;
        node.send(payload, timeout)
            .await
            .map_err(map_isotp_send_error)
    }

    /// Async receive+dispatch helper for ISO-TP async endpoints.
    pub async fn recv_one_dispatch_async<Handlers>(
        &mut self,
        handlers: &mut Handlers,
        timeout: Duration,
    ) -> Result<RecvDispatch, Error>
    where
        Router: RouterDispatch<Handlers>,
    {
        self.recv_one_dispatch_with_async(handlers, timeout, |_| Ok(RecvControl::Continue))
            .await
    }

    /// Async receive+dispatch helper with event callback for ISO-TP async endpoints.
    pub async fn recv_one_dispatch_with_async<Handlers, Ev>(
        &mut self,
        handlers: &mut Handlers,
        timeout: Duration,
        mut on_event: Ev,
    ) -> Result<RecvDispatch, Error>
    where
        Router: RouterDispatch<Handlers>,
        Ev: for<'a> FnMut(DispatchEvent<'a>) -> Result<RecvControl, Error>,
    {
        let (node, router) = (&mut self.node, &mut self.router);

        let mut summary = RecvDispatch::TimedOut;
        let mut cb_err: Option<Error> = None;

        let status = node
            .recv_one(timeout, |payload| {
                if cb_err.is_some() {
                    return Ok(can_isotp_interface::RecvControl::Stop);
                }

                let res: Result<RecvControl, Error> = match decode_wire(payload) {
                    Ok(raw) => match router.dispatch(handlers, raw) {
                        Ok(outcome) => {
                            summary = RecvDispatch::Dispatched {
                                id: raw.id,
                                kind: match outcome {
                                    DispatchOutcome::Handled => RecvDispatchKind::Handled,
                                    DispatchOutcome::Unhandled(_) => RecvDispatchKind::Unhandled,
                                    DispatchOutcome::Unknown { .. } => RecvDispatchKind::Unknown,
                                },
                            };
                            on_event(DispatchEvent::Dispatched { raw, outcome })
                        }
                        Err(err) => Err(err),
                    },
                    Err(_) => {
                        summary = RecvDispatch::MalformedPayload;
                        on_event(DispatchEvent::MalformedPayload { payload })
                    }
                };

                match res {
                    Ok(control) => Ok(map_isotp_control(control)),
                    Err(err) => {
                        cb_err = Some(err);
                        Ok(can_isotp_interface::RecvControl::Stop)
                    }
                }
            })
            .await
            .map_err(map_isotp_recv_error)?;

        if let Some(err) = cb_err {
            return Err(err);
        }

        Ok(match status {
            can_isotp_interface::RecvStatus::TimedOut => RecvDispatch::TimedOut,
            can_isotp_interface::RecvStatus::DeliveredOne => summary,
        })
    }
}

#[cfg(all(feature = "async", feature = "isotp-uds"))]
impl<Node, Router, TxBuf> Interface<Node, Router, TxBuf>
where
    Node: can_isotp_interface::IsoTpAsyncEndpointMeta,
    TxBuf: AsMut<[u8]>,
{
    /// Async send helper for ISO-TP async demux nodes (addressed send).
    pub async fn send_msg_to_async<M: Message>(
        &mut self,
        to: Node::ReplyTo,
        body: &[u8],
        timeout: Duration,
    ) -> Result<(), Error> {
        let (node, tx) = (&mut self.node, &mut self.tx);
        let payload = Self::encode_msg_into_buf::<M>(tx.as_mut(), body)?;
        node.send_to(to, payload, timeout)
            .await
            .map_err(map_isotp_send_error)
    }

    /// Async send-encoded helper for ISO-TP async demux nodes (addressed send).
    pub async fn send_encoded_to_async<M: Message, V: Encode<M>>(
        &mut self,
        to: Node::ReplyTo,
        value: &V,
        timeout: Duration,
    ) -> Result<(), Error> {
        let (node, tx) = (&mut self.node, &mut self.tx);
        let payload = Self::encode_value_into_buf::<M, V>(tx.as_mut(), value)?;
        node.send_to(to, payload, timeout)
            .await
            .map_err(map_isotp_send_error)
    }

    /// Async receive+dispatch helper with reply-to metadata for ISO-TP async demux nodes.
    pub async fn recv_one_dispatch_with_meta_async<Handlers, Ev>(
        &mut self,
        handlers: &mut Handlers,
        timeout: Duration,
        mut on_event: Ev,
    ) -> Result<RecvDispatch, Error>
    where
        Router: RouterDispatch<Handlers>,
        Ev: for<'a> FnMut(DispatchEventMeta<'a, Node::ReplyTo>) -> Result<RecvControl, Error>,
    {
        let (node, router) = (&mut self.node, &mut self.router);

        let mut summary = RecvDispatch::TimedOut;
        let mut cb_err: Option<Error> = None;

        let status = node
            .recv_one_meta(timeout, |meta, payload| {
                if cb_err.is_some() {
                    return Ok(can_isotp_interface::RecvControl::Stop);
                }
                let meta = RecvMeta {
                    reply_to: meta.reply_to,
                };

                let res: Result<RecvControl, Error> = match decode_wire(payload) {
                    Ok(raw) => match router.dispatch(handlers, raw) {
                        Ok(outcome) => {
                            summary = RecvDispatch::Dispatched {
                                id: raw.id,
                                kind: match outcome {
                                    DispatchOutcome::Handled => RecvDispatchKind::Handled,
                                    DispatchOutcome::Unhandled(_) => RecvDispatchKind::Unhandled,
                                    DispatchOutcome::Unknown { .. } => RecvDispatchKind::Unknown,
                                },
                            };
                            on_event(DispatchEventMeta::Dispatched { meta, raw, outcome })
                        }
                        Err(err) => Err(err),
                    },
                    Err(_) => {
                        summary = RecvDispatch::MalformedPayload;
                        on_event(DispatchEventMeta::MalformedPayload { meta, payload })
                    }
                };

                match res {
                    Ok(control) => Ok(map_isotp_control(control)),
                    Err(err) => {
                        cb_err = Some(err);
                        Ok(can_isotp_interface::RecvControl::Stop)
                    }
                }
            })
            .await
            .map_err(map_isotp_recv_error)?;

        if let Some(err) = cb_err {
            return Err(err);
        }

        Ok(match status {
            can_isotp_interface::RecvStatus::TimedOut => RecvDispatch::TimedOut,
            can_isotp_interface::RecvStatus::DeliveredOne => summary,
        })
    }

    /// Async receive+dispatch helper with reply-to metadata for ISO-TP async demux nodes.
    pub async fn recv_one_dispatch_meta_async<Handlers>(
        &mut self,
        handlers: &mut Handlers,
        timeout: Duration,
    ) -> Result<RecvDispatch, Error>
    where
        Router: RouterDispatch<Handlers>,
    {
        self.recv_one_dispatch_with_meta_async(handlers, timeout, |_| Ok(RecvControl::Continue))
            .await
    }
}

/// Define a bus atlas: a registry of message ids and names.
///
/// This macro generates a module containing:
/// - message marker types implementing [`Message`]
/// - a marker `Atlas` type for bundle composition
///
/// See the crate-level **Examples** section for a complete, working snippet.
#[macro_export]
macro_rules! bus_atlas {
    (
        $vis:vis mod $atlas:ident {
            $($entries:tt)*
        }
    ) => {
        $vis mod $atlas {
            #[derive(Debug, Clone, Copy, Default)]
            /// Marker type representing this atlas in bundle/maplet composition.
            pub struct Atlas;
            $crate::bus_atlas!(@entries $($entries)*);
        }
    };

    (@entries) => {};

    (@entries $(#[$meta:meta])* $id:literal => $name:ident (len = $len:literal); $($rest:tt)*) => {
        $(#[$meta])*
        #[doc = concat!(
            "Atlas message `",
            stringify!($name),
            "` (id ",
            stringify!($id),
            ", fixed body length ",
            stringify!($len),
            " bytes)."
        )]
        #[derive(Clone, Copy, Debug, PartialEq, Eq)]
        pub struct $name;

        impl $name {
            /// Fixed body length in bytes.
            pub const BODY_LEN: usize = $len;
        }

        impl $crate::Message for $name {
            const ID: u16 = $id;
            const BODY_LEN: Option<usize> = Some($len);
        }

        impl $crate::MessageParser<$name> for $crate::DefaultParser {
            type Parsed<'a> = &'a [u8; $len];
            fn parse<'a>(&self, body: &'a [u8]) -> Result<Self::Parsed<'a>, $crate::Error> {
                if body.len() != $len {
                    return Err($crate::Error {
                        kind: $crate::ErrorKind::Other,
                    });
                }
                let bytes: &[u8; $len] = body.try_into().map_err(|_| $crate::Error {
                    kind: $crate::ErrorKind::Other,
                })?;
                Ok(bytes)
            }
        }

        $crate::bus_atlas!(@entries $($rest)*);
    };

    (@entries $(#[$meta:meta])* $id:literal => $name:ident (capnp = $owned:path); $($rest:tt)*) => {
        $(#[$meta])*
        #[doc = concat!(
            "Atlas message `",
            stringify!($name),
            "` (id ",
            stringify!($id),
            ", Cap'n Proto single-segment body)."
        )]
        #[derive(Clone, Copy, Debug, PartialEq, Eq)]
        #[doc = concat!("capnp owned type: `", stringify!($owned), "`")]
        pub struct $name;

        impl $crate::Message for $name {
            const ID: u16 = $id;
        }

        impl $crate::MessageParser<$name> for $crate::DefaultParser {
            type Parsed<'a> = $crate::CapnpTyped<'a, $owned>;
            fn parse<'a>(&self, body: &'a [u8]) -> Result<Self::Parsed<'a>, $crate::Error> {
                Ok($crate::CapnpTyped::new(body))
            }
        }

        $crate::bus_atlas!(@entries $($rest)*);
    };

    (@entries $(#[$meta:meta])* $id:literal => removed; $($rest:tt)*) => {
        $(#[$meta])*
        #[doc = "Reserved/removed message id (kept to avoid accidental reuse)."]
        #[allow(dead_code)]
        const _: u16 = $id;
        $crate::bus_atlas!(@entries $($rest)*);
    };
}

/// Define a reusable handler bundle tied to an atlas.
///
/// Bundles encapsulate parsing and handler logic for a set of messages and can be composed into a
/// maplet via `bus_maplet!`.
///
/// See the crate-level **Examples** section for a complete, working snippet.
#[macro_export]
macro_rules! bundle {
    (
        $vis:vis mod $bundle:ident ($atlas:ident) {
            parser: $parser:ty;
            use msgs [ $($msg:ident),* $(,)? ];
            handles { $($handled:ident => $method:ident),* $(,)? }
            $(items { $($items:tt)* })?
        }
    ) => {
        $vis mod $bundle {
            pub use super::$atlas::{ $($msg,)* };
            /// Parser type used by this bundle.
            pub type BundleParser = $parser;

            #[derive(Default, Debug, Clone, Copy)]
            /// Marker type representing this bundle for [`bus_maplet!`] composition.
            pub struct Bundle;

            /// Handlers implemented by a concrete bundle implementation.
            ///
            /// A `bundle!` expands to a trait with one method per handled message. A maplet can then
            /// embed this bundle's handler implementation inside [`bus_maplet!`]'s `HandlersImpl`.
            pub trait Handlers<'a> {
                /// Bundle-specific error type.
                type Error;
                $(
                    fn $method(
                        &mut self,
                        msg: <$parser as $crate::MessageParser<super::$atlas::$handled>>::Parsed<'a>,
                    ) -> Result<(), Self::Error>;
                )*
            }

            #[derive(Default, Debug, Clone, Copy)]
            /// A no-op handler implementation that accepts all handled messages.
            pub struct DefaultHandlers;

            impl<'a> Handlers<'a> for DefaultHandlers {
                type Error = core::convert::Infallible;
                $(
                    fn $method(
                        &mut self,
                        _msg: <$parser as $crate::MessageParser<super::$atlas::$handled>>::Parsed<'a>,
                    ) -> Result<(), Self::Error> {
                        Ok(())
                    }
                )*
            }

            const _: () = {
                fn _assert_one<P, M>()
                where
                    P: $crate::MessageParser<M>,
                    M: $crate::Message,
                {
                }
                $(
                    let _ = _assert_one::<$parser, super::$atlas::$handled>;
                )*
            };

            #[doc(hidden)]
            pub const HANDLED_IDS: &[u16] = &[
                $(<super::$atlas::$handled as $crate::Message>::ID,)*
            ];

            #[doc(hidden)]
            pub const USED_IDS: &[u16] = &[
                $(<super::$atlas::$msg as $crate::Message>::ID,)*
            ];

            /// Returns `true` if this bundle recognizes `id` as any message it references.
            pub fn is_known(id: u16) -> bool {
                match id {
                    $(<super::$atlas::$msg as $crate::Message>::ID => true,)*
                    _ => false,
                }
            }

            /// Attempt to dispatch a decoded message to this bundle's handlers.
            ///
            /// Returns `None` when the message is not handled by this bundle.
            pub fn try_dispatch<'a, H>(
                parser: &BundleParser,
                handlers: &mut H,
                msg: $crate::RawMessage<'a>,
            ) -> Option<Result<(), $crate::Error>>
            where
                H: Handlers<'a>,
            {
                let _ = parser;
                let _ = handlers;
                match msg.id {
                    $(
                        <super::$atlas::$handled as $crate::Message>::ID => {
                            let parsed = match <$parser as $crate::MessageParser<super::$atlas::$handled>>::parse(parser, msg.body) {
                                Ok(v) => v,
                                Err(e) => return Some(Err(e)),
                            };
                            if let Err(_e) = handlers.$method(parsed) {
                                return Some(Err($crate::Error { kind: $crate::ErrorKind::Other }));
                            }
                            Some(Ok(()))
                        }
                    )*
                    _ => None,
                }
            }

            impl<Atlas> $crate::BundleMeta<Atlas> for Bundle {
                type Parser = BundleParser;
                const HANDLED_IDS: &'static [u16] = HANDLED_IDS;
                const USED_IDS: &'static [u16] = USED_IDS;
                fn is_known(id: u16) -> bool {
                    is_known(id)
                }
            }

            impl<Atlas, H> $crate::BundleDispatch<Atlas, H> for Bundle
            where
                for<'a> H: Handlers<'a>,
            {
                fn try_dispatch<'a>(
                    parser: &Self::Parser,
                    handlers: &mut H,
                    msg: $crate::RawMessage<'a>,
                ) -> Option<Result<(), $crate::Error>> {
                    try_dispatch(parser, handlers, msg)
                }
            }

            $( $($items)* )?
        }
    };
}

/// Define a per-device/app maplet (atlas subset + handlers + bundles).
///
/// Maplets compose bundles with application handlers and provide a router that dispatches raw
/// messages by id.
///
/// See the crate-level **Examples** section for a complete, working snippet.
#[macro_export]
macro_rules! bus_maplet {
    (
        $vis:vis mod $maplet:ident : $atlas:ident {
            bundles [ $($bundles:tt)* ];
            parser: $app_parser:ty;
            use msgs [ $($msg:ident),* $(,)? ];
            handles { $($handled:ident => $method:ident),* $(,)? }
            unhandled_by_default = $unhandled_by_default:literal;
            ignore [ $($ignore:ident),* $(,)? ];
        }
    ) => {
        $crate::bus_maplet!(@parse_bundles
            ($vis mod $maplet : $atlas {
                parser: $app_parser;
                use msgs [ $($msg,)* ];
                handles { $($handled => $method,)* }
                unhandled_by_default = $unhandled_by_default;
                ignore [ $($ignore,)* ];
            })
            []
            $($bundles)*
        );
    };

    (@parse_bundles
        ($vis:vis mod $maplet:ident : $atlas:ident {
            parser: $app_parser:ty;
            use msgs [ $($msg:ident,)* ];
            handles { $($handled:ident => $method:ident,)* }
            unhandled_by_default = $unhandled_by_default:literal;
            ignore [ $($ignore:ident,)* ];
        })
        [ $($out:tt)* ]
    ) => {
        $crate::bus_maplet!(@expand
            $vis mod $maplet : $atlas {
                bundles [ $($out)* ];
                parser: $app_parser;
                use msgs [ $($msg,)* ];
                handles { $($handled => $method,)* }
                unhandled_by_default = $unhandled_by_default;
                ignore [ $($ignore,)* ];
            }
        );
    };

    (@parse_bundles
        ($vis:vis mod $maplet:ident : $atlas:ident {
            parser: $app_parser:ty;
            use msgs [ $($msg:ident,)* ];
            handles { $($handled:ident => $method:ident,)* }
            unhandled_by_default = $unhandled_by_default:literal;
            ignore [ $($ignore:ident,)* ];
        })
        [ $($out:tt)* ]
        $name:ident = $spec:ty , $($rest:tt)*
    ) => {
        $crate::bus_maplet!(@parse_bundles
            ($vis mod $maplet : $atlas {
                parser: $app_parser;
                use msgs [ $($msg,)* ];
                handles { $($handled => $method,)* }
                unhandled_by_default = $unhandled_by_default;
                ignore [ $($ignore,)* ];
            })
            [ $($out)* ($name, $spec), ]
            $($rest)*
        );
    };

    (@parse_bundles
        ($vis:vis mod $maplet:ident : $atlas:ident {
            parser: $app_parser:ty;
            use msgs [ $($msg:ident,)* ];
            handles { $($handled:ident => $method:ident,)* }
            unhandled_by_default = $unhandled_by_default:literal;
            ignore [ $($ignore:ident,)* ];
        })
        [ $($out:tt)* ]
        $name:ident , $($rest:tt)*
    ) => {
        $crate::bus_maplet!(@parse_bundles
            ($vis mod $maplet : $atlas {
                parser: $app_parser;
                use msgs [ $($msg,)* ];
                handles { $($handled => $method,)* }
                unhandled_by_default = $unhandled_by_default;
                ignore [ $($ignore,)* ];
            })
            [ $($out)* ($name, super::$name::Bundle), ]
            $($rest)*
        );
    };

    (@parse_bundles
        ($vis:vis mod $maplet:ident : $atlas:ident {
            parser: $app_parser:ty;
            use msgs [ $($msg:ident,)* ];
            handles { $($handled:ident => $method:ident,)* }
            unhandled_by_default = $unhandled_by_default:literal;
            ignore [ $($ignore:ident,)* ];
        })
        [ $($out:tt)* ]
        $name:ident = $spec:ty
    ) => {
        $crate::bus_maplet!(@parse_bundles
            ($vis mod $maplet : $atlas {
                parser: $app_parser;
                use msgs [ $($msg,)* ];
                handles { $($handled => $method,)* }
                unhandled_by_default = $unhandled_by_default;
                ignore [ $($ignore,)* ];
            })
            [ $($out)* ($name, $spec), ]
        );
    };

    (@parse_bundles
        ($vis:vis mod $maplet:ident : $atlas:ident {
            parser: $app_parser:ty;
            use msgs [ $($msg:ident,)* ];
            handles { $($handled:ident => $method:ident,)* }
            unhandled_by_default = $unhandled_by_default:literal;
            ignore [ $($ignore:ident,)* ];
        })
        [ $($out:tt)* ]
        $name:ident
    ) => {
        $crate::bus_maplet!(@parse_bundles
            ($vis mod $maplet : $atlas {
                parser: $app_parser;
                use msgs [ $($msg,)* ];
                handles { $($handled => $method,)* }
                unhandled_by_default = $unhandled_by_default;
                ignore [ $($ignore,)* ];
            })
            [ $($out)* ($name, super::$name::Bundle), ]
        );
    };

    (@expand
        $vis:vis mod $maplet:ident : $atlas:ident {
            bundles [ $(($bundle:ident, $bundle_spec:ty),)* ];
            parser: $app_parser:ty;
            use msgs [ $($msg:ident,)* ];
            handles { $($handled:ident => $method:ident,)* }
            unhandled_by_default = $unhandled_by_default:literal;
            ignore [ $($ignore:ident,)* ];
        }
    ) => {
        $vis mod $maplet {
            #![allow(non_camel_case_types)]
            pub use super::$atlas;

            #[derive(Debug)]
            /// Router generated for this maplet.
            ///
            /// Implements [`RouterDispatch`] and routes [`RawMessage`] values to either:
            /// - bundle handlers (listed in `bundles [...]`), or
            /// - application handlers (listed in `handles { ... }`).
            pub struct Router {
                /// Parser used for application-handled messages.
                pub app_parser: $app_parser,
                $(
                    #[doc = concat!("Parser for bundle `", stringify!($bundle), "`.")]
                    pub $bundle: <$bundle_spec as $crate::BundleMeta<super::$atlas::Atlas>>::Parser,
                )*
            }

            impl Router {
                /// Construct a router with default parser instances.
                pub fn new() -> Self {
                    Self {
                        app_parser: Default::default(),
                        $($bundle: Default::default(),)*
                    }
                }
            }

            /// When `true`, known-but-unhandled messages return [`DispatchOutcome::Unhandled`].
            pub const UNHANDLED_BY_DEFAULT: bool = $unhandled_by_default;

            #[doc(hidden)]
            pub const MAPLET_HANDLED_IDS: &[u16] = &[
                $(<super::$atlas::$handled as $crate::Message>::ID,)*
            ];

            const _: () = {
                $crate::__assert_unique_u16_slices([
                    $(<$bundle_spec as $crate::BundleMeta<super::$atlas::Atlas>>::HANDLED_IDS,)*
                    MAPLET_HANDLED_IDS,
                ]);
            };

            #[allow(dead_code)]
            /// Message IDs that are recognized but intentionally ignored.
            pub const IGNORED_IDS: &[u16] = &[
                $(<super::$atlas::$ignore as $crate::Message>::ID,)*
            ];

            /// Handlers implemented by the application for this maplet.
            ///
            /// A `bus_maplet!` expands to a trait with one method per `handles { ... }` entry.
            pub trait Handlers<'a> {
                /// Application-specific error type.
                type Error;
                $(
                    fn $method(
                        &mut self,
                        msg: <$app_parser as $crate::MessageParser<super::$atlas::$handled>>::Parsed<'a>,
                    ) -> Result<(), Self::Error>;
                )*
            }

            #[allow(non_camel_case_types)]
            /// Concrete handler container used by [`Router::dispatch`](RouterDispatch::dispatch).
            ///
            /// `App` is the application's `Handlers` implementation, and the remaining fields are
            /// bundle handler implementations (one per bundle in `bundles [...]`).
            pub struct HandlersImpl<App, $($bundle),*> {
                /// Application handlers.
                pub app: App,
                $(
                    #[doc = concat!("Handlers for bundle `", stringify!($bundle), "`.")]
                    pub $bundle: $bundle,
                )*
            }

            impl<'a, App, $($bundle),*> Handlers<'a> for HandlersImpl<App, $($bundle),*>
            where
                App: Handlers<'a>,
            {
                type Error = <App as Handlers<'a>>::Error;
                $(
                    fn $method(
                        &mut self,
                        msg: <$app_parser as $crate::MessageParser<super::$atlas::$handled>>::Parsed<'a>,
                    ) -> Result<(), Self::Error> {
                        self.app.$method(msg)
                    }
                )*
            }

            // This stub doesn't use these, but keeping them here makes the intended API more concrete.
            #[allow(dead_code)]
            /// Marker tuple of all message types referenced by this maplet.
            pub type MessagesInMaplet = ( $(super::$atlas::$msg,)* );

            const _: () = {
                fn _assert_one<P, M>()
                where
                    P: $crate::MessageParser<M>,
                    M: $crate::Message,
                {
                }
                $(
                    let _ = _assert_one::<$app_parser, super::$atlas::$handled>;
                )*
            };

            fn is_known(id: u16) -> bool {
                match id {
                    $(<super::$atlas::$msg as $crate::Message>::ID => true,)*
                    _ => false,
                }
            }

            fn is_ignored(id: u16) -> bool {
                match id {
                    $(<super::$atlas::$ignore as $crate::Message>::ID => true,)*
                    _ => false,
                }
            }

            impl<App, $($bundle),*> $crate::RouterDispatch<HandlersImpl<App, $($bundle),*>> for Router
            where
                for<'a> App: Handlers<'a>,
                $($bundle_spec: $crate::BundleDispatch<super::$atlas::Atlas, $bundle>,)*
            {
                fn dispatch<'a>(
                    &mut self,
                    handlers: &mut HandlersImpl<App, $($bundle),*>,
                    msg: $crate::RawMessage<'a>,
                ) -> Result<$crate::DispatchOutcome<'a>, $crate::Error> {
                    $(
                        if let Some(result) = <$bundle_spec as $crate::BundleDispatch<super::$atlas::Atlas, $bundle>>::try_dispatch(&self.$bundle, &mut handlers.$bundle, msg) {
                            result?;
                            return Ok($crate::DispatchOutcome::Handled);
                        }
                    )*

                    match msg.id {
                        $(
                            <super::$atlas::$handled as $crate::Message>::ID => {
                                let parsed = <$app_parser as $crate::MessageParser<super::$atlas::$handled>>::parse(
                                    &self.app_parser,
                                    msg.body,
                                )?;
                                if let Err(_e) = Handlers::$method(&mut handlers.app, parsed) {
                                    return Err($crate::Error { kind: $crate::ErrorKind::Other });
                                }
                                return Ok($crate::DispatchOutcome::Handled);
                            }
                        )*
                        _ => {}
                    }

                    let known = is_known(msg.id) $(|| <$bundle_spec as $crate::BundleMeta<super::$atlas::Atlas>>::is_known(msg.id))*;
                    if known {
                        if is_ignored(msg.id) {
                            return Ok($crate::DispatchOutcome::Handled);
                        }
                        if UNHANDLED_BY_DEFAULT {
                            return Ok($crate::DispatchOutcome::Unhandled(msg));
                        }
                        return Ok($crate::DispatchOutcome::Handled);
                    }

                    Ok($crate::DispatchOutcome::Unknown {
                        id: msg.id,
                        body: msg.body,
                    })
                }
            }
        }
    };
}
