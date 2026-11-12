//! thincan: capnp-only message transport helpers over ISO-TP payloads.
//!
//! `thincan` sits above ISO-TP and defines a tiny wire header:
//! - `u16` message id (little-endian)
//! - message body bytes (Cap'n Proto single segment)
//!
//! The crate provides:
//! - `bus_atlas!` for message marker declarations
//! - `maplet!` for composing a compile-time message set
//! - `bundle_instance!` for generating bundle instance structs and `BundleFactory` impls
//! - maplet-typed `Interface` for async send, ingest, and mailboxed typed receive
#![cfg_attr(not(feature = "std"), no_std)]
#![allow(async_fn_in_trait)]

use can_isotp_interface::{Deadline, Destination, IsoTpAsyncWrite, TransmitError, until};
use core::cell::{Cell, RefCell};
use core::marker::PhantomData;
use core::time::Duration;

pub use embassy_sync::blocking_mutex::raw::{NoopRawMutex, RawMutex};

/// Simple message metadata (id + body length).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MessageSpec {
    /// 16-bit message id.
    pub id: u16,
    /// Message body size in bytes.
    pub body_size: usize,
}

/// Marker trait for message types declared in a bus atlas.
pub trait Message {
    /// 16-bit message id written to the wire (little endian).
    const ID: u16;
}

/// Marker trait for messages whose body is a Cap'n Proto single segment.
pub trait CapnpMessage: Message {
    /// The Cap'n Proto owned type used for typed decoding.
    type Owned;
}

/// Error classification returned by `thincan` operations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
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
    /// Catch-all for other errors (parsing, protocol, or transport).
    Other,
    /// The owning transport session has ended.
    Closed,
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

    /// Convenience constructor for an error on a closed interface.
    pub const fn closed() -> Self {
        Self {
            kind: ErrorKind::Closed,
        }
    }
}

impl core::error::Error for Error {}

impl core::fmt::Display for Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
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

/// A convenient wrapper for Cap'n Proto decode helpers that borrow a buffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Capnp<'a> {
    /// Raw message bytes.
    pub bytes: &'a [u8],
}

/// A schema-typed Cap'n Proto decode helper that borrows a buffer.
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

/// A trait for values that can encode a Cap'n Proto body for transmission.
pub trait EncodeCapnp<M: CapnpMessage> {
    /// Upper bound on bytes `encode()` may write.
    fn max_encoded_len(&self) -> usize;

    /// Encodes into `out` and returns written body length (excluding header).
    fn encode(&self, out: &mut [u8]) -> Result<usize, Error>;
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

/// Optional extension trait: configure ISO-TP receive-side FlowControl (BS/STmin).
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

/// Marker for a compile-time message set.
pub trait MapletSpec<const MAX_TYPES: usize> {
    /// Ordered list of message IDs in this maplet.
    const MESSAGE_IDS: [u16; MAX_TYPES];

    /// Resolve message id -> mailbox slot index.
    fn slot_for_id(id: u16) -> Option<usize> {
        let mut i = 0usize;
        while i < MAX_TYPES {
            if Self::MESSAGE_IDS[i] == id {
                return Some(i);
            }
            i += 1;
        }
        None
    }
}

/// Marker for a bundle's declared message set.
pub trait BundleSpec<const N: usize> {
    /// Ordered list of message IDs in this bundle.
    const MESSAGE_IDS: [u16; N];
}

/// Marker: maplet contains bundle `B`.
pub trait MapletHasBundle<B> {}

/// Factory hook used by `maplet!` to build singleton bundle instances.
pub trait BundleFactory<
    'a,
    Maplet,
    RM,
    Node,
    TxBuf,
    const MAX_TYPES: usize,
    const DEPTH: usize,
    const MAX_BODY: usize,
    const MAX_WAITERS: usize,
>: Sized where
    Maplet: MapletSpec<MAX_TYPES> + MapletHasBundle<Self>,
    RM: embassy_sync::blocking_mutex::raw::RawMutex,
{
    /// Concrete bundle instance type stored by the generated maplet bundle container.
    type Instance;

    /// Create a bundle instance from a scoped bus handle.
    fn make(
        bus: BusHandle<'a, Maplet, RM, Node, TxBuf, MAX_TYPES, DEPTH, MAX_BODY, MAX_WAITERS, Self>,
    ) -> Self::Instance;
}

/// Marker for an unscoped handle that is limited to ingest-only operations.
#[derive(Clone, Copy, Debug, Default)]
pub struct Unscoped;

#[derive(Debug)]
struct TxState<Node, TxBuf> {
    node: Node,
    tx: TxBuf,
}

impl<Node, TxBuf> TxState<Node, TxBuf>
where
    TxBuf: AsMut<[u8]>,
{
    fn encode_capnp_into_buf<'b, M: CapnpMessage, V: EncodeCapnp<M>>(
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
        Ok(&buf[..HEADER_LEN + used])
    }
}

/// Result of ingesting a payload into a bus mailbox.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IngestOutcome {
    pub from: u8,
    pub id: u16,
    pub len: usize,
}

/// Error returned while ingesting payloads into a bus mailbox.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum IngestError {
    MalformedPayload,
    UnknownId { id: u16 },
    BodyTooLarge { got: usize, max: usize },
    MailboxFull,
    Closed,
}

/// Encoding failure or a typed transport failure.
#[derive(Debug)]
#[non_exhaustive]
pub enum SendMessageError<E> {
    Encode(Error),
    Transport(TransmitError<E>),
}
impl<E> From<Error> for SendMessageError<E> {
    fn from(error: Error) -> Self {
        Self::Encode(error)
    }
}
impl<E> From<TransmitError<E>> for SendMessageError<E> {
    fn from(error: TransmitError<E>) -> Self {
        Self::Transport(error)
    }
}
impl<E: core::fmt::Display> core::fmt::Display for SendMessageError<E> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Encode(e) => write!(f, "encoding failed: {:?}", e.kind),
            Self::Transport(e) => e.fmt(f),
        }
    }
}
impl<E: core::error::Error + 'static> core::error::Error for SendMessageError<E> {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::Encode(e) => Some(e),
            Self::Transport(e) => Some(e),
        }
    }
}

/// Result of a receive pump operation.
#[derive(Debug)]
#[non_exhaustive]
pub enum PumpError<E> {
    Receive(can_isotp_interface::ReceiveError<E>),
    InvalidPayloadLength { reported: usize, capacity: usize },
    Ingest(IngestError),
}

impl<E> PumpError<E> {
    /// Whether the pump has stopped for good; later calls return `Closed`.
    ///
    /// Any other error drops a single payload: the transport and mailboxes stay
    /// usable, so the owner should keep pumping.
    pub fn is_terminal(&self) -> bool {
        use can_isotp_interface::ReceiveError;
        matches!(
            self,
            Self::Receive(ReceiveError::Fatal(_) | ReceiveError::Closed)
                | Self::InvalidPayloadLength { .. }
                | Self::Ingest(IngestError::Closed)
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct MailboxSlot<const MAX_BODY: usize> {
    from: u8,
    len: usize,
    body: [u8; MAX_BODY],
}

impl<const MAX_BODY: usize> MailboxSlot<MAX_BODY> {
    const fn empty() -> Self {
        Self {
            from: 0,
            len: 0,
            body: [0u8; MAX_BODY],
        }
    }
}

#[derive(Debug)]
struct TypeMailbox<const DEPTH: usize, const MAX_BODY: usize> {
    slots: [MailboxSlot<MAX_BODY>; DEPTH],
    used: usize,
}

impl<const DEPTH: usize, const MAX_BODY: usize> TypeMailbox<DEPTH, MAX_BODY> {
    fn new() -> Self {
        Self {
            slots: [MailboxSlot::empty(); DEPTH],
            used: 0,
        }
    }

    fn push(&mut self, from: u8, body: &[u8]) -> Result<(), IngestError> {
        if body.len() > MAX_BODY {
            return Err(IngestError::BodyTooLarge {
                got: body.len(),
                max: MAX_BODY,
            });
        }
        if self.used == DEPTH {
            return Err(IngestError::MailboxFull);
        }

        let idx = self.used;
        self.slots[idx].from = from;
        self.slots[idx].len = body.len();
        self.slots[idx].body[..body.len()].copy_from_slice(body);
        self.used += 1;
        Ok(())
    }

    /// Remove and return the oldest message for which `predicate(from, body)` holds.
    fn pop_where<F>(&mut self, mut predicate: F) -> Option<MailboxSlot<MAX_BODY>>
    where
        F: FnMut(u8, &[u8]) -> bool,
    {
        let idx = (0..self.used).find(|&i| {
            let slot = &self.slots[i];
            predicate(slot.from, &slot.body[..slot.len])
        })?;
        let out = self.slots[idx];
        let mut j = idx;
        while j + 1 < self.used {
            self.slots[j] = self.slots[j + 1];
            j += 1;
        }
        self.slots[self.used - 1] = MailboxSlot::empty();
        self.used -= 1;
        Some(out)
    }
}

#[derive(Debug)]
struct RxState<const MAX_TYPES: usize, const DEPTH: usize, const MAX_BODY: usize> {
    by_type: [TypeMailbox<DEPTH, MAX_BODY>; MAX_TYPES],
}

impl<const MAX_TYPES: usize, const DEPTH: usize, const MAX_BODY: usize>
    RxState<MAX_TYPES, DEPTH, MAX_BODY>
{
    fn new() -> Self {
        Self {
            by_type: core::array::from_fn(|_| TypeMailbox::new()),
        }
    }

    fn push(&mut self, slot: usize, from: u8, body: &[u8]) -> Result<(), IngestError> {
        self.by_type[slot].push(from, body)
    }

    fn pop_where<F>(&mut self, slot: usize, predicate: F) -> Option<MailboxSlot<MAX_BODY>>
    where
        F: FnMut(u8, &[u8]) -> bool,
    {
        self.by_type[slot].pop_where(predicate)
    }
}

/// Typed received message payload returned by protocol receive helpers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Received<M, const MAX_BODY: usize>
where
    M: CapnpMessage,
{
    /// Sender address.
    pub from: u8,
    /// Message id.
    pub id: u16,
    len: usize,
    body: [u8; MAX_BODY],
    _marker: PhantomData<M>,
}

impl<M, const MAX_BODY: usize> Received<M, MAX_BODY>
where
    M: CapnpMessage,
{
    fn from_slot(from: u8, slot: MailboxSlot<MAX_BODY>) -> Self {
        Self {
            from,
            id: M::ID,
            len: slot.len,
            body: slot.body,
            _marker: PhantomData,
        }
    }

    /// Borrow raw body bytes (without the 2-byte thincan header).
    pub fn body(&self) -> &[u8] {
        &self.body[..self.len]
    }

    /// Borrow this payload as a typed Cap'n Proto helper.
    pub fn as_capnp(&self) -> CapnpTyped<'_, M::Owned> {
        CapnpTyped::new(self.body())
    }

    /// Read the typed Cap'n Proto root.
    #[cfg(feature = "capnp")]
    pub fn with_root<R>(
        &self,
        options: capnp::message::ReaderOptions,
        f: impl FnOnce(<M::Owned as capnp::traits::Owned>::Reader<'_>) -> R,
    ) -> Result<R, capnp::Error>
    where
        M::Owned: capnp::traits::Owned,
    {
        self.as_capnp().with_root(options, f)
    }
}

/// Maplet-typed shared interface containing transport send path and per-message receive mailboxes.
pub struct Interface<
    Maplet,
    RM,
    Node,
    TxBuf,
    const MAX_TYPES: usize,
    const DEPTH: usize,
    const MAX_BODY: usize,
    const MAX_WAITERS: usize,
> where
    Maplet: MapletSpec<MAX_TYPES>,
    RM: embassy_sync::blocking_mutex::raw::RawMutex,
{
    tx_state: embassy_sync::mutex::Mutex<RM, TxState<Node, TxBuf>>,
    closure: embassy_sync::blocking_mutex::Mutex<RM, RefCell<Closure>>,
    pump_owned: embassy_sync::blocking_mutex::Mutex<RM, Cell<bool>>,
    rx_state: embassy_sync::mutex::Mutex<RM, RxState<MAX_TYPES, DEPTH, MAX_BODY>>,
    notify: [embassy_sync::watch::Watch<RM, (), MAX_WAITERS>; MAX_TYPES],
    _maplet: PhantomData<Maplet>,
}

/// Wakers kept for tasks waiting for closure (the pump, a send, `serve`). More
/// waiters still work: a full registration wakes everyone to re-register.
const CLOSURE_WAKERS: usize = 4;

/// Whether the session has ended, and the tasks waiting for it to.
struct Closure {
    closed: bool,
    waiters: embassy_sync::waitqueue::MultiWakerRegistration<CLOSURE_WAKERS>,
}

// Session state needs no transmit-buffer bound, so the receive side can use it too.
impl<
    Maplet,
    RM,
    Node,
    TxBuf,
    const MAX_TYPES: usize,
    const DEPTH: usize,
    const MAX_BODY: usize,
    const MAX_WAITERS: usize,
> Interface<Maplet, RM, Node, TxBuf, MAX_TYPES, DEPTH, MAX_BODY, MAX_WAITERS>
where
    Maplet: MapletSpec<MAX_TYPES>,
    RM: embassy_sync::blocking_mutex::raw::RawMutex,
{
    /// End the session and wake active sends and protocol mailbox waiters.
    pub fn close(&self) {
        self.closure.lock(|closure| {
            let mut closure = closure.borrow_mut();
            closure.closed = true;
            closure.waiters.wake();
        });
        for notify in &self.notify {
            notify.sender().send(());
        }
    }

    /// Whether the owning session has ended.
    pub fn is_closed(&self) -> bool {
        self.closure.lock(|closure| closure.borrow().closed)
    }

    /// Resolves once the session has ended.
    fn closed(&self) -> impl core::future::Future<Output = ()> + '_ {
        core::future::poll_fn(|cx| {
            self.closure.lock(|closure| {
                let mut closure = closure.borrow_mut();
                if closure.closed {
                    core::task::Poll::Ready(())
                } else {
                    closure.waiters.register(cx.waker());
                    core::task::Poll::Pending
                }
            })
        })
    }
}

impl<
    Maplet,
    RM,
    Node,
    TxBuf,
    const MAX_TYPES: usize,
    const DEPTH: usize,
    const MAX_BODY: usize,
    const MAX_WAITERS: usize,
> Interface<Maplet, RM, Node, TxBuf, MAX_TYPES, DEPTH, MAX_BODY, MAX_WAITERS>
where
    Maplet: MapletSpec<MAX_TYPES>,
    RM: embassy_sync::blocking_mutex::raw::RawMutex,
    TxBuf: AsMut<[u8]>,
{
    /// Create a new maplet-typed interface from split transport endpoints and caller-provided TX buffer.
    pub fn new(tx_node: Node, tx: TxBuf) -> Self {
        Self {
            tx_state: embassy_sync::mutex::Mutex::new(TxState { node: tx_node, tx }),
            closure: embassy_sync::blocking_mutex::Mutex::new(RefCell::new(Closure {
                closed: false,
                waiters: embassy_sync::waitqueue::MultiWakerRegistration::new(),
            })),
            pump_owned: embassy_sync::blocking_mutex::Mutex::new(Cell::new(false)),
            rx_state: embassy_sync::mutex::Mutex::new(RxState::new()),
            notify: core::array::from_fn(|_| embassy_sync::watch::Watch::new_with(())),
            _maplet: PhantomData,
        }
    }

    /// Create a cloneable bus handle borrowing this interface.
    pub fn bus(
        &self,
    ) -> BusHandle<'_, Maplet, RM, Node, TxBuf, MAX_TYPES, DEPTH, MAX_BODY, MAX_WAITERS> {
        BusHandle {
            iface: self,
            _bundle: PhantomData,
        }
    }

    /// Mutably borrow the TX transport endpoint.
    pub fn tx_node_mut(&mut self) -> &mut Node {
        &mut self.tx_state.get_mut().node
    }

    /// Recover the transmitter and encoding buffer after all handles end.
    pub fn into_tx(self) -> (Node, TxBuf) {
        let state = self.tx_state.into_inner();
        (state.node, state.tx)
    }

    /// Encode a Cap'n Proto value for message `M` into this interface's TX buffer.
    pub fn encode_capnp_into<M: CapnpMessage, V: EncodeCapnp<M>>(
        &mut self,
        value: &V,
    ) -> Result<&[u8], Error> {
        let state = self.tx_state.get_mut();
        TxState::<Node, TxBuf>::encode_capnp_into_buf::<M, V>(state.tx.as_mut(), value)
    }

    /// Encode and send a Cap'n Proto value for message `M` to a specific address.
    pub fn send_capnp_to<M: CapnpMessage, V: EncodeCapnp<M>>(
        &mut self,
        to: u8,
        value: &V,
        timeout: Duration,
    ) -> Result<(), Error>
    where
        Node: can_isotp_interface::IsoTpEndpoint,
    {
        if self.is_closed() {
            return Err(Error::closed());
        }
        let tx_state = self.tx_state.get_mut();
        let TxState { node, tx } = tx_state;
        let payload = TxState::<Node, TxBuf>::encode_capnp_into_buf::<M, V>(tx.as_mut(), value)?;
        <Node as can_isotp_interface::IsoTpEndpoint>::send_to(node, to, payload, timeout)
            .map_err(map_isotp_send_error)
    }

    /// Encode and send a Cap'n Proto value for message `M` to a functional address.
    pub fn send_capnp_functional_to<M: CapnpMessage, V: EncodeCapnp<M>>(
        &mut self,
        functional_to: u8,
        value: &V,
        timeout: Duration,
    ) -> Result<(), Error>
    where
        Node: can_isotp_interface::IsoTpEndpoint,
    {
        if self.is_closed() {
            return Err(Error::closed());
        }
        let tx_state = self.tx_state.get_mut();
        let TxState { node, tx } = tx_state;
        let payload = TxState::<Node, TxBuf>::encode_capnp_into_buf::<M, V>(tx.as_mut(), value)?;
        <Node as can_isotp_interface::IsoTpEndpoint>::send_functional_to(
            node,
            functional_to,
            payload,
            timeout,
        )
        .map_err(map_isotp_send_error)
    }

    async fn send_capnp_async_shared<M: CapnpMessage, V: EncodeCapnp<M>>(
        &self,
        destination: Destination,
        value: &V,
        deadline: Node::Deadline,
    ) -> Result<(), SendMessageError<Node::Error>>
    where
        Node: IsoTpAsyncWrite,
    {
        if self.is_closed() {
            return Err(TransmitError::Closed.into());
        }
        // Bound the wait for the encoder; the send itself is bounded by the
        // writer, which `IsoTpAsyncWrite` requires to honour `deadline`.
        let Ok(mut state) = until(deadline.clone(), self.tx_state.lock()).await else {
            return Err(TransmitError::Timeout.into());
        };
        if self.is_closed() {
            return Err(TransmitError::Closed.into());
        }
        let TxState { node, tx } = &mut *state;
        let payload = TxState::<Node, TxBuf>::encode_capnp_into_buf::<M, V>(tx.as_mut(), value)?;
        if deadline.remaining().is_zero() {
            return Err(TransmitError::Timeout.into());
        }
        let send = node.send(destination, payload, deadline);
        let result = match embassy_futures::select::select(self.closed(), send).await {
            embassy_futures::select::Either::First(()) => Err(TransmitError::Closed),
            embassy_futures::select::Either::Second(result) => result,
        };
        if matches!(result, Err(TransmitError::Fatal(_) | TransmitError::Closed)) {
            self.close();
        }
        result.map_err(SendMessageError::Transport)
    }
}

impl<
    Maplet,
    RM,
    Node,
    TxBuf,
    const MAX_TYPES: usize,
    const DEPTH: usize,
    const MAX_BODY: usize,
    const MAX_WAITERS: usize,
> Interface<Maplet, RM, Node, TxBuf, MAX_TYPES, DEPTH, MAX_BODY, MAX_WAITERS>
where
    Maplet: MapletSpec<MAX_TYPES>,
    RM: embassy_sync::blocking_mutex::raw::RawMutex,
    Node: can_isotp_interface::IsoTpAsyncWrite,
    TxBuf: AsMut<[u8]>,
{
    /// Async addressed send helper.
    pub async fn send_capnp_to_async<M: CapnpMessage, V: EncodeCapnp<M>>(
        &mut self,
        to: u8,
        value: &V,
        deadline: Node::Deadline,
    ) -> Result<(), SendMessageError<Node::Error>> {
        self.send_capnp_async_shared::<M, V>(Destination::Physical(to), value, deadline)
            .await
    }

    /// Async functional-address send helper.
    pub async fn send_capnp_functional_to_async<M: CapnpMessage, V: EncodeCapnp<M>>(
        &mut self,
        functional_to: u8,
        value: &V,
        deadline: Node::Deadline,
    ) -> Result<(), SendMessageError<Node::Error>> {
        self.send_capnp_async_shared::<M, V>(
            Destination::Functional(functional_to),
            value,
            deadline,
        )
        .await
    }
}

/// Owns receive transport state and preserves received input across ingest cancellation.
pub struct ReceivePump<
    'a,
    Rx,
    Maplet,
    RM,
    Node,
    TxBuf,
    const MAX_TYPES: usize,
    const DEPTH: usize,
    const MAX_BODY: usize,
    const MAX_WAITERS: usize,
> where
    Maplet: MapletSpec<MAX_TYPES>,
    RM: RawMutex,
{
    reader: Rx,
    iface: &'a Interface<Maplet, RM, Node, TxBuf, MAX_TYPES, DEPTH, MAX_BODY, MAX_WAITERS>,
    buffer: &'a mut [u8],
    pending: Option<can_isotp_interface::ReceivedPayload>,
}
impl<
    'a,
    Rx,
    Maplet,
    RM,
    Node,
    TxBuf,
    const MAX_TYPES: usize,
    const DEPTH: usize,
    const MAX_BODY: usize,
    const MAX_WAITERS: usize,
> ReceivePump<'a, Rx, Maplet, RM, Node, TxBuf, MAX_TYPES, DEPTH, MAX_BODY, MAX_WAITERS>
where
    Maplet: MapletSpec<MAX_TYPES>,
    RM: RawMutex,
    TxBuf: AsMut<[u8]>,
{
    pub fn new(
        reader: Rx,
        iface: &'a Interface<Maplet, RM, Node, TxBuf, MAX_TYPES, DEPTH, MAX_BODY, MAX_WAITERS>,
        buffer: &'a mut [u8],
    ) -> Self {
        assert!(
            !iface.pump_owned.lock(|owned| owned.replace(true)),
            "an interface has one receive pump per session"
        );
        Self {
            reader,
            iface,
            buffer,
            pending: None,
        }
    }
    pub async fn next(&mut self) -> Result<IngestOutcome, PumpError<Rx::Error>>
    where
        Rx: can_isotp_interface::IsoTpAsyncRead,
    {
        if self.iface.is_closed() {
            return Err(PumpError::Receive(
                can_isotp_interface::ReceiveError::Closed,
            ));
        }
        if self.pending.is_none() {
            let received = match embassy_futures::select::select(
                self.iface.closed(),
                self.reader.recv_into(self.buffer),
            )
            .await
            {
                embassy_futures::select::Either::First(()) => {
                    return Err(PumpError::Receive(
                        can_isotp_interface::ReceiveError::Closed,
                    ));
                }
                embassy_futures::select::Either::Second(result) => match result {
                    Ok(payload) => payload,
                    Err(error) => {
                        let error = PumpError::Receive(error);
                        if error.is_terminal() {
                            self.iface.close();
                        }
                        return Err(error);
                    }
                },
            };
            if received.len > self.buffer.len() {
                self.iface.close();
                return Err(PumpError::InvalidPayloadLength {
                    reported: received.len,
                    capacity: self.buffer.len(),
                });
            }
            self.pending = Some(received);
        }
        let payload = self.pending.as_ref().expect("pending receive");
        let result = self
            .iface
            .bus()
            .ingest(payload.meta.reply_to, &self.buffer[..payload.len])
            .await;
        self.pending = None;
        result.map_err(PumpError::Ingest)
    }
}
impl<
    Rx,
    Maplet,
    RM,
    Node,
    TxBuf,
    const MAX_TYPES: usize,
    const DEPTH: usize,
    const MAX_BODY: usize,
    const MAX_WAITERS: usize,
> Drop for ReceivePump<'_, Rx, Maplet, RM, Node, TxBuf, MAX_TYPES, DEPTH, MAX_BODY, MAX_WAITERS>
where
    Maplet: MapletSpec<MAX_TYPES>,
    RM: RawMutex,
{
    fn drop(&mut self) {
        self.iface.close();
    }
}

/// Cloneable async protocol handle that borrows a shared [`Interface`].
pub struct BusHandle<
    'a,
    Maplet,
    RM,
    Node,
    TxBuf,
    const MAX_TYPES: usize,
    const DEPTH: usize,
    const MAX_BODY: usize,
    const MAX_WAITERS: usize,
    B = Unscoped,
> where
    Maplet: MapletSpec<MAX_TYPES>,
    RM: embassy_sync::blocking_mutex::raw::RawMutex,
{
    iface: &'a Interface<Maplet, RM, Node, TxBuf, MAX_TYPES, DEPTH, MAX_BODY, MAX_WAITERS>,
    _bundle: PhantomData<B>,
}

impl<
    'a,
    Maplet,
    RM,
    Node,
    TxBuf,
    const MAX_TYPES: usize,
    const DEPTH: usize,
    const MAX_BODY: usize,
    const MAX_WAITERS: usize,
    B,
> Copy for BusHandle<'a, Maplet, RM, Node, TxBuf, MAX_TYPES, DEPTH, MAX_BODY, MAX_WAITERS, B>
where
    Maplet: MapletSpec<MAX_TYPES>,
    RM: embassy_sync::blocking_mutex::raw::RawMutex,
{
}

impl<
    'a,
    Maplet,
    RM,
    Node,
    TxBuf,
    const MAX_TYPES: usize,
    const DEPTH: usize,
    const MAX_BODY: usize,
    const MAX_WAITERS: usize,
    B,
> Clone for BusHandle<'a, Maplet, RM, Node, TxBuf, MAX_TYPES, DEPTH, MAX_BODY, MAX_WAITERS, B>
where
    Maplet: MapletSpec<MAX_TYPES>,
    RM: embassy_sync::blocking_mutex::raw::RawMutex,
{
    fn clone(&self) -> Self {
        *self
    }
}

impl<
    'a,
    Maplet,
    RM,
    Node,
    TxBuf,
    const MAX_TYPES: usize,
    const DEPTH: usize,
    const MAX_BODY: usize,
    const MAX_WAITERS: usize,
    B,
> BusHandle<'a, Maplet, RM, Node, TxBuf, MAX_TYPES, DEPTH, MAX_BODY, MAX_WAITERS, B>
where
    Maplet: MapletSpec<MAX_TYPES>,
    RM: embassy_sync::blocking_mutex::raw::RawMutex,
{
    /// Ingest one payload delivered by the external demux pump.
    pub async fn ingest(&self, from: u8, payload: &[u8]) -> Result<IngestOutcome, IngestError> {
        if self.iface.is_closed() {
            return Err(IngestError::Closed);
        }
        let raw = decode_wire(payload).map_err(|_| IngestError::MalformedPayload)?;
        let slot = Maplet::slot_for_id(raw.id).ok_or(IngestError::UnknownId { id: raw.id })?;
        let len = raw.body.len();

        {
            let mut state = self.iface.rx_state.lock().await;
            if self.iface.is_closed() {
                return Err(IngestError::Closed);
            }
            state.push(slot, from, raw.body)?;
        }

        self.iface.notify[slot].sender().send(());
        Ok(IngestOutcome {
            from,
            id: raw.id,
            len,
        })
    }
}

impl<
    'a,
    Maplet,
    RM,
    Node,
    TxBuf,
    const MAX_TYPES: usize,
    const DEPTH: usize,
    const MAX_BODY: usize,
    const MAX_WAITERS: usize,
> BusHandle<'a, Maplet, RM, Node, TxBuf, MAX_TYPES, DEPTH, MAX_BODY, MAX_WAITERS, Unscoped>
where
    Maplet: MapletSpec<MAX_TYPES>,
    RM: embassy_sync::blocking_mutex::raw::RawMutex,
{
    /// Narrow this handle to a specific bundle's message capabilities.
    pub fn scope<B>(
        self,
    ) -> BusHandle<'a, Maplet, RM, Node, TxBuf, MAX_TYPES, DEPTH, MAX_BODY, MAX_WAITERS, B>
    where
        Maplet: MapletHasBundle<B>,
    {
        BusHandle {
            iface: self.iface,
            _bundle: PhantomData,
        }
    }
}

impl<
    'a,
    Maplet,
    RM,
    Node,
    TxBuf,
    const MAX_TYPES: usize,
    const DEPTH: usize,
    const MAX_BODY: usize,
    const MAX_WAITERS: usize,
    B,
> BusHandle<'a, Maplet, RM, Node, TxBuf, MAX_TYPES, DEPTH, MAX_BODY, MAX_WAITERS, B>
where
    Maplet: MapletSpec<MAX_TYPES> + MapletHasBundle<B>,
    RM: embassy_sync::blocking_mutex::raw::RawMutex,
    Node: can_isotp_interface::IsoTpAsyncWrite,
    TxBuf: AsMut<[u8]>,
{
    /// Hidden protocol primitive: async addressed send.
    #[doc(hidden)]
    pub async fn __send_capnp_to<M: CapnpMessage, V: EncodeCapnp<M>>(
        &self,
        to: u8,
        value: &V,
        deadline: Node::Deadline,
    ) -> Result<(), SendMessageError<Node::Error>> {
        self.iface
            .send_capnp_async_shared::<M, V>(Destination::Physical(to), value, deadline)
            .await
    }

    /// Hidden protocol primitive: async functional-address send.
    #[doc(hidden)]
    pub async fn __send_capnp_functional_to<M: CapnpMessage, V: EncodeCapnp<M>>(
        &self,
        functional_to: u8,
        value: &V,
        deadline: Node::Deadline,
    ) -> Result<(), SendMessageError<Node::Error>> {
        self.iface
            .send_capnp_async_shared::<M, V>(
                Destination::Functional(functional_to),
                value,
                deadline,
            )
            .await
    }
}

impl<
    'a,
    Maplet,
    RM,
    Node,
    TxBuf,
    const MAX_TYPES: usize,
    const DEPTH: usize,
    const MAX_BODY: usize,
    const MAX_WAITERS: usize,
    B,
> BusHandle<'a, Maplet, RM, Node, TxBuf, MAX_TYPES, DEPTH, MAX_BODY, MAX_WAITERS, B>
where
    Maplet: MapletSpec<MAX_TYPES> + MapletHasBundle<B>,
    RM: embassy_sync::blocking_mutex::raw::RawMutex,
{
    /// Hidden protocol primitive: wait for the next message of type `M` from `from`.
    #[doc(hidden)]
    pub async fn __recv_next_capnp_from<M: CapnpMessage>(
        &self,
        from: u8,
    ) -> Result<Received<M, MAX_BODY>, Error> {
        self.__recv_next_capnp_from_where::<M, _>(from, |_| true)
            .await
    }

    /// Hidden protocol primitive: wait for the next message of type `M` from `from` that
    /// satisfies `predicate`.
    #[doc(hidden)]
    pub async fn __recv_next_capnp_from_where<M: CapnpMessage, F>(
        &self,
        from: u8,
        mut predicate: F,
    ) -> Result<Received<M, MAX_BODY>, Error>
    where
        F: FnMut(&[u8]) -> bool,
    {
        self.__recv_next_capnp_where::<M, _>(|sender, body| sender == from && predicate(body))
            .await
    }

    /// Hidden protocol primitive: take the oldest queued message of type `M`
    /// from `from` for which `predicate` holds, without waiting for arrivals.
    #[doc(hidden)]
    pub async fn __take_capnp_from_where<M: CapnpMessage, F>(
        &self,
        from: u8,
        mut predicate: F,
    ) -> Result<Option<Received<M, MAX_BODY>>, Error>
    where
        F: FnMut(&[u8]) -> bool,
    {
        let slot = Maplet::slot_for_id(M::ID).ok_or(Error {
            kind: ErrorKind::Other,
        })?;
        let mut state = self.iface.rx_state.lock().await;
        if self.iface.is_closed() {
            return Err(Error::closed());
        }
        Ok(state
            .pop_where(slot, |sender, body| sender == from && predicate(body))
            .map(|found| Received::from_slot(from, found)))
    }

    /// Hidden protocol primitive: wait for the next message of type `M` from any
    /// peer for which `predicate(from, body)` holds.
    #[doc(hidden)]
    pub async fn __recv_next_capnp_where<M: CapnpMessage, F>(
        &self,
        mut predicate: F,
    ) -> Result<Received<M, MAX_BODY>, Error>
    where
        F: FnMut(u8, &[u8]) -> bool,
    {
        let slot = Maplet::slot_for_id(M::ID).ok_or(Error {
            kind: ErrorKind::Other,
        })?;
        let mut receiver = self.iface.notify[slot].receiver().ok_or(Error {
            kind: ErrorKind::Other,
        })?;
        // Mark earlier ingests as seen: the scan below covers them, and the
        // first `changed()` would otherwise return at once and rescan.
        let _ = receiver.try_changed();
        loop {
            if self.iface.is_closed() {
                return Err(Error::closed());
            }
            {
                let mut state = self.iface.rx_state.lock().await;
                if self.iface.is_closed() {
                    return Err(Error::closed());
                }
                if let Some(found) = state.pop_where(slot, &mut predicate) {
                    return Ok(Received::from_slot(found.from, found));
                }
            }
            let _ = receiver.changed().await;
        }
    }

    /// Hidden protocol primitive: wait until the interface closes, without
    /// occupying a mailbox waiter.
    #[doc(hidden)]
    pub async fn __closed(&self) {
        self.iface.closed().await;
    }
}

/// Define a bus atlas: a registry of message ids and names.
#[macro_export]
macro_rules! bus_atlas {
    (
        $vis:vis mod $atlas:ident {
            $($entries:tt)*
        }
    ) => {
        $vis mod $atlas {
            #[derive(Debug, Clone, Copy, Default)]
            /// Marker type representing this atlas.
            pub struct Atlas;
            $crate::bus_atlas!(@entries $($entries)*);
        }
    };

    (@entries) => {};

    (@entries $(#[$meta:meta])* $id:literal => $name:ident (capnp = $owned:path); $($rest:tt)*) => {
        $(#[$meta])*
        #[derive(Clone, Copy, Debug, PartialEq, Eq)]
        pub struct $name;

        impl $crate::Message for $name {
            const ID: u16 = $id;
        }

        impl $crate::CapnpMessage for $name {
            type Owned = $owned;
        }

        $crate::bus_atlas!(@entries $($rest)*);
    };

    (@entries $(#[$meta:meta])* $id:literal => removed; $($rest:tt)*) => {
        $(#[$meta])*
        #[allow(dead_code)]
        const _: u16 = $id;
        $crate::bus_atlas!(@entries $($rest)*);
    };
}

/// Generate a bundle instance wrapper struct and its [`BundleFactory`] implementation.
///
/// # Usage
///
/// ```rust,ignore
/// thincan::bundle_instance! {
///     pub struct TelemetryBundleInstance for Bundle;
/// }
/// ```
///
/// This expands to:
/// - A struct `TelemetryBundleInstance<'a, Maplet, RM, Node, TxBuf, const MAX_TYPES, ...>`
///   holding a scoped [`BusHandle`].
/// - A `pub const fn new(handle: ...) -> Self` constructor.
/// - A [`BundleFactory`] impl for `Bundle` that delegates to `new`.
///
/// Protocol methods should be added in a separate `impl` block in the same module,
/// where they can access the `handle` field:
///
/// ```rust,ignore
/// impl<
///     'a, Maplet, RM, Node, TxBuf,
///     const MAX_TYPES: usize, const DEPTH: usize, const MAX_BODY: usize, const MAX_WAITERS: usize,
/// > TelemetryBundleInstance<'a, Maplet, RM, Node, TxBuf, MAX_TYPES, DEPTH, MAX_BODY, MAX_WAITERS>
/// where
///     Maplet: thincan::MapletSpec<MAX_TYPES> + thincan::MapletHasBundle<Bundle>,
///     RM: thincan::RawMutex,
///     Node: can_isotp_interface::IsoTpAsyncWrite,
///     TxBuf: AsMut<[u8]>,
/// {
///     pub async fn send_status_to(
///         &self,
///         to: u8,
///         ...,
///         deadline: Node::Deadline,
///     ) -> Result<(), thincan::SendMessageError<Node::Error>> {
///         self.bus.__send_capnp_to::<atlas::MyMessage, _>(to, &value, deadline).await
///     }
/// }
/// ```
#[macro_export]
macro_rules! bundle_instance {
    (
        $vis:vis struct $name:ident for $bundle:ty;
    ) => {
        $vis struct $name<
            'a,
            Maplet,
            RM,
            Node,
            TxBuf,
            const MAX_TYPES: usize,
            const DEPTH: usize,
            const MAX_BODY: usize,
            const MAX_WAITERS: usize,
        >
        where
            Maplet: $crate::MapletSpec<MAX_TYPES> + $crate::MapletHasBundle<$bundle>,
            RM: $crate::RawMutex,
        {
            pub(super) bus: $crate::BusHandle<
                'a,
                Maplet,
                RM,
                Node,
                TxBuf,
                MAX_TYPES,
                DEPTH,
                MAX_BODY,
                MAX_WAITERS,
                $bundle,
            >,
        }

        impl<
            'a,
            Maplet,
            RM,
            Node,
            TxBuf,
            const MAX_TYPES: usize,
            const DEPTH: usize,
            const MAX_BODY: usize,
            const MAX_WAITERS: usize,
        >
            $name<'a, Maplet, RM, Node, TxBuf, MAX_TYPES, DEPTH, MAX_BODY, MAX_WAITERS>
        where
            Maplet: $crate::MapletSpec<MAX_TYPES> + $crate::MapletHasBundle<$bundle>,
            RM: $crate::RawMutex,
        {
            pub const fn new(
                bus: $crate::BusHandle<
                    'a,
                    Maplet,
                    RM,
                    Node,
                    TxBuf,
                    MAX_TYPES,
                    DEPTH,
                    MAX_BODY,
                    MAX_WAITERS,
                    $bundle,
                >,
            ) -> Self {
                Self { bus }
            }
        }

        impl<
            'a,
            Maplet,
            RM,
            Node,
            TxBuf,
            const MAX_TYPES: usize,
            const DEPTH: usize,
            const MAX_BODY: usize,
            const MAX_WAITERS: usize,
        >
            $crate::BundleFactory<
                'a,
                Maplet,
                RM,
                Node,
                TxBuf,
                MAX_TYPES,
                DEPTH,
                MAX_BODY,
                MAX_WAITERS,
            > for $bundle
        where
            Maplet: $crate::MapletSpec<MAX_TYPES> + $crate::MapletHasBundle<$bundle> + 'a,
            RM: $crate::RawMutex + 'a,
            Node: 'a,
            TxBuf: 'a,
        {
            type Instance =
                $name<'a, Maplet, RM, Node, TxBuf, MAX_TYPES, DEPTH, MAX_BODY, MAX_WAITERS>;

            fn make(
                bus: $crate::BusHandle<
                    'a,
                    Maplet,
                    RM,
                    Node,
                    TxBuf,
                    MAX_TYPES,
                    DEPTH,
                    MAX_BODY,
                    MAX_WAITERS,
                    Self,
                >,
            ) -> Self::Instance {
                $name::new(bus)
            }
        }
    };
}

/// Define a maplet by composing message bundles.
#[macro_export]
macro_rules! maplet {
    (
        $vis:vis mod $maplet:ident : $atlas:ident {
            use msgs [ $($msg:ident),* $(,)? ];
        }
    ) => {
        $crate::maplet!(@define_from_msgs $vis mod $maplet : $atlas { [ $($msg),* ] });
    };

    (
        $vis:vis mod $maplet:ident : $atlas:ident {
            bundles [ $( $bundle:ident ),* $(,)? ];
        }
    ) => {
        $crate::maplet!(@define_from_bundles $vis mod $maplet : $atlas { [ $($bundle),* ] });
    };

    (
        $vis:vis mod $maplet:ident : $atlas:ident {
            bundles [ $( $alias:ident = $bundle:ident ),* $(,)? ];
        }
    ) => {
        $crate::maplet!(@define_from_bundles_aliases $vis mod $maplet : $atlas { [ $( $alias = $bundle ),* ] });
    };

    (
        $vis:vis mod $maplet:ident : $atlas:ident {
            bundles [ $( $bundle:ident => [ $($msg:ident),* $(,)? ] ),* $(,)? ];
        }
    ) => {
        $crate::maplet!(@define_from_msgs $vis mod $maplet : $atlas { [ $($($msg),*),* ] });
    };

    (@define_from_msgs $vis:vis mod $maplet:ident : $atlas:ident { [ $($msg:ident),* ] }) => {
        $vis mod $maplet {
            #[derive(Clone, Copy, Debug, Default)]
            pub struct Maplet;

            pub const MESSAGE_COUNT: usize = [$(<super::$atlas::$msg as $crate::Message>::ID),*].len();

            impl $crate::MapletSpec<{ MESSAGE_COUNT }> for Maplet {
                const MESSAGE_IDS: [u16; MESSAGE_COUNT] = [
                    $(<super::$atlas::$msg as $crate::Message>::ID),*
                ];
            }

            pub type Interface<
                RM,
                Node,
                TxBuf,
                const DEPTH: usize,
                const MAX_BODY: usize,
                const MAX_WAITERS: usize,
            > = $crate::Interface<Maplet, RM, Node, TxBuf, { MESSAGE_COUNT }, DEPTH, MAX_BODY, MAX_WAITERS>;

            pub type BusHandle<'a, RM, Node, TxBuf, const DEPTH: usize, const MAX_BODY: usize, const MAX_WAITERS: usize, B = $crate::Unscoped> =
                $crate::BusHandle<'a, Maplet, RM, Node, TxBuf, { MESSAGE_COUNT }, DEPTH, MAX_BODY, MAX_WAITERS, B>;
        }
    };

    (@define_from_bundles $vis:vis mod $maplet:ident : $atlas:ident { [ $($bundle:ident),* ] }) => {
        $vis mod $maplet {
            #[derive(Clone, Copy, Debug, Default)]
            pub struct Maplet;

            pub const MESSAGE_COUNT: usize = 0 $(+ super::$bundle::MESSAGE_COUNT)*;

            impl $crate::MapletSpec<{ MESSAGE_COUNT }> for Maplet {
                const MESSAGE_IDS: [u16; MESSAGE_COUNT] = {
                    let mut out = [0u16; MESSAGE_COUNT];
                    let mut at = 0usize;
                    $(
                        let ids = <super::$bundle::Bundle as $crate::BundleSpec<{ super::$bundle::MESSAGE_COUNT }>>::MESSAGE_IDS;
                        let mut i = 0usize;
                        while i < super::$bundle::MESSAGE_COUNT {
                            out[at + i] = ids[i];
                            i += 1;
                        }
                        at += super::$bundle::MESSAGE_COUNT;
                    )*
                    out
                };
            }

            $(
                impl $crate::MapletHasBundle<super::$bundle::Bundle> for Maplet {}
            )*

            const _: () = {
                let ids = <Maplet as $crate::MapletSpec<{ MESSAGE_COUNT }>>::MESSAGE_IDS;
                let mut i = 0usize;
                while i < MESSAGE_COUNT {
                    let mut j = i + 1usize;
                    while j < MESSAGE_COUNT {
                        if ids[i] == ids[j] {
                            panic!("duplicate message id in maplet bundles");
                        }
                        j += 1;
                    }
                    i += 1;
                }
            };

            pub type Interface<
                RM,
                Node,
                TxBuf,
                const DEPTH: usize,
                const MAX_BODY: usize,
                const MAX_WAITERS: usize,
            > = $crate::Interface<Maplet, RM, Node, TxBuf, { MESSAGE_COUNT }, DEPTH, MAX_BODY, MAX_WAITERS>;

            pub type BusHandle<'a, RM, Node, TxBuf, const DEPTH: usize, const MAX_BODY: usize, const MAX_WAITERS: usize, B = $crate::Unscoped> =
                $crate::BusHandle<'a, Maplet, RM, Node, TxBuf, { MESSAGE_COUNT }, DEPTH, MAX_BODY, MAX_WAITERS, B>;
        }
    };

    (@define_from_bundles_aliases $vis:vis mod $maplet:ident : $atlas:ident { [ $( $alias:ident = $bundle:ident ),* ] }) => {
        $vis mod $maplet {
            #[derive(Clone, Copy, Debug, Default)]
            pub struct Maplet;

            pub const MESSAGE_COUNT: usize = 0 $(+ super::$bundle::MESSAGE_COUNT)*;

            impl $crate::MapletSpec<{ MESSAGE_COUNT }> for Maplet {
                const MESSAGE_IDS: [u16; MESSAGE_COUNT] = {
                    let mut out = [0u16; MESSAGE_COUNT];
                    let mut at = 0usize;
                    $(
                        let ids = <super::$bundle::Bundle as $crate::BundleSpec<{ super::$bundle::MESSAGE_COUNT }>>::MESSAGE_IDS;
                        let mut i = 0usize;
                        while i < super::$bundle::MESSAGE_COUNT {
                            out[at + i] = ids[i];
                            i += 1;
                        }
                        at += super::$bundle::MESSAGE_COUNT;
                    )*
                    out
                };
            }

            $(
                impl $crate::MapletHasBundle<super::$bundle::Bundle> for Maplet {}
            )*

            const _: () = {
                let ids = <Maplet as $crate::MapletSpec<{ MESSAGE_COUNT }>>::MESSAGE_IDS;
                let mut i = 0usize;
                while i < MESSAGE_COUNT {
                    let mut j = i + 1usize;
                    while j < MESSAGE_COUNT {
                        if ids[i] == ids[j] {
                            panic!("duplicate message id in maplet bundles");
                        }
                        j += 1;
                    }
                    i += 1;
                }
            };

            pub type Interface<
                RM,
                Node,
                TxBuf,
                const DEPTH: usize,
                const MAX_BODY: usize,
                const MAX_WAITERS: usize,
            > = $crate::Interface<Maplet, RM, Node, TxBuf, { MESSAGE_COUNT }, DEPTH, MAX_BODY, MAX_WAITERS>;

            pub type BusHandle<'a, RM, Node, TxBuf, const DEPTH: usize, const MAX_BODY: usize, const MAX_WAITERS: usize, B = $crate::Unscoped> =
                $crate::BusHandle<'a, Maplet, RM, Node, TxBuf, { MESSAGE_COUNT }, DEPTH, MAX_BODY, MAX_WAITERS, B>;

            pub struct Bundles<'a, RM, Node, TxBuf, const DEPTH: usize, const MAX_BODY: usize, const MAX_WAITERS: usize>
            where
                RM: $crate::RawMutex,
                TxBuf: AsMut<[u8]>,
                $(
                    super::$bundle::Bundle: $crate::BundleFactory<'a, Maplet, RM, Node, TxBuf, { MESSAGE_COUNT }, DEPTH, MAX_BODY, MAX_WAITERS>,
                )*
            {
                $(
                    pub $alias: <super::$bundle::Bundle as $crate::BundleFactory<'a, Maplet, RM, Node, TxBuf, { MESSAGE_COUNT }, DEPTH, MAX_BODY, MAX_WAITERS>>::Instance,
                )*
            }

            impl<'a, RM, Node, TxBuf, const DEPTH: usize, const MAX_BODY: usize, const MAX_WAITERS: usize>
                Bundles<'a, RM, Node, TxBuf, DEPTH, MAX_BODY, MAX_WAITERS>
            where
                RM: $crate::RawMutex,
                TxBuf: AsMut<[u8]>,
                $(
                    super::$bundle::Bundle: $crate::BundleFactory<'a, Maplet, RM, Node, TxBuf, { MESSAGE_COUNT }, DEPTH, MAX_BODY, MAX_WAITERS>,
                )*
            {
                pub fn new(iface: &'a Interface<RM, Node, TxBuf, DEPTH, MAX_BODY, MAX_WAITERS>) -> Self {
                    let ingress = iface.bus();
                    Self {
                        $(
                            $alias: <super::$bundle::Bundle as $crate::BundleFactory<'a, Maplet, RM, Node, TxBuf, { MESSAGE_COUNT }, DEPTH, MAX_BODY, MAX_WAITERS>>::make(
                                ingress.scope::<super::$bundle::Bundle>(),
                            ),
                        )*
                    }
                }
            }
        }
    };
}

#[cfg(all(test, feature = "std", feature = "capnp"))]
mod transport_tests;
