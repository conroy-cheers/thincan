//! thincan: capnp-only message transport helpers over ISO-TP payloads.
//!
//! `thincan` sits above ISO-TP and defines a tiny wire header:
//! - `u16` message id (little-endian)
//! - message body bytes (Cap'n Proto single segment)
//!
//! The crate provides:
//! - `bus_atlas!` for message marker declarations
//! - `maplet!` for composing a compile-time message set
//! - maplet-typed `Interface` for async send, ingest, and mailboxed typed receive
#![cfg_attr(not(feature = "std"), no_std)]
#![allow(async_fn_in_trait)]

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

    /// Create a bundle instance from a scoped doodad handle.
    fn make(
        handle: DoodadHandle<
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

/// Result of ingesting a payload into a doodad mailbox.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IngestOutcome {
    pub from: u8,
    pub id: u16,
    pub len: usize,
}

/// Error returned while ingesting payloads into a doodad mailbox.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IngestError {
    MalformedPayload,
    UnknownId { id: u16 },
    BodyTooLarge { got: usize, max: usize },
    MailboxFull,
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

    fn pop_matching_where<F>(&mut self, from: u8, mut predicate: F) -> Option<MailboxSlot<MAX_BODY>>
    where
        F: FnMut(&[u8]) -> bool,
    {
        let mut idx = None;
        let mut i = 0usize;
        while i < self.used {
            if self.slots[i].from == from {
                let len = self.slots[i].len;
                if predicate(&self.slots[i].body[..len]) {
                    idx = Some(i);
                    break;
                }
            }
            i += 1;
        }

        let idx = idx?;
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

    fn pop_matching_where<F>(
        &mut self,
        slot: usize,
        from: u8,
        predicate: F,
    ) -> Option<MailboxSlot<MAX_BODY>>
    where
        F: FnMut(&[u8]) -> bool,
    {
        self.by_type[slot].pop_matching_where(from, predicate)
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
    rx_state: embassy_sync::mutex::Mutex<RM, RxState<MAX_TYPES, DEPTH, MAX_BODY>>,
    notify: [embassy_sync::watch::Watch<RM, (), MAX_WAITERS>; MAX_TYPES],
    _maplet: PhantomData<Maplet>,
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
    /// Create a new maplet-typed interface from a transport node and caller-provided TX buffer.
    pub fn new(node: Node, tx: TxBuf) -> Self {
        Self {
            tx_state: embassy_sync::mutex::Mutex::new(TxState { node, tx }),
            rx_state: embassy_sync::mutex::Mutex::new(RxState::new()),
            notify: core::array::from_fn(|_| embassy_sync::watch::Watch::new_with(())),
            _maplet: PhantomData,
        }
    }

    /// Create a cloneable handle borrowing this interface.
    pub fn handle(
        &self,
    ) -> DoodadHandle<'_, Maplet, RM, Node, TxBuf, MAX_TYPES, DEPTH, MAX_BODY, MAX_WAITERS> {
        DoodadHandle {
            iface: self,
            _bundle: PhantomData,
        }
    }

    /// Mutably borrow the underlying transport node.
    pub fn node_mut(&mut self) -> &mut Node {
        &mut self.tx_state.get_mut().node
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
        let tx_state = self.tx_state.get_mut();
        let TxState { node, tx } = tx_state;
        let payload = TxState::<Node, TxBuf>::encode_capnp_into_buf::<M, V>(tx.as_mut(), value)?;
        node.send_to(to, payload, timeout)
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
        let tx_state = self.tx_state.get_mut();
        let TxState { node, tx } = tx_state;
        let payload = TxState::<Node, TxBuf>::encode_capnp_into_buf::<M, V>(tx.as_mut(), value)?;
        node.send_functional_to(functional_to, payload, timeout)
            .map_err(map_isotp_send_error)
    }

    async fn send_capnp_to_async_shared<M: CapnpMessage, V: EncodeCapnp<M>>(
        &self,
        to: u8,
        value: &V,
        timeout: Duration,
    ) -> Result<(), Error>
    where
        Node: can_isotp_interface::IsoTpAsyncEndpoint,
    {
        let mut state = self.tx_state.lock().await;
        let tx_state = &mut *state;
        let TxState { node, tx } = tx_state;
        let payload = TxState::<Node, TxBuf>::encode_capnp_into_buf::<M, V>(tx.as_mut(), value)?;
        node.send_to(to, payload, timeout)
            .await
            .map_err(map_isotp_send_error)
    }

    async fn send_capnp_functional_to_async_shared<M: CapnpMessage, V: EncodeCapnp<M>>(
        &self,
        functional_to: u8,
        value: &V,
        timeout: Duration,
    ) -> Result<(), Error>
    where
        Node: can_isotp_interface::IsoTpAsyncEndpoint,
    {
        let mut state = self.tx_state.lock().await;
        let tx_state = &mut *state;
        let TxState { node, tx } = tx_state;
        let payload = TxState::<Node, TxBuf>::encode_capnp_into_buf::<M, V>(tx.as_mut(), value)?;
        node.send_functional_to(functional_to, payload, timeout)
            .await
            .map_err(map_isotp_send_error)
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
    Node: can_isotp_interface::IsoTpAsyncEndpoint,
    TxBuf: AsMut<[u8]>,
{
    /// Async addressed send helper.
    pub async fn send_capnp_to_async<M: CapnpMessage, V: EncodeCapnp<M>>(
        &mut self,
        to: u8,
        value: &V,
        timeout: Duration,
    ) -> Result<(), Error> {
        self.send_capnp_to_async_shared::<M, V>(to, value, timeout)
            .await
    }

    /// Async functional-address send helper.
    pub async fn send_capnp_functional_to_async<M: CapnpMessage, V: EncodeCapnp<M>>(
        &mut self,
        functional_to: u8,
        value: &V,
        timeout: Duration,
    ) -> Result<(), Error> {
        self.send_capnp_functional_to_async_shared::<M, V>(functional_to, value, timeout)
            .await
    }
}

/// Cloneable async protocol handle that borrows a shared [`Interface`].
pub struct DoodadHandle<
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
> Copy for DoodadHandle<'a, Maplet, RM, Node, TxBuf, MAX_TYPES, DEPTH, MAX_BODY, MAX_WAITERS, B>
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
> Clone for DoodadHandle<'a, Maplet, RM, Node, TxBuf, MAX_TYPES, DEPTH, MAX_BODY, MAX_WAITERS, B>
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
> DoodadHandle<'a, Maplet, RM, Node, TxBuf, MAX_TYPES, DEPTH, MAX_BODY, MAX_WAITERS, B>
where
    Maplet: MapletSpec<MAX_TYPES>,
    RM: embassy_sync::blocking_mutex::raw::RawMutex,
{
    /// Ingest one payload delivered by the external demux pump.
    pub async fn ingest(&self, from: u8, payload: &[u8]) -> Result<IngestOutcome, IngestError> {
        let raw = decode_wire(payload).map_err(|_| IngestError::MalformedPayload)?;
        let slot = Maplet::slot_for_id(raw.id).ok_or(IngestError::UnknownId { id: raw.id })?;
        let len = raw.body.len();

        {
            let mut state = self.iface.rx_state.lock().await;
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
> DoodadHandle<'a, Maplet, RM, Node, TxBuf, MAX_TYPES, DEPTH, MAX_BODY, MAX_WAITERS, Unscoped>
where
    Maplet: MapletSpec<MAX_TYPES>,
    RM: embassy_sync::blocking_mutex::raw::RawMutex,
{
    /// Narrow this handle to a specific bundle's message capabilities.
    pub fn scope<B>(
        self,
    ) -> DoodadHandle<'a, Maplet, RM, Node, TxBuf, MAX_TYPES, DEPTH, MAX_BODY, MAX_WAITERS, B>
    where
        Maplet: MapletHasBundle<B>,
    {
        DoodadHandle {
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
> DoodadHandle<'a, Maplet, RM, Node, TxBuf, MAX_TYPES, DEPTH, MAX_BODY, MAX_WAITERS, B>
where
    Maplet: MapletSpec<MAX_TYPES> + MapletHasBundle<B>,
    RM: embassy_sync::blocking_mutex::raw::RawMutex,
    Node: can_isotp_interface::IsoTpAsyncEndpoint,
    TxBuf: AsMut<[u8]>,
{
    /// Hidden protocol primitive: async addressed send.
    #[doc(hidden)]
    pub async fn __send_capnp_to<M: CapnpMessage, V: EncodeCapnp<M>>(
        &self,
        to: u8,
        value: &V,
        timeout: Duration,
    ) -> Result<(), Error> {
        self.iface
            .send_capnp_to_async_shared::<M, V>(to, value, timeout)
            .await
    }

    /// Hidden protocol primitive: async functional-address send.
    #[doc(hidden)]
    pub async fn __send_capnp_functional_to<M: CapnpMessage, V: EncodeCapnp<M>>(
        &self,
        functional_to: u8,
        value: &V,
        timeout: Duration,
    ) -> Result<(), Error> {
        self.iface
            .send_capnp_functional_to_async_shared::<M, V>(functional_to, value, timeout)
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
> DoodadHandle<'a, Maplet, RM, Node, TxBuf, MAX_TYPES, DEPTH, MAX_BODY, MAX_WAITERS, B>
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
        let slot = Maplet::slot_for_id(M::ID).ok_or(Error {
            kind: ErrorKind::Other,
        })?;

        let mut receiver = self.iface.notify[slot].receiver().ok_or(Error {
            kind: ErrorKind::Other,
        })?;

        loop {
            {
                let mut state = self.iface.rx_state.lock().await;
                if let Some(found) = state.pop_matching_where(slot, from, &mut predicate) {
                    return Ok(Received::from_slot(from, found));
                }
            }

            let _ = receiver.changed().await;
        }
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

            pub type DoodadHandle<'a, RM, Node, TxBuf, const DEPTH: usize, const MAX_BODY: usize, const MAX_WAITERS: usize, B = $crate::Unscoped> =
                $crate::DoodadHandle<'a, Maplet, RM, Node, TxBuf, { MESSAGE_COUNT }, DEPTH, MAX_BODY, MAX_WAITERS, B>;
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

            pub type DoodadHandle<'a, RM, Node, TxBuf, const DEPTH: usize, const MAX_BODY: usize, const MAX_WAITERS: usize, B = $crate::Unscoped> =
                $crate::DoodadHandle<'a, Maplet, RM, Node, TxBuf, { MESSAGE_COUNT }, DEPTH, MAX_BODY, MAX_WAITERS, B>;
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

            pub type DoodadHandle<'a, RM, Node, TxBuf, const DEPTH: usize, const MAX_BODY: usize, const MAX_WAITERS: usize, B = $crate::Unscoped> =
                $crate::DoodadHandle<'a, Maplet, RM, Node, TxBuf, { MESSAGE_COUNT }, DEPTH, MAX_BODY, MAX_WAITERS, B>;

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
                    let ingress = iface.handle();
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
