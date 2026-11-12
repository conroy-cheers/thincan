#![cfg(any(feature = "tokio", feature = "embassy"))]

use can_isotp_interface::{Deadline, earlier, until};
use core::cell::{Cell, RefCell};
use embassy_futures::select::{Either, select};
use embassy_sync::{blocking_mutex::Mutex as BlockingMutex, signal::Signal};

use capnp::message::ReaderOptions;
use sha2::{Digest, Sha256};

#[cfg(feature = "alloc")]
use alloc::vec::Vec;

#[cfg(feature = "heapless")]
use heapless::Vec as HeaplessVec;

use crate::{
    Atlas, Error, FileAckValue, FileTransferBundle, ReceiverConfig, file_ack_abort,
    file_ack_accept, file_ack_complete, file_ack_progress, file_ack_reject, file_chunk, file_offer,
    schema,
};

// Default to per-chunk progress ACKs to avoid timeout-paced stalls when sender
// windows are small relative to link latency/jitter.
const PROGRESS_ACK_EVERY_CHUNKS: u32 = 1;
const SHA256_HASH_LEN: usize = 32;

/// Parsed `FileAck` fields.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ack {
    pub transfer_id: u32,
    pub kind: schema::FileAckKind,
    pub next_offset: u32,
    pub chunk_size: u32,
    pub error: schema::FileAckError,
}

/// Decode a received `FileAck` body.
pub fn decode_file_ack<'a>(
    msg: thincan::CapnpTyped<'a, schema::file_ack::Owned>,
) -> Result<Ack, thincan::Error> {
    let (transfer_id, kind, next_offset, chunk_size, error) = msg
        .with_root(ReaderOptions::default(), |root| {
            (
                root.get_transfer_id(),
                root.get_kind(),
                root.get_next_offset(),
                root.get_chunk_size(),
                root.get_error(),
            )
        })
        .map_err(|_| thincan::Error {
            kind: thincan::ErrorKind::Other,
        })?;

    let kind = kind.map_err(|_| thincan::Error {
        kind: thincan::ErrorKind::Other,
    })?;
    let error = error.map_err(|_| thincan::Error {
        kind: thincan::ErrorKind::Other,
    })?;

    Ok(Ack {
        transfer_id,
        kind,
        next_offset,
        chunk_size,
        error,
    })
}

/// Sender tuning options for [`FileTransferBundleInstance::send_file`] and
/// [`FileTransferBundleInstance::send_file_with_id`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SendConfig {
    pub sender_max_chunk_size: usize,
    pub window_chunks: usize,
    /// Window retransmissions after an ACK timeout, and separately the most
    /// times an unanswered offer is repeated.
    pub max_retries: usize,
    /// Per-window ACK timer, always capped by the overall absolute deadline. On
    /// expiry the window is retransmitted, using one of `max_retries`.
    ///
    /// Retransmissions are safe with every receiver release, but a value shorter
    /// than the receiver's per-chunk work (for example a flash write) spends
    /// retries without loss. Keep it below the receiver's
    /// `ReceiverConfig::idle_timeout`, minus the time to send one window.
    pub ack_timeout: core::time::Duration,
    /// Re-send the offer when no Accept arrives within this interval, at most
    /// `max_retries` times, then wait for Accept until the overall deadline;
    /// `None` sends it once.
    ///
    /// Current receivers discard repeats of an offer they handled. A 0.1
    /// receiver that takes longer than this to prepare storage would read a
    /// repeat as a new transfer later, so keep it well above receivers' storage
    /// preparation time, or use `None` with 0.1 receivers that prepare slowly.
    pub offer_retry_interval: Option<core::time::Duration>,
}

impl Default for SendConfig {
    fn default() -> Self {
        Self {
            sender_max_chunk_size: crate::DEFAULT_CHUNK_SIZE,
            window_chunks: 4,
            max_retries: 5,
            ack_timeout: core::time::Duration::from_secs(3),
            offer_retry_interval: Some(core::time::Duration::from_secs(10)),
        }
    }
}

impl SendConfig {
    fn validate(self) -> Result<Self, thincan::Error> {
        if self.sender_max_chunk_size == 0
            || self.window_chunks == 0
            || self.ack_timeout.is_zero()
            || self
                .offer_retry_interval
                .is_some_and(|interval| interval.is_zero())
        {
            return Err(other_error());
        }
        Ok(self)
    }
}

/// Mutable sender state for transfer-id allocation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SendState {
    next_transfer_id: u32,
}

/// Stateful file-transfer bundle instance stored in `maplet::Bundles`.
pub struct FileTransferBundleInstance<
    'a,
    Maplet,
    RM,
    Node,
    TxBuf,
    const MAX_TYPES: usize,
    const DEPTH: usize,
    const MAX_BODY: usize,
    const MAX_WAITERS: usize,
    A,
> where
    A: Atlas,
    Maplet: thincan::MapletSpec<MAX_TYPES> + thincan::MapletHasBundle<FileTransferBundle<A>>,
    RM: thincan::RawMutex,
    Node: can_isotp_interface::IsoTpAsyncWrite,
{
    bus: Bus<'a, Maplet, RM, Node, TxBuf, MAX_TYPES, DEPTH, MAX_BODY, MAX_WAITERS, A>,
    send_state: BlockingMutex<RM, Cell<SendState>>,
    receiver: ReceiverState<RM, Node::Deadline>,
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
    A,
>
    FileTransferBundleInstance<
        'a,
        Maplet,
        RM,
        Node,
        TxBuf,
        MAX_TYPES,
        DEPTH,
        MAX_BODY,
        MAX_WAITERS,
        A,
    >
where
    A: Atlas,
    Maplet: thincan::MapletSpec<MAX_TYPES> + thincan::MapletHasBundle<FileTransferBundle<A>> + 'a,
    RM: thincan::RawMutex + 'a,
    Node: can_isotp_interface::IsoTpAsyncWrite + 'a,
    TxBuf: 'a,
{
    pub fn new(
        bus: Bus<'a, Maplet, RM, Node, TxBuf, MAX_TYPES, DEPTH, MAX_BODY, MAX_WAITERS, A>,
    ) -> Self {
        Self {
            bus,
            send_state: BlockingMutex::new(Cell::new(SendState::new())),
            receiver: ReceiverState::new(),
        }
    }

    pub fn send_state(&self) -> SendState {
        self.send_state.lock(Cell::get)
    }

    fn update_send_state<T>(&self, f: impl FnOnce(&mut SendState) -> T) -> T {
        self.send_state.lock(|cell| {
            let mut state = cell.get();
            let out = f(&mut state);
            cell.set(state);
            out
        })
    }
}

impl Default for SendState {
    fn default() -> Self {
        Self::new()
    }
}

impl SendState {
    pub fn new() -> Self {
        Self {
            next_transfer_id: 1,
        }
    }

    pub fn next_transfer_id(&self) -> u32 {
        self.next_transfer_id
    }

    fn alloc_transfer_id(&mut self) -> u32 {
        let out = self.next_transfer_id;
        self.next_transfer_id = self.next_transfer_id.wrapping_add(1);
        out
    }

    fn observe_transfer_id(&mut self, transfer_id: u32) {
        if transfer_id >= self.next_transfer_id {
            self.next_transfer_id = transfer_id.wrapping_add(1);
        }
    }
}

/// File-send failure, preserving protocol and transport causes separately.
#[derive(Debug)]
pub enum SendFileError<E> {
    Protocol(thincan::Error),
    Send(thincan::SendMessageError<E>),
}
impl<E> From<thincan::Error> for SendFileError<E> {
    fn from(error: thincan::Error) -> Self {
        Self::Protocol(error)
    }
}
impl<E> From<thincan::SendMessageError<E>> for SendFileError<E> {
    fn from(error: thincan::SendMessageError<E>) -> Self {
        Self::Send(error)
    }
}
impl<E> SendFileError<E> {
    /// Whether a protocol wait or transport admission exhausted its budget.
    pub fn is_timeout(&self) -> bool {
        matches!(
            self,
            Self::Protocol(thincan::Error {
                kind: thincan::ErrorKind::Timeout
            }) | Self::Send(thincan::SendMessageError::Transport(
                can_isotp_interface::TransmitError::Timeout
            ))
        )
    }
}
impl<E: core::fmt::Display> core::fmt::Display for SendFileError<E> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Protocol(error) => write!(f, "file-transfer protocol failed: {error}"),
            Self::Send(error) => write!(f, "file-transfer send failed: {error}"),
        }
    }
}
impl<E: core::error::Error + 'static> core::error::Error for SendFileError<E> {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::Protocol(error) => Some(error),
            Self::Send(error) => Some(error),
        }
    }
}

/// Result of a bundle-driven send operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SendFileResult {
    pub transfer_id: u32,
    pub total_len: usize,
    pub chunk_size: usize,
    pub chunks_sent: usize,
    pub retries: usize,
}

/// Result of a one-shot bundle-driven receive, with request metadata in `M`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecvFileResultOf<M> {
    pub transfer_id: u32,
    pub total_len: u32,
    pub received_len: u32,
    pub sender_max_chunk_size: u32,
    pub negotiated_chunk_size: u32,
    pub metadata: M,
    pub file_hash_algo: schema::FileHashAlgo,
    pub file_hash: [u8; SHA256_HASH_LEN],
}

/// Result of [`FileTransferBundleInstance::recv_file`].
#[cfg(feature = "alloc")]
pub type RecvFileResult = RecvFileResultOf<Vec<u8>>;

/// Result of [`FileTransferBundleInstance::recv_file_no_alloc`].
#[cfg(feature = "heapless")]
pub type RecvFileResultNoAlloc<const MAX_METADATA: usize> =
    RecvFileResultOf<HeaplessVec<u8, MAX_METADATA>>;

fn other_error() -> thincan::Error {
    thincan::Error {
        kind: thincan::ErrorKind::Other,
    }
}

fn validate_sha256_file_hash(hash: &[u8]) -> Result<[u8; SHA256_HASH_LEN], schema::FileAckError> {
    if hash.is_empty() {
        return Err(schema::FileAckError::HashMissing);
    }
    if hash.len() != SHA256_HASH_LEN {
        return Err(schema::FileAckError::HashInvalidLength);
    }
    let mut out = [0u8; SHA256_HASH_LEN];
    out.copy_from_slice(hash);
    Ok(out)
}

/// What identifies an offer, to recognise repeats of it.
#[derive(Clone, Copy, PartialEq, Eq)]
struct OfferIdentity {
    transfer_id: u32,
    total_len: u32,
    file_hash: Option<[u8; SHA256_HASH_LEN]>,
}

fn offer_identity(body: &[u8]) -> Option<OfferIdentity> {
    thincan::CapnpTyped::<schema::file_req::Owned>::new(body)
        .with_root(ReaderOptions::default(), |root| OfferIdentity {
            transfer_id: root.get_transfer_id(),
            total_len: root.get_total_len(),
            file_hash: root
                .get_file_hash()
                .ok()
                .and_then(|hash| hash.try_into().ok()),
        })
        .ok()
}

/// `(transfer_id, offset, data length)` of an encoded chunk.
fn chunk_position(body: &[u8]) -> Option<(u32, u32, usize)> {
    thincan::CapnpTyped::<schema::file_chunk::Owned>::new(body)
        .with_root(ReaderOptions::default(), |root| {
            let len = root.get_data().map_or(0, |data| data.len());
            (root.get_transfer_id(), root.get_offset(), len)
        })
        .ok()
}

/// What a chunk of a transfer it already finished means to a receiver.
enum FinishedChunk {
    /// The sender's confirmation of `Complete`: an empty chunk at `total_len`.
    Confirmation,
    /// The final chunk again: the sender missed `Complete`.
    FinalRetransmit,
    /// Any other chunk of the finished transfer.
    Stale,
}

fn classify_finished(offset: u32, len: usize, total_len: u32) -> FinishedChunk {
    if offset == total_len && len == 0 {
        FinishedChunk::Confirmation
    } else if u64::from(offset) + len as u64 == u64::from(total_len) {
        FinishedChunk::FinalRetransmit
    } else {
        FinishedChunk::Stale
    }
}

/// How many finished transfers are remembered at once; the oldest is evicted.
const TIME_WAIT_SLOTS: usize = 4;

/// A transfer this receiver finished and still answers for (TIME_WAIT).
struct Finished<D> {
    peer: u8,
    transfer_id: u32,
    total_len: u32,
    file_hash: [u8; SHA256_HASH_LEN],
    /// Answer its retransmitted final chunk. Cleared by the sender's
    /// confirmation; the entry stays until expiry to recognise repeated offers.
    watch: bool,
    expires: D,
}

/// Finished transfers, by peer and transfer id, until they expire.
struct TimeWait<D> {
    entries: [Option<Finished<D>>; TIME_WAIT_SLOTS],
}

impl<D: Deadline> TimeWait<D> {
    fn new() -> Self {
        Self {
            entries: core::array::from_fn(|_| None),
        }
    }

    /// Keep the entries for which `keep` holds; returns whether any were dropped.
    fn retain(&mut self, mut keep: impl FnMut(&Finished<D>) -> bool) -> bool {
        let mut dropped = false;
        for entry in &mut self.entries {
            if entry.as_ref().is_some_and(|e| !keep(e)) {
                *entry = None;
                dropped = true;
            }
        }
        dropped
    }

    fn prune(&mut self) {
        self.retain(|e| !e.expires.remaining().is_zero());
    }

    fn live(&mut self) -> impl Iterator<Item = &mut Finished<D>> {
        self.prune();
        self.entries.iter_mut().flatten()
    }

    /// Whether `offer` from `peer` repeats a finished transfer exactly.
    fn is_repeat(&mut self, peer: u8, offer: &OfferIdentity) -> bool {
        self.live().any(|e| {
            e.peer == peer
                && e.transfer_id == offer.transfer_id
                && e.total_len == offer.total_len
                && offer.file_hash == Some(e.file_hash)
        })
    }

    /// A sender offers something new only after it is done with its previous
    /// transfer, so a new offer ends TIME_WAIT for that peer. This also lets a
    /// restarted sender reuse a transfer id for a different file. Returns
    /// whether anything was forgotten.
    fn forget_peer(&mut self, peer: u8) -> bool {
        self.retain(|e| e.peer != peer)
    }

    fn remove(&mut self, peer: u8, transfer_id: u32) {
        self.retain(|e| !(e.peer == peer && e.transfer_id == transfer_id));
    }

    fn insert(&mut self, finished: Finished<D>) {
        self.remove(finished.peer, finished.transfer_id);
        self.prune();
        let slot = self
            .entries
            .iter()
            .position(Option::is_none)
            .unwrap_or_else(|| {
                // Full: evict the entry that expires first.
                let remaining =
                    |i: &usize| self.entries[*i].as_ref().map(|e| e.expires.remaining());
                (0..TIME_WAIT_SLOTS).min_by_key(remaining).unwrap_or(0)
            });
        self.entries[slot] = Some(finished);
    }

    fn stop_watching(&mut self, peer: u8, transfer_id: u32) {
        if let Some(entry) = self
            .live()
            .find(|e| e.peer == peer && e.transfer_id == transfer_id)
        {
            entry.watch = false;
        }
    }

    /// `(peer, transfer_id, total_len, expiry)` of each watched transfer.
    fn watched(&mut self) -> [Option<(u8, u32, u32, D)>; TIME_WAIT_SLOTS] {
        self.prune();
        core::array::from_fn(|i| {
            self.entries[i]
                .as_ref()
                .filter(|e| e.watch)
                .map(|e| (e.peer, e.transfer_id, e.total_len, e.expires.clone()))
        })
    }

    /// The earliest expiry among watched transfers.
    fn next_watch_expiry(&mut self) -> Option<D> {
        self.live()
            .filter(|e| e.watch)
            .map(|e| e.expires.clone())
            .reduce(earlier)
    }
}

/// Receiver state shared by `recv_file` and `serve`.
struct ReceiverState<RM: thincan::RawMutex, D> {
    table: BlockingMutex<RM, RefCell<TimeWait<D>>>,
    /// How long finished transfers stay in `table`; zero disables TIME_WAIT.
    time_wait: BlockingMutex<RM, Cell<core::time::Duration>>,
    /// Signalled when `table` changes, to wake `serve`.
    changed: Signal<RM, ()>,
    /// Set while a `serve` future runs.
    serving: BlockingMutex<RM, Cell<bool>>,
}

impl<RM: thincan::RawMutex, D: Deadline> ReceiverState<RM, D> {
    fn new() -> Self {
        Self {
            table: BlockingMutex::new(RefCell::new(TimeWait::new())),
            time_wait: BlockingMutex::new(Cell::new(crate::DEFAULT_TIME_WAIT)),
            changed: Signal::new(),
            serving: BlockingMutex::new(Cell::new(false)),
        }
    }

    fn with<T>(&self, f: impl FnOnce(&mut TimeWait<D>) -> T) -> T {
        self.table.lock(|table| f(&mut table.borrow_mut()))
    }

    fn update<T>(&self, f: impl FnOnce(&mut TimeWait<D>) -> T) -> T {
        let out = self.with(f);
        self.changed.signal(());
        out
    }

    fn is_serving(&self) -> bool {
        self.serving.lock(Cell::get)
    }

    fn claim_serving(&self) -> Option<ServingGuard<'_, RM, D>> {
        let claimed = self.serving.lock(|serving| !serving.replace(true));
        claimed.then_some(ServingGuard(self))
    }
}

/// Marks `serve` as running until dropped.
struct ServingGuard<'s, RM: thincan::RawMutex, D>(&'s ReceiverState<RM, D>);

impl<RM: thincan::RawMutex, D> Drop for ServingGuard<'_, RM, D> {
    fn drop(&mut self) {
        self.0.serving.lock(|serving| serving.set(false));
    }
}

/// Why [`FileTransferBundleInstance::serve`] stopped.
#[derive(Debug)]
pub enum ServeError {
    /// Another `serve` future is already running for this bundle.
    AlreadyServing,
    /// Waiting on the bus failed, for example because `MAX_WAITERS` is exhausted.
    Receive(thincan::Error),
}

/// `deadline`, or `relative` from now on the same clock if that expires sooner.
fn capped<D: Deadline>(deadline: D, relative: Option<core::time::Duration>) -> D {
    match relative.and_then(D::after) {
        Some(relative) => earlier(deadline, relative),
        None => deadline,
    }
}

fn inflight_chunk_count(
    last_acked: u32,
    next_to_send: u32,
    chunk_size: usize,
) -> Result<usize, thincan::Error> {
    if next_to_send <= last_acked {
        return Ok(0);
    }

    let outstanding = usize::try_from(next_to_send - last_acked).map_err(|_| other_error())?;
    Ok(outstanding.div_ceil(chunk_size))
}

/// Where a receiver keeps `FileReq` metadata.
trait Metadata: Sized {
    /// Copy metadata out of the request; `None` if it does not fit.
    fn copy_from(bytes: &[u8]) -> Option<Self>;
}

#[cfg(feature = "alloc")]
impl Metadata for Vec<u8> {
    fn copy_from(bytes: &[u8]) -> Option<Self> {
        Some(bytes.to_vec())
    }
}

#[cfg(feature = "heapless")]
impl<const N: usize> Metadata for HeaplessVec<u8, N> {
    fn copy_from(bytes: &[u8]) -> Option<Self> {
        HeaplessVec::from_slice(bytes).ok()
    }
}

/// Where `inner` lies within `outer`. Cap'n Proto reads a message in place, so
/// its non-empty `Data` fields are sub-slices of the received body.
fn subslice_range(outer: &[u8], inner: &[u8]) -> Option<core::ops::Range<usize>> {
    if inner.is_empty() {
        return Some(0..0);
    }
    let start = (inner.as_ptr() as usize).checked_sub(outer.as_ptr() as usize)?;
    let end = start.checked_add(inner.len())?;
    (end <= outer.len()).then_some(start..end)
}

/// The bus handle scoped to the file-transfer bundle.
type Bus<
    'a,
    Maplet,
    RM,
    Node,
    TxBuf,
    const MAX_TYPES: usize,
    const DEPTH: usize,
    const MAX_BODY: usize,
    const MAX_WAITERS: usize,
    A,
> = thincan::BusHandle<
    'a,
    Maplet,
    RM,
    Node,
    TxBuf,
    MAX_TYPES,
    DEPTH,
    MAX_BODY,
    MAX_WAITERS,
    FileTransferBundle<A>,
>;

/// A file-transfer bus handle paired with the peer node it is talking to.
struct Peer<'b, B, R> {
    bus: &'b B,
    id: u8,
    receiver: &'b R,
}

impl<
    'a,
    'b,
    Maplet,
    RM,
    Node,
    TxBuf,
    const MAX_TYPES: usize,
    const DEPTH: usize,
    const MAX_BODY: usize,
    const MAX_WAITERS: usize,
    A,
>
    Peer<
        'b,
        Bus<'a, Maplet, RM, Node, TxBuf, MAX_TYPES, DEPTH, MAX_BODY, MAX_WAITERS, A>,
        ReceiverState<RM, Node::Deadline>,
    >
where
    A: Atlas,
    Maplet: thincan::MapletSpec<MAX_TYPES> + thincan::MapletHasBundle<FileTransferBundle<A>>,
    RM: thincan::RawMutex,
    Node: can_isotp_interface::IsoTpAsyncWrite,
    TxBuf: AsMut<[u8]>,
{
    async fn send<M, V>(
        &self,
        value: &V,
        deadline: Node::Deadline,
    ) -> Result<(), thincan::SendMessageError<Node::Error>>
    where
        M: thincan::CapnpMessage,
        V: thincan::EncodeCapnp<M>,
    {
        self.bus
            .__send_capnp_to::<M, V>(self.id, value, deadline)
            .await
    }

    async fn send_ack(
        &self,
        ack: &FileAckValue<A>,
        deadline: Node::Deadline,
    ) -> Result<(), thincan::SendMessageError<Node::Error>> {
        self.send::<A::FileAck, _>(ack, deadline).await
    }

    /// Best-effort `Reject` ack; the transfer is already failing, so send errors are ignored.
    async fn reject(
        &self,
        transfer_id: u32,
        error: schema::FileAckError,
        deadline: Node::Deadline,
    ) {
        let _ = self
            .send_ack(&file_ack_reject::<A>(transfer_id, error), deadline)
            .await;
    }

    /// Best-effort `Abort` ack; the transfer is already failing, so send errors are ignored.
    async fn abort(
        &self,
        transfer_id: u32,
        next_offset: u32,
        error: schema::FileAckError,
        deadline: Node::Deadline,
    ) {
        let abort = file_ack_abort::<A>(transfer_id, next_offset, error);
        let _ = self.send_ack(&abort, deadline).await;
    }

    /// Next ack for `transfer_id`, discarding acks for other transfers.
    async fn next_transfer_ack(&self, transfer_id: u32) -> Result<Ack, thincan::Error> {
        loop {
            let received = self
                .bus
                .__recv_next_capnp_from::<A::FileAck>(self.id)
                .await?;
            let ack = decode_file_ack(thincan::CapnpTyped::<schema::file_ack::Owned>::new(
                received.body(),
            ))?;
            if ack.transfer_id == transfer_id {
                return Ok(ack);
            }
        }
    }

    async fn wait_for_transfer_ack(
        &self,
        transfer_id: u32,
        timeout: Node::Deadline,
        ack_timeout: core::time::Duration,
    ) -> Result<Ack, thincan::Error> {
        // The ack timer runs on the caller's clock, capped by the transfer deadline.
        match until(
            capped(timeout, Some(ack_timeout)),
            self.next_transfer_ack(transfer_id),
        )
        .await
        {
            Ok(v) => v,
            Err(_) => Err(thincan::Error::timeout()),
        }
    }

    /// Wait for the next chunk of `transfer_id`, discarding chunks of other
    /// transfers, and decode it once with `parse`.
    async fn wait_for_transfer_chunk<T>(
        &self,
        transfer_id: u32,
        timeout: Node::Deadline,
        mut parse: impl FnMut(schema::file_chunk::Reader<'_>, &[u8]) -> T,
    ) -> Result<Result<(thincan::Received<A::FileChunk, MAX_BODY>, T), capnp::Error>, thincan::Error>
    {
        let wait = async {
            loop {
                let received = self
                    .bus
                    .__recv_next_capnp_from::<A::FileChunk>(self.id)
                    .await?;
                let body = received.body();
                let parsed = thincan::CapnpTyped::<schema::file_chunk::Owned>::new(body)
                    .with_root(ReaderOptions::default(), |root| {
                        (root.get_transfer_id() == transfer_id).then(|| parse(root, body))
                    });
                match parsed {
                    Ok(None) => continue,
                    Ok(Some(value)) => return Ok(Ok((received, value))),
                    Err(error) => return Ok(Err(error)),
                }
            }
        };
        match until(timeout, wait).await {
            Ok(v) => v,
            Err(_) => Err(thincan::Error::timeout()),
        }
    }

    /// After sending `Complete`, wait for the sender's confirmation: an empty
    /// chunk at `total_len`. A repeated final chunk means the sender missed
    /// `Complete`, so send it again. Gives up after `idle` (or the default idle
    /// timeout when unset) without the confirmation; the file is already
    /// committed, so that is not an error.
    async fn await_complete_confirmation(
        &self,
        transfer_id: u32,
        total_len: u32,
        complete: &FileAckValue<A>,
        deadline: Node::Deadline,
        idle: Option<core::time::Duration>,
    ) {
        // Without an idle timeout, still bound the wait: a lost confirmation
        // must not hold a committed file until a possibly distant deadline.
        let linger = idle.unwrap_or(crate::DEFAULT_RECEIVE_IDLE_TIMEOUT);
        loop {
            let chunk = self
                .wait_for_transfer_chunk(
                    transfer_id,
                    capped(deadline.clone(), Some(linger)),
                    |root, _| {
                        let len = root.get_data().map_or(0, |data| data.len());
                        (root.get_offset(), len)
                    },
                )
                .await;
            match chunk {
                Ok(Ok((_, (offset, len)))) => match classify_finished(offset, len, total_len) {
                    FinishedChunk::Confirmation => {
                        self.receiver
                            .update(|table| table.stop_watching(self.id, transfer_id));
                        return;
                    }
                    FinishedChunk::FinalRetransmit => {
                        if self.send_ack(complete, deadline.clone()).await.is_err() {
                            return;
                        }
                    }
                    FinishedChunk::Stale => {}
                },
                // A malformed chunk.
                Ok(Err(_)) => {}
                // Idle or overall deadline, or the interface closed.
                Err(_) => return,
            }
        }
    }

    /// Record a committed transfer for TIME_WAIT; `watch` asks `serve` to
    /// answer a sender that misses `Complete`. Call before sending `Complete`,
    /// so a quick confirmation finds the entry. Returns whether it was recorded
    /// (TIME_WAIT may be disabled).
    fn record_finished(
        &self,
        transfer_id: u32,
        total_len: u32,
        file_hash: [u8; SHA256_HASH_LEN],
        watch: bool,
    ) -> bool {
        let time_wait = self.receiver.time_wait.lock(Cell::get);
        let Some(expires) = (!time_wait.is_zero())
            .then(|| Node::Deadline::after(time_wait))
            .flatten()
        else {
            return false;
        };
        self.receiver.update(|table| {
            table.insert(Finished {
                peer: self.id,
                transfer_id,
                total_len,
                file_hash,
                watch,
                expires,
            })
        });
        true
    }

    /// Drop queued offers from this peer that exactly repeat `offer`.
    async fn discard_repeats(&self, offer: OfferIdentity) {
        let repeat = |body: &[u8]| offer_identity(body) == Some(offer);
        while let Ok(Some(_)) = self
            .bus
            .__take_capnp_from_where::<A::FileReq, _>(self.id, repeat)
            .await
        {}
    }

    async fn send_file(
        &self,
        transfer_id: u32,
        bytes: &[u8],
        timeout: Node::Deadline,
        config: SendConfig,
    ) -> Result<SendFileResult, SendFileError<Node::Error>> {
        let config = config.validate()?;
        let effective_window_chunks = config.window_chunks;

        let total_len = bytes.len();
        let total_len_u32 = u32::try_from(total_len).map_err(|_| other_error())?;
        let max_chunk_u32 =
            u32::try_from(config.sender_max_chunk_size).map_err(|_| other_error())?;
        let file_hash = Sha256::digest(bytes);

        let offer = file_offer::<A>(
            transfer_id,
            total_len_u32,
            max_chunk_u32,
            &[],
            schema::FileHashAlgo::Sha256,
            file_hash.as_slice(),
        )
        .confirming_complete();
        // Re-send the offer only after `offer_retry_interval` without Accept, not
        // `ack_timeout`: the receiver may still be preparing storage, and a 0.1
        // receiver reads a repeated offer as a new transfer in its next
        // `recv_file`. Current receivers discard repeats of an offer they handled.
        // At most `max_retries` repeats, as in 0.1, then wait for the deadline:
        // each repeat holds a slot in the receiver's offer mailbox until read,
        // and a full mailbox makes the receiver's ingest fail.
        let mut repeats_left = config.max_retries;
        let accept = 'offer: loop {
            self.send::<A::FileReq, _>(&offer, timeout.clone()).await?;
            let resend = if repeats_left > 0 {
                repeats_left -= 1;
                capped(timeout.clone(), config.offer_retry_interval)
            } else {
                timeout.clone()
            };
            loop {
                let ack = match until(resend.clone(), self.next_transfer_ack(transfer_id)).await {
                    Ok(ack) => ack?,
                    Err(_) if timeout.remaining().is_zero() => {
                        return Err(thincan::Error::timeout().into());
                    }
                    Err(_) => continue 'offer,
                };
                match ack.kind {
                    schema::FileAckKind::Accept => break 'offer ack,
                    schema::FileAckKind::Reject | schema::FileAckKind::Abort => {
                        return Err(other_error().into());
                    }
                    _ => {}
                }
            }
        };

        let chunk_size = usize::try_from(accept.chunk_size).map_err(|_| other_error())?;
        if chunk_size == 0 || chunk_size > config.sender_max_chunk_size {
            return Err(other_error().into());
        }

        let mut chunks_sent = 0usize;
        let mut retries = 0usize;

        let mut last_acked = 0u32;
        let mut next_to_send = 0u32;

        while (last_acked as usize) < total_len {
            while inflight_chunk_count(last_acked, next_to_send, chunk_size)?
                < effective_window_chunks
                && (next_to_send as usize) < total_len
            {
                let offset = next_to_send as usize;
                let end = (offset + chunk_size).min(total_len);
                let data = &bytes[offset..end];

                let chunk = file_chunk::<A>(transfer_id, next_to_send, data);
                self.send::<A::FileChunk, _>(&chunk, timeout.clone())
                    .await?;

                next_to_send = next_to_send
                    .checked_add(u32::try_from(data.len()).map_err(|_| other_error())?)
                    .ok_or_else(other_error)?;
                chunks_sent += 1;
            }

            let ack = match self
                .wait_for_transfer_ack(transfer_id, timeout.clone(), config.ack_timeout)
                .await
            {
                Ok(a) => a,
                Err(e) if e.kind == thincan::ErrorKind::Timeout => {
                    retries += 1;
                    if timeout.remaining().is_zero() || retries > config.max_retries {
                        return Err(e.into());
                    }
                    next_to_send = last_acked;
                    continue;
                }
                Err(e) => return Err(e.into()),
            };

            match ack.kind {
                schema::FileAckKind::Ack | schema::FileAckKind::Complete => {}
                schema::FileAckKind::Reject | schema::FileAckKind::Abort => {
                    return Err(other_error().into());
                }
                schema::FileAckKind::Accept => continue,
            }

            let next = ack.next_offset;
            if next < last_acked || (next as usize) > total_len || next > next_to_send {
                return Err(other_error().into());
            }

            last_acked = next;

            if ack.kind == schema::FileAckKind::Complete {
                if (last_acked as usize) != total_len {
                    return Err(other_error().into());
                }
                // Confirm `Complete` so the receiver can stop listening for a
                // retransmission. Best-effort: the transfer has already succeeded,
                // and older receivers discard this chunk.
                let confirm = file_chunk::<A>(transfer_id, total_len_u32, &[]);
                let _ = self.send::<A::FileChunk, _>(&confirm, timeout).await;
                break;
            }
        }

        Ok(SendFileResult {
            transfer_id,
            total_len,
            chunk_size,
            chunks_sent,
            retries,
        })
    }

    /// Receive one file, buffering request metadata and chunk payloads per `Buf`.
    async fn recv_file<M, S>(
        &self,
        store: &mut S,
        send_timeout: Node::Deadline,
        receiver: ReceiverConfig,
    ) -> Result<RecvFileResultOf<M>, Error<S::Error, Node::Error>>
    where
        M: Metadata,
        S: crate::AsyncFileStore,
    {
        let (req, identity) = loop {
            let req = until(
                send_timeout.clone(),
                self.bus.__recv_next_capnp_from::<A::FileReq>(self.id),
            )
            .await
            .map_err(|_| Error::Receive(thincan::Error::timeout()))?
            .map_err(Error::Receive)?;
            // Drop repeats of a finished transfer, which a sender may emit while
            // waiting for Accept (0.1 senders after each timeout). Anything else
            // is a new offer and ends TIME_WAIT for this peer.
            let identity = offer_identity(req.body());
            let (repeat, forgot) = self.receiver.with(|table| match &identity {
                Some(offer) if table.is_repeat(self.id, offer) => (true, false),
                _ => (false, table.forget_peer(self.id)),
            });
            if forgot {
                self.receiver.changed.signal(());
            }
            if !repeat {
                break (req, identity);
            }
        };
        let result = self
            .receive_offer::<M, S>(&req, store, send_timeout, receiver)
            .await;
        // Drop repeats of this offer still queued, whatever the outcome. Senders
        // repeat an offer only until they hear back (Accept, Reject or Abort), so
        // on an in-order link every repeat is queued by now, and one left behind
        // would start a phantom transfer in the next `recv_file`.
        if let Some(offer) = identity {
            self.discard_repeats(offer).await;
        }
        result
    }

    /// Receive the file `req` offers.
    async fn receive_offer<M, S>(
        &self,
        req: &thincan::Received<A::FileReq, MAX_BODY>,
        store: &mut S,
        send_timeout: Node::Deadline,
        receiver: ReceiverConfig,
    ) -> Result<RecvFileResultOf<M>, Error<S::Error, Node::Error>>
    where
        M: Metadata,
        S: crate::AsyncFileStore,
    {
        let (
            transfer_id,
            total_len,
            sender_max_chunk_size,
            metadata,
            file_hash_algo,
            file_hash,
            confirms_complete,
        ) = thincan::CapnpTyped::<schema::file_req::Owned>::new(req.body())
            .with_root(ReaderOptions::default(), |root| {
                (
                    root.get_transfer_id(),
                    root.get_total_len(),
                    root.get_sender_max_chunk_size(),
                    M::copy_from(root.get_file_metadata().unwrap_or(&[])),
                    root.get_file_hash_algo(),
                    validate_sha256_file_hash(root.get_file_hash().unwrap_or(&[])),
                    root.get_confirms_complete(),
                )
            })
            .map_err(Error::Capnp)?;

        let Some(metadata) = metadata else {
            self.reject(
                transfer_id,
                schema::FileAckError::InvalidRequest,
                send_timeout,
            )
            .await;
            return Err(Error::Protocol);
        };
        let file_hash_algo = match file_hash_algo {
            Ok(v) if v == schema::FileHashAlgo::Sha256 => v,
            _ => {
                self.reject(
                    transfer_id,
                    schema::FileAckError::HashAlgoUnsupported,
                    send_timeout,
                )
                .await;
                return Err(Error::Protocol);
            }
        };
        let expected_hash = match file_hash {
            Ok(v) => v,
            Err(e) => {
                self.reject(transfer_id, e, send_timeout).await;
                return Err(Error::Protocol);
            }
        };

        let negotiated_chunk_size = match (receiver.max_chunk_size, sender_max_chunk_size) {
            (0, 0) => 0,
            (0, s) => s,
            (r, 0) => r,
            (r, s) => r.min(s),
        };

        if negotiated_chunk_size == 0 {
            self.reject(
                transfer_id,
                schema::FileAckError::ChunkSizeTooSmall,
                send_timeout,
            )
            .await;
            return Err(Error::Protocol);
        }

        let outcome = |received_len, metadata| RecvFileResultOf {
            transfer_id,
            total_len,
            received_len,
            sender_max_chunk_size,
            negotiated_chunk_size,
            metadata,
            file_hash_algo,
            file_hash: expected_hash,
        };

        let mut write_bus = store
            .begin_write(transfer_id, total_len)
            .await
            .map_err(Error::Store)?;
        let mut hasher = Sha256::new();

        let accept = file_ack_accept::<A>(transfer_id, negotiated_chunk_size);
        if let Err(error) = self.send_ack(&accept, send_timeout.clone()).await {
            store.abort(write_bus).await;
            return Err(Error::Send(error));
        }

        if total_len == 0 {
            let computed = hasher.finalize();
            if computed.as_slice() != expected_hash.as_slice() {
                store.abort(write_bus).await;
                self.abort(
                    transfer_id,
                    0,
                    schema::FileAckError::HashMismatch,
                    send_timeout,
                )
                .await;
                return Err(Error::Protocol);
            }
            store.commit(write_bus).await.map_err(Error::Store)?;
            // Accept is the last message here, so there is no `Complete` to
            // answer for, but repeats of this offer must still be recognised.
            self.record_finished(transfer_id, 0, expected_hash, false);
            return Ok(outcome(0, metadata));
        }

        let mut next_offset = 0u32;
        let ack_stride = negotiated_chunk_size.saturating_mul(PROGRESS_ACK_EVERY_CHUNKS);
        let mut last_progress_offset = 0u32;
        while next_offset < total_len {
            let chunk_deadline = capped(send_timeout.clone(), receiver.idle_timeout);
            let parsed = self
                .wait_for_transfer_chunk(transfer_id, chunk_deadline, |root, body| {
                    let data = root.get_data().unwrap_or(&[]);
                    (root.get_offset(), subslice_range(body, data))
                })
                .await;
            let (chunk, offset, data) = match parsed {
                Ok(Ok((chunk, (offset, Some(data))))) => (chunk, offset, data),
                Ok(Ok((_, (_, None)))) => {
                    // Unreachable for a body read in place; refuse rather than guess.
                    store.abort(write_bus).await;
                    self.abort(
                        transfer_id,
                        next_offset,
                        schema::FileAckError::InvalidRequest,
                        send_timeout,
                    )
                    .await;
                    return Err(Error::Protocol);
                }
                Ok(Err(e)) => {
                    store.abort(write_bus).await;
                    self.abort(
                        transfer_id,
                        next_offset,
                        schema::FileAckError::InvalidRequest,
                        send_timeout,
                    )
                    .await;
                    return Err(Error::Capnp(e));
                }
                Err(error) => {
                    store.abort(write_bus).await;
                    if error.kind == thincan::ErrorKind::Timeout {
                        // Tell a live but stalled sender to stop. Every release treats
                        // `Abort` as fatal; the schema has no timeout error code.
                        self.abort(
                            transfer_id,
                            next_offset,
                            schema::FileAckError::None,
                            send_timeout,
                        )
                        .await;
                    }
                    return Err(Error::Receive(error));
                }
            };

            let data = &chunk.body()[data];
            if data.len() > negotiated_chunk_size as usize {
                store.abort(write_bus).await;
                self.abort(
                    transfer_id,
                    next_offset,
                    schema::FileAckError::ChunkSizeTooLarge,
                    send_timeout,
                )
                .await;
                return Err(Error::Protocol);
            }

            if offset != next_offset {
                let progress = file_ack_progress::<A>(transfer_id, next_offset);
                if let Err(error) = self.send_ack(&progress, send_timeout.clone()).await {
                    store.abort(write_bus).await;
                    return Err(Error::Send(error));
                }
                last_progress_offset = next_offset;
                continue;
            }

            let remaining = total_len.saturating_sub(next_offset);
            let write_len = (data.len() as u32).min(remaining) as usize;
            if write_len == 0 {
                continue;
            }

            if let Err(e) = store
                .write_at(&mut write_bus, offset, &data[..write_len])
                .await
            {
                store.abort(write_bus).await;
                return Err(Error::Store(e));
            }
            hasher.update(&data[..write_len]);
            next_offset = next_offset.saturating_add(write_len as u32);

            if next_offset >= total_len {
                let computed = hasher.finalize();
                if computed.as_slice() != expected_hash.as_slice() {
                    store.abort(write_bus).await;
                    self.abort(
                        transfer_id,
                        total_len,
                        schema::FileAckError::HashMismatch,
                        send_timeout,
                    )
                    .await;
                    return Err(Error::Protocol);
                }
                store.commit(write_bus).await.map_err(Error::Store)?;
                let complete = file_ack_complete::<A>(transfer_id, total_len);
                let recorded = self.record_finished(transfer_id, total_len, expected_hash, true);
                // Best-effort: the file is committed, so this receive succeeded. If
                // `Complete` is lost, the sender retransmits its final chunk, which
                // `serve` or the wait below answers from the TIME_WAIT entry.
                let _ = self.send_ack(&complete, send_timeout.clone()).await;
                // `serve` answers a lost `Complete` from the TIME_WAIT entry; without
                // both, wait for the confirmation here.
                let served = recorded && self.receiver.is_serving();
                if confirms_complete && !served {
                    self.await_complete_confirmation(
                        transfer_id,
                        total_len,
                        &complete,
                        send_timeout,
                        receiver.idle_timeout,
                    )
                    .await;
                }

                return Ok(outcome(total_len, metadata));
            }

            if next_offset.saturating_sub(last_progress_offset) >= ack_stride {
                let progress = file_ack_progress::<A>(transfer_id, next_offset);
                if let Err(error) = self.send_ack(&progress, send_timeout.clone()).await {
                    store.abort(write_bus).await;
                    return Err(Error::Send(error));
                }
                last_progress_offset = next_offset;
            }
        }

        Err(Error::Protocol)
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
    A,
>
    FileTransferBundleInstance<
        'a,
        Maplet,
        RM,
        Node,
        TxBuf,
        MAX_TYPES,
        DEPTH,
        MAX_BODY,
        MAX_WAITERS,
        A,
    >
where
    A: Atlas,
    Maplet: thincan::MapletSpec<MAX_TYPES> + thincan::MapletHasBundle<FileTransferBundle<A>>,
    RM: thincan::RawMutex,
    Node: can_isotp_interface::IsoTpAsyncWrite,
    TxBuf: AsMut<[u8]>,
{
    pub async fn send_file(
        &self,
        to: u8,
        bytes: &[u8],
        timeout: Node::Deadline,
        config: SendConfig,
    ) -> Result<SendFileResult, SendFileError<Node::Error>> {
        let transfer_id = self.update_send_state(SendState::alloc_transfer_id);
        self.send_file_with_id(to, transfer_id, bytes, timeout, config)
            .await
    }

    pub async fn send_file_with_id(
        &self,
        to: u8,
        transfer_id: u32,
        bytes: &[u8],
        timeout: Node::Deadline,
        config: SendConfig,
    ) -> Result<SendFileResult, SendFileError<Node::Error>> {
        self.update_send_state(|state| state.observe_transfer_id(transfer_id));
        self.peer(to)
            .send_file(transfer_id, bytes, timeout, config)
            .await
    }

    #[allow(clippy::type_complexity)]
    fn peer(
        &self,
        id: u8,
    ) -> Peer<
        '_,
        Bus<'a, Maplet, RM, Node, TxBuf, MAX_TYPES, DEPTH, MAX_BODY, MAX_WAITERS, A>,
        ReceiverState<RM, Node::Deadline>,
    > {
        Peer {
            bus: &self.bus,
            id,
            receiver: &self.receiver,
        }
    }

    /// How long a finished transfer is remembered (TIME_WAIT); default 30 s,
    /// zero disables it. Repeated offers of a remembered transfer are dropped,
    /// and while [`Self::serve`] runs, a sender that missed `Complete` is
    /// answered. Cover the sender's whole retry window, `ack_timeout` x
    /// (`max_retries` + 1). Applies to transfers finished after the call.
    pub fn set_time_wait(&self, time_wait: core::time::Duration) {
        self.receiver.time_wait.lock(|cell| cell.set(time_wait));
    }

    /// Keep answering for finished transfers (TIME_WAIT), off the caller's path.
    ///
    /// If the sender misses `Complete`, it retransmits its final chunk; `serve`
    /// answers with `Complete` again until the sender confirms or the entry's
    /// [`Self::set_time_wait`] expires. While `serve` runs, `recv_file`
    /// returns as soon as it has sent `Complete`; otherwise it waits for the
    /// confirmation itself, delaying the caller if that is lost.
    ///
    /// Run it next to the receive pump for the life of the session. It waits on
    /// the bus only while a finished transfer is watched (normally one round
    /// trip), so idle it holds no mailbox waiter; while watching it holds one
    /// `FileChunk` waiter, plus one waiter on the interface's closure. It returns
    /// `Ok(())` when the interface closes.
    pub async fn serve(&self) -> Result<(), ServeError> {
        let _serving = self
            .receiver
            .claim_serving()
            .ok_or(ServeError::AlreadyServing)?;
        loop {
            let Some(watch_until) = self.receiver.with(TimeWait::next_watch_expiry) else {
                match select(self.receiver.changed.wait(), self.bus.__closed()).await {
                    Either::First(()) => continue,
                    Either::Second(()) => return Ok(()),
                }
            };
            // One snapshot per wait: the table changing restarts the loop, and
            // expiry ends the wait, so the filter needs no lock or clock.
            let watched = self.receiver.with(TimeWait::watched);
            let watched_entry = |peer: u8, transfer_id: u32| {
                watched
                    .iter()
                    .flatten()
                    .find(|(p, id, ..)| *p == peer && *id == transfer_id)
                    .map(|(.., total_len, expires)| (*total_len, expires.clone()))
            };
            let next = self
                .bus
                .__recv_next_capnp_where::<A::FileChunk, _>(|from, body| {
                    // Check the sender before decoding: during a transfer, its
                    // chunks come from a peer whose entries a new offer cleared.
                    watched.iter().flatten().any(|(peer, ..)| *peer == from)
                        && chunk_position(body)
                            .is_some_and(|(id, ..)| watched_entry(from, id).is_some())
                });
            let outcome = select(
                self.receiver.changed.wait(),
                until(watch_until.clone(), next),
            )
            .await;
            let received = match outcome {
                // The table changed, or the next entry expired: recompute.
                Either::First(()) | Either::Second(Err(_)) => continue,
                Either::Second(Ok(Err(error))) if error.kind == thincan::ErrorKind::Closed => {
                    return Ok(());
                }
                Either::Second(Ok(Err(error))) => return Err(ServeError::Receive(error)),
                Either::Second(Ok(Ok(received))) => received,
            };
            // The filter only takes chunks that decode and belong to a watched transfer.
            let (transfer_id, offset, len) =
                chunk_position(received.body()).expect("filtered chunk decodes");
            let (total_len, expires) =
                watched_entry(received.from, transfer_id).expect("filtered chunk is watched");
            match classify_finished(offset, len, total_len) {
                FinishedChunk::Confirmation => self
                    .receiver
                    .update(|table| table.stop_watching(received.from, transfer_id)),
                // Best-effort; the sender retransmits again if this is lost.
                FinishedChunk::FinalRetransmit => {
                    let complete = file_ack_complete::<A>(transfer_id, total_len);
                    let _ = self.peer(received.from).send_ack(&complete, expires).await;
                }
                FinishedChunk::Stale => {}
            }
        }
    }

    #[cfg(feature = "alloc")]
    pub async fn recv_file<S>(
        &self,
        from: u8,
        store: &mut S,
        send_timeout: Node::Deadline,
        receiver: ReceiverConfig,
    ) -> Result<RecvFileResult, Error<S::Error, Node::Error>>
    where
        S: crate::AsyncFileStore,
    {
        self.peer(from)
            .recv_file::<Vec<u8>, S>(store, send_timeout, receiver)
            .await
    }

    #[cfg(feature = "heapless")]
    pub async fn recv_file_no_alloc<S, const MAX_METADATA: usize>(
        &self,
        from: u8,
        store: &mut S,
        send_timeout: Node::Deadline,
        receiver: ReceiverConfig,
    ) -> Result<RecvFileResultNoAlloc<MAX_METADATA>, Error<S::Error, Node::Error>>
    where
        S: crate::AsyncFileStore,
    {
        self.peer(from)
            .recv_file::<HeaplessVec<u8, MAX_METADATA>, S>(store, send_timeout, receiver)
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
    A,
> thincan::BundleFactory<'a, Maplet, RM, Node, TxBuf, MAX_TYPES, DEPTH, MAX_BODY, MAX_WAITERS>
    for FileTransferBundle<A>
where
    A: Atlas,
    Maplet: thincan::MapletSpec<MAX_TYPES> + thincan::MapletHasBundle<FileTransferBundle<A>> + 'a,
    RM: thincan::RawMutex + 'a,
    Node: can_isotp_interface::IsoTpAsyncWrite + 'a,
    TxBuf: 'a,
{
    type Instance = FileTransferBundleInstance<
        'a,
        Maplet,
        RM,
        Node,
        TxBuf,
        MAX_TYPES,
        DEPTH,
        MAX_BODY,
        MAX_WAITERS,
        A,
    >;

    fn make(
        bus: thincan::BusHandle<
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
        FileTransferBundleInstance::new(bus)
    }
}
