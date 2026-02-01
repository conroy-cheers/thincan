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
//! - Optional send/receive helpers ([`Interface`]/[`AsyncInterface`]) that integrate with an
//!   ISO-TP node/transport.
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
//! - `std` (default): enables blocking [`Interface`] and [`Transport`].
//! - `async`: enables [`AsyncInterface`] and [`AsyncTransport`] (requires `std`).
//! - `capnp`: enables Cap'n Proto decode helpers (`Capnp` / `CapnpTyped`).
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
#[cfg(any(feature = "std", feature = "async"))]
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

#[cfg(feature = "std")]
/// Blocking transport for ISO-TP payloads.
pub trait Transport {
    /// Send a payload within a timeout.
    fn send(&mut self, payload: &[u8], timeout: Duration) -> Result<(), Error>;
    /// Receive payloads within a timeout, delivering each to `deliver`.
    fn recv(&mut self, timeout: Duration, deliver: &mut dyn FnMut(&[u8])) -> Result<(), Error>;
}

#[cfg(feature = "async")]
/// Async transport for ISO-TP payloads.
pub trait AsyncTransport {
    /// Future returned by `send`.
    type SendFuture<'a>: core::future::Future<Output = Result<(), Error>> + 'a
    where
        Self: 'a;
    /// Future returned by `recv`.
    ///
    /// The returned payloads must already be ISO-TP deframed (each `Vec<u8>` is one payload).
    type RecvFuture<'a>: core::future::Future<Output = Result<Vec<Vec<u8>>, Error>> + 'a
    where
        Self: 'a;

    /// Send a payload within a timeout.
    fn send<'a>(&'a mut self, payload: &'a [u8], timeout: Duration) -> Self::SendFuture<'a>;
    /// Receive payloads within a timeout.
    fn recv<'a>(&'a mut self, timeout: Duration) -> Self::RecvFuture<'a>;
}

#[cfg(feature = "std")]
impl<'a, Tx, Rx, F> Transport for can_iso_tp::IsoTpNode<'a, Tx, Rx, F, can_iso_tp::StdClock>
where
    Tx: embedded_can_interface::TxFrameIo<Frame = F>,
    Rx: embedded_can_interface::RxFrameIo<Frame = F, Error = Tx::Error>,
    F: embedded_can::Frame,
{
    fn send(&mut self, payload: &[u8], timeout: Duration) -> Result<(), Error> {
        can_iso_tp::IsoTpNode::send(self, payload, timeout).map_err(|e| Error {
            kind: match e {
                can_iso_tp::IsoTpError::Timeout(_) => ErrorKind::Timeout,
                _ => ErrorKind::Other,
            },
        })
    }

    fn recv(&mut self, timeout: Duration, deliver: &mut dyn FnMut(&[u8])) -> Result<(), Error> {
        can_iso_tp::IsoTpNode::recv(self, timeout, deliver).map_err(|e| Error {
            kind: match e {
                can_iso_tp::IsoTpError::Timeout(_) => ErrorKind::Timeout,
                _ => ErrorKind::Other,
            },
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

/// High-level interface that couples an ISO-TP node and a generated maplet router.
///
/// For DX tests we keep this intentionally small, but it is functional:
/// it sends/receives ISO-TP payloads and routes them by message ID.
#[derive(Debug)]
#[cfg(feature = "std")]
pub struct Interface<Node, Router> {
    node: Node,
    router: Router,
    #[cfg(feature = "std")]
    rx_queue: std::collections::VecDeque<Vec<u8>>,
    #[cfg(feature = "std")]
    rx_scratch: Vec<u8>,
    #[cfg(feature = "std")]
    tx_scratch: Vec<u8>,
}

#[cfg(feature = "std")]
impl<Node, Router> Interface<Node, Router> {
    /// Create a new interface from a transport node and a maplet router.
    pub fn new(node: Node, router: Router) -> Self {
        Self {
            node,
            router,
            #[cfg(feature = "std")]
            rx_queue: std::collections::VecDeque::new(),
            #[cfg(feature = "std")]
            rx_scratch: Vec::new(),
            #[cfg(feature = "std")]
            tx_scratch: Vec::new(),
        }
    }

    #[cfg(feature = "std")]
    /// Send a raw body for message `M` (validates `BODY_LEN` if present).
    pub fn send_msg<M: Message>(&mut self, body: &[u8], timeout: Duration) -> Result<(), Error>
    where
        Node: Transport,
    {
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
        self.tx_scratch.clear();
        self.tx_scratch.reserve(HEADER_LEN + body.len());
        self.tx_scratch.extend_from_slice(&M::ID.to_le_bytes());
        self.tx_scratch.extend_from_slice(body);
        self.node.send(&self.tx_scratch, timeout)
    }

    #[cfg(feature = "std")]
    /// Encode and send a typed value for message `M`.
    pub fn send_encoded<M: Message, V: Encode<M>>(
        &mut self,
        value: &V,
        timeout: Duration,
    ) -> Result<(), Error>
    where
        Node: Transport,
    {
        let max_len = value.max_encoded_len();

        self.tx_scratch.clear();
        self.tx_scratch.reserve(HEADER_LEN + max_len);
        self.tx_scratch.extend_from_slice(&M::ID.to_le_bytes());
        self.tx_scratch.resize(HEADER_LEN + max_len, 0u8);

        let used = value.encode(&mut self.tx_scratch[HEADER_LEN..])?;
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
        self.tx_scratch.truncate(HEADER_LEN + used);
        self.node.send(&self.tx_scratch, timeout)
    }

    #[cfg(feature = "std")]
    /// Receive a payload and dispatch it through the router.
    pub fn recv_dispatch<'a, Handlers>(
        &'a mut self,
        handlers: &mut Handlers,
        timeout: Duration,
    ) -> Result<DispatchOutcome<'a>, Error>
    where
        Node: Transport,
        Router: RouterDispatch<Handlers>,
    {
        if self.rx_queue.is_empty() {
            let queue = &mut self.rx_queue;
            self.node.recv(timeout, &mut |payload| {
                queue.push_back(payload.to_vec());
            })?;
        }

        let next = self.rx_queue.pop_front().ok_or(Error {
            kind: ErrorKind::Other,
        })?;
        self.rx_scratch = next;

        let msg = decode_wire(&self.rx_scratch)?;
        self.router.dispatch(handlers, msg)
    }
}

/// Async variant of `Interface`.
///
/// This is a small, allocation-backed implementation meant for `std` async runtimes.
#[cfg(feature = "async")]
#[derive(Debug)]
pub struct AsyncInterface<Node, Router> {
    node: Node,
    router: Router,
    rx_queue: std::collections::VecDeque<Vec<u8>>,
    rx_scratch: Vec<u8>,
    tx_scratch: Vec<u8>,
}

#[cfg(feature = "async")]
impl<Node, Router> AsyncInterface<Node, Router> {
    /// Create a new async interface from a transport node and a maplet router.
    pub fn new(node: Node, router: Router) -> Self {
        Self {
            node,
            router,
            rx_queue: std::collections::VecDeque::new(),
            rx_scratch: Vec::new(),
            tx_scratch: Vec::new(),
        }
    }

    /// Send a raw body for message `M` (validates `BODY_LEN` if present).
    pub async fn send_msg<M: Message>(
        &mut self,
        body: &[u8],
        timeout: Duration,
    ) -> Result<(), Error>
    where
        Node: AsyncTransport,
    {
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
        self.tx_scratch.clear();
        self.tx_scratch.reserve(HEADER_LEN + body.len());
        self.tx_scratch.extend_from_slice(&M::ID.to_le_bytes());
        self.tx_scratch.extend_from_slice(body);
        self.node.send(&self.tx_scratch, timeout).await
    }

    /// Encode and send a typed value for message `M`.
    pub async fn send_encoded<M: Message, V: Encode<M>>(
        &mut self,
        value: &V,
        timeout: Duration,
    ) -> Result<(), Error>
    where
        Node: AsyncTransport,
    {
        let max_len = value.max_encoded_len();

        self.tx_scratch.clear();
        self.tx_scratch.reserve(HEADER_LEN + max_len);
        self.tx_scratch.extend_from_slice(&M::ID.to_le_bytes());
        self.tx_scratch.resize(HEADER_LEN + max_len, 0u8);

        let used = value.encode(&mut self.tx_scratch[HEADER_LEN..])?;
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
        self.tx_scratch.truncate(HEADER_LEN + used);
        self.node.send(&self.tx_scratch, timeout).await
    }

    /// Receive a payload and dispatch it through the router.
    pub async fn recv_dispatch<'a, Handlers>(
        &'a mut self,
        handlers: &mut Handlers,
        timeout: Duration,
    ) -> Result<DispatchOutcome<'a>, Error>
    where
        Node: AsyncTransport,
        Router: RouterDispatch<Handlers>,
    {
        if self.rx_queue.is_empty() {
            let payloads = self.node.recv(timeout).await?;
            self.rx_queue.extend(payloads);
        }

        let next = self.rx_queue.pop_front().ok_or(Error {
            kind: ErrorKind::Other,
        })?;
        self.rx_scratch = next;
        let msg = decode_wire(&self.rx_scratch)?;
        self.router.dispatch(handlers, msg)
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
