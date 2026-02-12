use alloc::vec::Vec;
use capnp::message::ReaderOptions;
use core::marker::PhantomData;
use core::time::Duration;

use crate::{
    Atlas, Bundle, DEFAULT_CHUNK_SIZE, Error, FileAckValue, FileChunkValue, FileOfferValue,
    FileReqValue, FileStore, PendingAck, ReceiverConfig, SendResult, file_chunk, file_offer,
    schema,
};

#[cfg(feature = "std")]
use core::cell::RefCell;

#[cfg(feature = "std")]
thread_local! {
    static CAPNP_SCRATCH: RefCell<Vec<capnp::Word>> = const { RefCell::new(Vec::new()) };
}

fn with_capnp_scratch_bytes<R>(min_len: usize, f: impl FnOnce(&mut [u8]) -> R) -> R {
    let min_words = (min_len + 7) / 8;

    #[cfg(feature = "std")]
    {
        return CAPNP_SCRATCH.with(|cell| {
            let mut words = cell.borrow_mut();
            if words.len() < min_words {
                words.resize(min_words, capnp::word(0, 0, 0, 0, 0, 0, 0, 0));
            }
            let bytes = unsafe {
                core::slice::from_raw_parts_mut(words.as_mut_ptr() as *mut u8, words.len() * 8)
            };
            f(&mut bytes[..min_len])
        });
    }

    #[cfg(not(feature = "std"))]
    {
        let mut words: Vec<capnp::Word> = Vec::new();
        words.resize(min_words, capnp::word(0, 0, 0, 0, 0, 0, 0, 0));
        let bytes = unsafe {
            core::slice::from_raw_parts_mut(words.as_mut_ptr() as *mut u8, words.len() * 8)
        };
        f(&mut bytes[..min_len])
    }
}

/// Dependency "umbrella trait" for bundle handlers.
///
/// Most applications will simply use [`State`], which already implements this trait.
pub trait Deps {
    type Store: FileStore;
    fn state(&mut self) -> &mut State<Self::Store>;
}

struct InProgress<S: FileStore> {
    transfer_id: u32,
    total_len: u32,
    max_chunk_size: u32,
    file_metadata: Vec<u8>,
    handle: S::WriteHandle,
    received: Vec<(u32, u32)>,
}

/// Bundle state (store + in-progress transfer tracking).
pub struct State<S: FileStore> {
    pub store: S,
    in_progress: Option<InProgress<S>>,
    config: ReceiverConfig,
    pending_ack: Option<PendingAck>,
}

impl<S: FileStore> State<S> {
    /// Create new bundle state around a concrete store implementation.
    pub fn new(store: S) -> Self {
        Self {
            store,
            in_progress: None,
            config: ReceiverConfig::default(),
            pending_ack: None,
        }
    }

    /// Set receiver-side protocol configuration.
    pub fn set_config(&mut self, config: ReceiverConfig) {
        self.config = config;
    }

    /// Current receiver-side configuration.
    pub fn config(&self) -> ReceiverConfig {
        self.config
    }

    /// Take (and clear) any pending ack the receiver wants to send.
    pub fn take_pending_ack(&mut self) -> Option<PendingAck> {
        self.pending_ack.take()
    }

    /// Access metadata bytes for the currently in-progress transfer (if any).
    pub fn in_progress_metadata(&self) -> Option<&[u8]> {
        self.in_progress
            .as_ref()
            .map(|ip| ip.file_metadata.as_slice())
    }
}

impl<S: FileStore> Deps for State<S> {
    type Store = S;
    fn state(&mut self) -> &mut State<Self::Store> {
        self
    }
}

/// Handle an incoming `FileReq` message.
pub fn handle_file_req<'a, S: FileStore>(
    state: &mut State<S>,
    msg: thincan::CapnpTyped<'a, schema::file_req::Owned>,
) -> Result<(), Error<S::Error>> {
    let (transfer_id, total_len, file_metadata, sender_max_chunk_size) = msg
        .with_root(ReaderOptions::default(), |root| {
            (
                root.get_transfer_id(),
                root.get_total_len(),
                root.get_file_metadata().unwrap_or(&[]).to_vec(),
                root.get_sender_max_chunk_size(),
            )
        })
        .map_err(Error::Capnp)?;

    if let Some(in_progress) = state.in_progress.take() {
        state.store.abort(in_progress.handle);
    }

    let negotiated_chunk_size = match (state.config.max_chunk_size, sender_max_chunk_size) {
        (0, 0) => 0,
        (0, s) => s,
        (r, 0) => r,
        (r, s) => r.min(s),
    };

    let handle = state
        .store
        .begin_write(transfer_id, total_len)
        .map_err(Error::Store)?;
    if total_len == 0 {
        state.store.commit(handle).map_err(Error::Store)?;
        state.in_progress = None;
    } else {
        state.in_progress = Some(InProgress {
            transfer_id,
            total_len,
            max_chunk_size: negotiated_chunk_size,
            file_metadata,
            handle,
            received: Vec::new(),
        });
    }

    state.pending_ack = Some(PendingAck {
        transfer_id,
        kind: schema::FileAckKind::Accept,
        next_offset: 0,
        chunk_size: negotiated_chunk_size,
        error: schema::FileAckError::None,
    });
    Ok(())
}

fn insert_received_range(ranges: &mut Vec<(u32, u32)>, start: u32, end: u32) {
    if start >= end {
        return;
    }

    let mut i = 0usize;
    while i < ranges.len() && ranges[i].1 < start {
        i += 1;
    }

    let mut merged_start = start;
    let mut merged_end = end;
    while i < ranges.len() && ranges[i].0 <= merged_end {
        merged_start = merged_start.min(ranges[i].0);
        merged_end = merged_end.max(ranges[i].1);
        ranges.remove(i);
    }

    ranges.insert(i, (merged_start, merged_end));
}

/// Handle an incoming `FileChunk` message.
pub fn handle_file_chunk<'a, S: FileStore>(
    state: &mut State<S>,
    msg: thincan::CapnpTyped<'a, schema::file_chunk::Owned>,
) -> Result<(), Error<S::Error>> {
    msg.0.with_reader(ReaderOptions::default(), |reader| {
        let typed = capnp::message::TypedReader::<_, schema::file_chunk::Owned>::new(reader);
        let root = typed.get().map_err(Error::Capnp)?;

        let transfer_id = root.get_transfer_id();
        let offset = root.get_offset();
        let data = root.get_data().unwrap_or(&[]);

        let in_progress = state.in_progress.as_mut().ok_or(Error::Protocol)?;
        if in_progress.transfer_id != transfer_id {
            return Err(Error::Protocol);
        }

        if in_progress.max_chunk_size != 0 && data.len() > (in_progress.max_chunk_size as usize) {
            return Err(Error::Protocol);
        }

        let remaining = in_progress.total_len.saturating_sub(offset);
        if remaining == 0 {
            return Ok(());
        }

        let chunk_len = (data.len() as u32).min(remaining) as usize;
        state
            .store
            .write_at(&mut in_progress.handle, offset, &data[..chunk_len])
            .map_err(Error::Store)?;

        insert_received_range(
            &mut in_progress.received,
            offset,
            offset + (chunk_len as u32),
        );

        let mut next_offset = 0u32;
        if in_progress.received.first().is_some_and(|r| r.0 == 0) {
            next_offset = in_progress.received[0].1.min(in_progress.total_len);
        }

        state.pending_ack = Some(PendingAck {
            transfer_id,
            kind: schema::FileAckKind::Ack,
            next_offset,
            chunk_size: 0,
            error: schema::FileAckError::None,
        });

        if in_progress.received.len() == 1
            && in_progress.received[0].0 == 0
            && in_progress.received[0].1 >= in_progress.total_len
        {
            let in_progress = state.in_progress.take().ok_or(Error::Protocol)?;
            state
                .store
                .commit(in_progress.handle)
                .map_err(Error::Store)?;

            state.pending_ack = Some(PendingAck {
                transfer_id,
                kind: schema::FileAckKind::Complete,
                next_offset: in_progress.total_len,
                chunk_size: 0,
                error: schema::FileAckError::None,
            });
        }

        Ok(())
    })
}

/// Handle an incoming `FileAck` message.
pub fn handle_file_ack<'a, S: FileStore>(
    _state: &mut State<S>,
    _msg: thincan::CapnpTyped<'a, schema::file_ack::Owned>,
) -> Result<(), Error<S::Error>> {
    Ok(())
}

/// Transport abstraction for sending encoded messages.
pub trait SendEncoded {
    fn send_encoded<M: thincan::Message, V: thincan::Encode<M>>(
        &mut self,
        value: &V,
        timeout: Duration,
    ) -> Result<(), thincan::Error>;
}

impl<Node, Router, TxBuf> SendEncoded for thincan::Interface<Node, Router, TxBuf>
where
    Node: can_isotp_interface::IsoTpEndpoint,
    TxBuf: AsMut<[u8]>,
{
    fn send_encoded<M: thincan::Message, V: thincan::Encode<M>>(
        &mut self,
        value: &V,
        timeout: Duration,
    ) -> Result<(), thincan::Error> {
        thincan::Interface::send_encoded(self, value, timeout)
    }
}

/// Transport abstraction for sending encoded messages to a specific transport address.
pub trait SendEncodedTo<A> {
    fn send_encoded_to<M: thincan::Message, V: thincan::Encode<M>>(
        &mut self,
        to: A,
        value: &V,
        timeout: Duration,
    ) -> Result<(), thincan::Error>;
}

impl<Node, Router, TxBuf> SendEncodedTo<Node::ReplyTo> for thincan::Interface<Node, Router, TxBuf>
where
    Node: can_isotp_interface::IsoTpEndpointMeta,
    TxBuf: AsMut<[u8]>,
{
    fn send_encoded_to<M: thincan::Message, V: thincan::Encode<M>>(
        &mut self,
        to: Node::ReplyTo,
        value: &V,
        timeout: Duration,
    ) -> Result<(), thincan::Error> {
        thincan::Interface::send_encoded_to(self, to, value, timeout)
    }
}

/// High-level sender that handles `FileReq` + chunking for you.
#[derive(Debug, Clone)]
pub struct Sender<A> {
    next_transfer_id: u32,
    chunk_size: usize,
    _atlas: PhantomData<A>,
}

impl<A> Default for Sender<A> {
    fn default() -> Self {
        Self::new()
    }
}

impl<A> Sender<A> {
    /// Create a sender with a default chunk size.
    pub fn new() -> Self {
        Self {
            next_transfer_id: 1,
            chunk_size: DEFAULT_CHUNK_SIZE,
            _atlas: PhantomData,
        }
    }

    /// Create a sender with a custom chunk size.
    pub fn with_chunk_size(chunk_size: usize) -> Result<Self, thincan::Error> {
        let mut sender = Self::new();
        sender.set_chunk_size(chunk_size)?;
        Ok(sender)
    }

    /// Current chunk size in bytes.
    pub fn chunk_size(&self) -> usize {
        self.chunk_size
    }

    /// Set the chunk size in bytes.
    pub fn set_chunk_size(&mut self, chunk_size: usize) -> Result<(), thincan::Error> {
        if chunk_size == 0 {
            return Err(thincan::Error {
                kind: thincan::ErrorKind::Other,
            });
        }
        self.chunk_size = chunk_size;
        Ok(())
    }

    /// Next transfer id that will be used by [`send_file`].
    pub fn next_transfer_id(&self) -> u32 {
        self.next_transfer_id
    }

    /// Send a full file using an auto-assigned transfer id.
    pub fn send_file<T: SendEncoded>(
        &mut self,
        tx: &mut T,
        bytes: &[u8],
        timeout: Duration,
    ) -> Result<SendResult, thincan::Error>
    where
        A: Atlas,
    {
        let transfer_id = self.next_transfer_id;
        self.next_transfer_id = self.next_transfer_id.wrapping_add(1);
        self.send_file_with_id(tx, transfer_id, bytes, timeout)
    }

    /// Send a full file using an explicit transfer id.
    pub fn send_file_with_id<T: SendEncoded>(
        &mut self,
        tx: &mut T,
        transfer_id: u32,
        bytes: &[u8],
        timeout: Duration,
    ) -> Result<SendResult, thincan::Error>
    where
        A: Atlas,
    {
        if transfer_id >= self.next_transfer_id {
            self.next_transfer_id = transfer_id.wrapping_add(1);
        }
        if self.chunk_size == 0 {
            return Err(thincan::Error {
                kind: thincan::ErrorKind::Other,
            });
        }

        let total_len = bytes.len();
        let total_len_u32 = u32::try_from(total_len).map_err(|_| thincan::Error {
            kind: thincan::ErrorKind::Other,
        })?;

        let sender_max_chunk_size = u32::try_from(self.chunk_size).map_err(|_| thincan::Error {
            kind: thincan::ErrorKind::Other,
        })?;
        tx.send_encoded::<A::FileReq, _>(
            &file_offer::<A>(transfer_id, total_len_u32, sender_max_chunk_size, &[]),
            timeout,
        )?;

        let mut chunks = 0usize;
        for (index, chunk) in bytes.chunks(self.chunk_size).enumerate() {
            let offset = index.checked_mul(self.chunk_size).ok_or(thincan::Error {
                kind: thincan::ErrorKind::Other,
            })?;
            let offset_u32 = u32::try_from(offset).map_err(|_| thincan::Error {
                kind: thincan::ErrorKind::Other,
            })?;
            tx.send_encoded::<A::FileChunk, _>(
                &file_chunk::<A>(transfer_id, offset_u32, chunk),
                timeout,
            )?;
            chunks += 1;
        }

        Ok(SendResult {
            transfer_id,
            total_len,
            chunk_size: self.chunk_size,
            chunks,
        })
    }

    /// Send a full file to a specific transport address using an auto-assigned transfer id.
    pub fn send_file_to<T, Addr>(
        &mut self,
        tx: &mut T,
        to: Addr,
        bytes: &[u8],
        timeout: Duration,
    ) -> Result<SendResult, thincan::Error>
    where
        A: Atlas,
        T: SendEncodedTo<Addr>,
        Addr: Copy,
    {
        let transfer_id = self.next_transfer_id;
        self.next_transfer_id = self.next_transfer_id.wrapping_add(1);
        self.send_file_with_id_to(tx, to, transfer_id, bytes, timeout)
    }

    /// Send a full file to a specific transport address using an explicit transfer id.
    pub fn send_file_with_id_to<T, Addr>(
        &mut self,
        tx: &mut T,
        to: Addr,
        transfer_id: u32,
        bytes: &[u8],
        timeout: Duration,
    ) -> Result<SendResult, thincan::Error>
    where
        A: Atlas,
        T: SendEncodedTo<Addr>,
        Addr: Copy,
    {
        if transfer_id >= self.next_transfer_id {
            self.next_transfer_id = transfer_id.wrapping_add(1);
        }
        if self.chunk_size == 0 {
            return Err(thincan::Error {
                kind: thincan::ErrorKind::Other,
            });
        }

        let total_len = bytes.len();
        let total_len_u32 = u32::try_from(total_len).map_err(|_| thincan::Error {
            kind: thincan::ErrorKind::Other,
        })?;

        let sender_max_chunk_size = u32::try_from(self.chunk_size).map_err(|_| thincan::Error {
            kind: thincan::ErrorKind::Other,
        })?;
        tx.send_encoded_to::<A::FileReq, _>(
            to,
            &file_offer::<A>(transfer_id, total_len_u32, sender_max_chunk_size, &[]),
            timeout,
        )?;

        let mut chunks = 0usize;
        for (index, chunk) in bytes.chunks(self.chunk_size).enumerate() {
            let offset = index.checked_mul(self.chunk_size).ok_or(thincan::Error {
                kind: thincan::ErrorKind::Other,
            })?;
            let offset_u32 = u32::try_from(offset).map_err(|_| thincan::Error {
                kind: thincan::ErrorKind::Other,
            })?;
            tx.send_encoded_to::<A::FileChunk, _>(
                to,
                &file_chunk::<A>(transfer_id, offset_u32, chunk),
                timeout,
            )?;
            chunks += 1;
        }

        Ok(SendResult {
            transfer_id,
            total_len,
            chunk_size: self.chunk_size,
            chunks,
        })
    }
}

impl<A, H> thincan::BundleDispatch<A, H> for Bundle
where
    A: Atlas,
    H: Deps,
{
    fn try_dispatch<'a>(
        _parser: &<Bundle as thincan::BundleMeta<A>>::Parser,
        handlers: &mut H,
        msg: thincan::RawMessage<'a>,
    ) -> Option<Result<(), thincan::Error>> {
        if msg.id == <A::FileReq as thincan::Message>::ID {
            let parsed = thincan::CapnpTyped::<schema::file_req::Owned>::new(msg.body);
            if handle_file_req(handlers.state(), parsed).is_err() {
                return Some(Err(thincan::Error {
                    kind: thincan::ErrorKind::Other,
                }));
            }
            return Some(Ok(()));
        }

        if msg.id == <A::FileChunk as thincan::Message>::ID {
            let parsed = thincan::CapnpTyped::<schema::file_chunk::Owned>::new(msg.body);
            if handle_file_chunk(handlers.state(), parsed).is_err() {
                return Some(Err(thincan::Error {
                    kind: thincan::ErrorKind::Other,
                }));
            }
            return Some(Ok(()));
        }

        if msg.id == <A::FileAck as thincan::Message>::ID {
            let parsed = thincan::CapnpTyped::<schema::file_ack::Owned>::new(msg.body);
            if handle_file_ack(handlers.state(), parsed).is_err() {
                return Some(Err(thincan::Error {
                    kind: thincan::ErrorKind::Other,
                }));
            }
            return Some(Ok(()));
        }
        None
    }
}

impl<A> thincan::Encode<<A as Atlas>::FileReq> for FileReqValue<A>
where
    A: Atlas,
{
    fn max_encoded_len(&self) -> usize {
        crate::file_req_max_encoded_len()
    }

    fn encode(&self, out: &mut [u8]) -> Result<usize, thincan::Error> {
        let max_len = self.max_encoded_len();
        if out.len() < max_len {
            return Err(thincan::Error {
                kind: thincan::ErrorKind::Other,
            });
        }
        with_capnp_scratch_bytes(max_len, |scratch| {
            let mut message =
                capnp::message::Builder::new(capnp::message::SingleSegmentAllocator::new(scratch));
            let mut root: schema::file_req::Builder = message.init_root();
            root.set_transfer_id(self.transfer_id);
            root.set_total_len(self.total_len);

            let segments = message.get_segments_for_output();
            if segments.len() != 1 {
                return Err(thincan::Error {
                    kind: thincan::ErrorKind::Other,
                });
            }
            let bytes = segments[0];
            if bytes.len() > max_len {
                return Err(thincan::Error {
                    kind: thincan::ErrorKind::Other,
                });
            }
            out[..bytes.len()].copy_from_slice(bytes);
            Ok(bytes.len())
        })
    }
}

impl<'a, A> thincan::Encode<<A as Atlas>::FileReq> for FileOfferValue<'a, A>
where
    A: Atlas,
{
    fn max_encoded_len(&self) -> usize {
        crate::file_offer_max_encoded_len(self.file_metadata.len())
    }

    fn encode(&self, out: &mut [u8]) -> Result<usize, thincan::Error> {
        let max_len = self.max_encoded_len();
        if out.len() < max_len {
            return Err(thincan::Error {
                kind: thincan::ErrorKind::Other,
            });
        }
        with_capnp_scratch_bytes(max_len, |scratch| {
            let mut message =
                capnp::message::Builder::new(capnp::message::SingleSegmentAllocator::new(scratch));
            let mut root: schema::file_req::Builder = message.init_root();
            root.set_transfer_id(self.transfer_id);
            root.set_total_len(self.total_len);
            root.set_file_metadata(self.file_metadata);
            root.set_sender_max_chunk_size(self.sender_max_chunk_size);

            let segments = message.get_segments_for_output();
            if segments.len() != 1 {
                return Err(thincan::Error {
                    kind: thincan::ErrorKind::Other,
                });
            }
            let bytes = segments[0];
            if bytes.len() > max_len {
                return Err(thincan::Error {
                    kind: thincan::ErrorKind::Other,
                });
            }
            out[..bytes.len()].copy_from_slice(bytes);
            Ok(bytes.len())
        })
    }
}

impl<'a, A> thincan::Encode<<A as Atlas>::FileChunk> for FileChunkValue<'a, A>
where
    A: Atlas,
{
    fn max_encoded_len(&self) -> usize {
        crate::file_chunk_max_encoded_len(self.data.len())
    }

    fn encode(&self, out: &mut [u8]) -> Result<usize, thincan::Error> {
        let max_len = self.max_encoded_len();
        if out.len() < max_len {
            return Err(thincan::Error {
                kind: thincan::ErrorKind::Other,
            });
        }
        with_capnp_scratch_bytes(max_len, |scratch| {
            let mut message =
                capnp::message::Builder::new(capnp::message::SingleSegmentAllocator::new(scratch));
            let mut root: schema::file_chunk::Builder = message.init_root();
            root.set_transfer_id(self.transfer_id);
            root.set_offset(self.offset);
            root.set_data(self.data);

            let segments = message.get_segments_for_output();
            if segments.len() != 1 {
                return Err(thincan::Error {
                    kind: thincan::ErrorKind::Other,
                });
            }
            let bytes = segments[0];
            if bytes.len() > max_len {
                return Err(thincan::Error {
                    kind: thincan::ErrorKind::Other,
                });
            }
            out[..bytes.len()].copy_from_slice(bytes);
            Ok(bytes.len())
        })
    }
}

impl<A> thincan::Encode<<A as Atlas>::FileAck> for FileAckValue<A>
where
    A: Atlas,
{
    fn max_encoded_len(&self) -> usize {
        crate::file_ack_max_encoded_len()
    }

    fn encode(&self, out: &mut [u8]) -> Result<usize, thincan::Error> {
        let max_len = self.max_encoded_len();
        if out.len() < max_len {
            return Err(thincan::Error {
                kind: thincan::ErrorKind::Other,
            });
        }
        with_capnp_scratch_bytes(max_len, |scratch| {
            let mut message =
                capnp::message::Builder::new(capnp::message::SingleSegmentAllocator::new(scratch));
            let mut root: schema::file_ack::Builder = message.init_root();
            root.set_transfer_id(self.transfer_id);
            root.set_kind(self.kind);
            root.set_next_offset(self.next_offset);
            root.set_chunk_size(self.chunk_size);
            root.set_error(self.error);

            let segments = message.get_segments_for_output();
            if segments.len() != 1 {
                return Err(thincan::Error {
                    kind: thincan::ErrorKind::Other,
                });
            }
            let bytes = segments[0];
            if bytes.len() > max_len {
                return Err(thincan::Error {
                    kind: thincan::ErrorKind::Other,
                });
            }
            out[..bytes.len()].copy_from_slice(bytes);
            Ok(bytes.len())
        })
    }
}
