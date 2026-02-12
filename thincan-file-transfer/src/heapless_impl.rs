use capnp::message::ReaderOptions;
use core::marker::PhantomData;
use core::time::Duration;
use heapless::Vec;

use crate::{
    Atlas, Bundle, DEFAULT_CHUNK_SIZE, Error, FileStore, PendingAck, ReceiverConfig, SendResult,
    capnp_scratch_words_for_bytes, file_chunk_max_encoded_len, file_offer_max_encoded_len, schema,
};

fn set_pending_reject(
    state: &mut impl PendingAckSink,
    transfer_id: u32,
    err: schema::FileAckError,
) {
    state.set_pending_ack(PendingAck {
        transfer_id,
        kind: schema::FileAckKind::Reject,
        next_offset: 0,
        chunk_size: 0,
        error: err,
    });
}

fn set_pending_abort(state: &mut impl PendingAckSink, transfer_id: u32, err: schema::FileAckError) {
    state.set_pending_ack(PendingAck {
        transfer_id,
        kind: schema::FileAckKind::Abort,
        next_offset: 0,
        chunk_size: 0,
        error: err,
    });
}

trait PendingAckSink {
    fn set_pending_ack(&mut self, ack: PendingAck);
}

/// Receiver state machine for `no_alloc` environments (fixed-capacity buffers).
///
/// `MAX_METADATA` bounds how many bytes of `FileReq.fileMetadata` are retained.
/// `MAX_RANGES` bounds how fragmented/out-of-order chunk delivery can be tracked.
pub struct StateNoAlloc<S: FileStore, const MAX_METADATA: usize, const MAX_RANGES: usize> {
    pub store: S,
    in_progress: Option<InProgress<S, MAX_METADATA, MAX_RANGES>>,
    config: ReceiverConfig,
    pending_ack: Option<PendingAck>,
}

struct InProgress<S: FileStore, const MAX_METADATA: usize, const MAX_RANGES: usize> {
    transfer_id: u32,
    total_len: u32,
    max_chunk_size: u32,
    file_metadata: Vec<u8, MAX_METADATA>,
    handle: S::WriteHandle,
    received: Vec<(u32, u32), MAX_RANGES>,
}

impl<S: FileStore, const MAX_METADATA: usize, const MAX_RANGES: usize> PendingAckSink
    for StateNoAlloc<S, MAX_METADATA, MAX_RANGES>
{
    fn set_pending_ack(&mut self, ack: PendingAck) {
        self.pending_ack = Some(ack);
    }
}

impl<S: FileStore, const MAX_METADATA: usize, const MAX_RANGES: usize>
    StateNoAlloc<S, MAX_METADATA, MAX_RANGES>
{
    pub fn new(store: S) -> Self {
        Self {
            store,
            in_progress: None,
            config: ReceiverConfig::default(),
            pending_ack: None,
        }
    }

    pub fn set_config(&mut self, config: ReceiverConfig) {
        self.config = config;
    }

    pub fn config(&self) -> ReceiverConfig {
        self.config
    }

    pub fn take_pending_ack(&mut self) -> Option<PendingAck> {
        self.pending_ack.take()
    }

    pub fn in_progress_metadata(&self) -> Option<&[u8]> {
        self.in_progress
            .as_ref()
            .map(|ip| ip.file_metadata.as_slice())
    }
}

fn insert_received_range<const N: usize>(
    ranges: &mut Vec<(u32, u32), N>,
    start: u32,
    end: u32,
) -> Result<(), ()> {
    if start >= end {
        return Ok(());
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

    if ranges.len() == N {
        return Err(());
    }
    ranges
        .insert(i, (merged_start, merged_end))
        .map_err(|_| ())?;
    Ok(())
}

/// Handle an incoming `FileReq` message (no-alloc).
pub fn handle_file_req_no_alloc<
    'a,
    S: FileStore,
    const MAX_METADATA: usize,
    const MAX_RANGES: usize,
>(
    state: &mut StateNoAlloc<S, MAX_METADATA, MAX_RANGES>,
    msg: thincan::CapnpTyped<'a, schema::file_req::Owned>,
) -> Result<(), Error<S::Error>> {
    let (transfer_id, total_len, file_metadata, sender_max_chunk_size) = msg
        .with_root(ReaderOptions::default(), |root| {
            let transfer_id = root.get_transfer_id();
            let total_len = root.get_total_len();
            let sender_max_chunk_size = root.get_sender_max_chunk_size();

            let mut meta: Vec<u8, MAX_METADATA> = Vec::new();
            let meta_slice = root.get_file_metadata().unwrap_or(&[]);
            if meta.extend_from_slice(meta_slice).is_err() {
                return (transfer_id, total_len, None, sender_max_chunk_size);
            }

            (transfer_id, total_len, Some(meta), sender_max_chunk_size)
        })
        .map_err(Error::Capnp)?;

    if let Some(in_progress) = state.in_progress.take() {
        state.store.abort(in_progress.handle);
    }

    let Some(file_metadata) = file_metadata else {
        set_pending_reject(state, transfer_id, schema::FileAckError::InvalidRequest);
        return Ok(());
    };

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

/// Handle an incoming `FileChunk` message (no-alloc).
pub fn handle_file_chunk_no_alloc<
    'a,
    S: FileStore,
    const MAX_METADATA: usize,
    const MAX_RANGES: usize,
>(
    state: &mut StateNoAlloc<S, MAX_METADATA, MAX_RANGES>,
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

        if insert_received_range::<MAX_RANGES>(
            &mut in_progress.received,
            offset,
            offset + (chunk_len as u32),
        )
        .is_err()
        {
            let in_progress = state.in_progress.take().ok_or(Error::Protocol)?;
            state.store.abort(in_progress.handle);
            set_pending_abort(state, transfer_id, schema::FileAckError::Internal);
            return Ok(());
        }

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

/// Handle an incoming `FileAck` message (no-alloc).
pub fn handle_file_ack_no_alloc<
    'a,
    S: FileStore,
    const MAX_METADATA: usize,
    const MAX_RANGES: usize,
>(
    _state: &mut StateNoAlloc<S, MAX_METADATA, MAX_RANGES>,
    _msg: thincan::CapnpTyped<'a, schema::file_ack::Owned>,
) -> Result<(), Error<S::Error>> {
    Ok(())
}

impl<A, S, const MAX_METADATA: usize, const MAX_RANGES: usize>
    thincan::BundleDispatch<A, StateNoAlloc<S, MAX_METADATA, MAX_RANGES>> for Bundle
where
    A: Atlas,
    S: FileStore,
{
    fn try_dispatch<'a>(
        _parser: &<Bundle as thincan::BundleMeta<A>>::Parser,
        handlers: &mut StateNoAlloc<S, MAX_METADATA, MAX_RANGES>,
        msg: thincan::RawMessage<'a>,
    ) -> Option<Result<(), thincan::Error>> {
        if msg.id == <A::FileReq as thincan::Message>::ID {
            let parsed = thincan::CapnpTyped::<schema::file_req::Owned>::new(msg.body);
            if handle_file_req_no_alloc(handlers, parsed).is_err() {
                return Some(Err(thincan::Error {
                    kind: thincan::ErrorKind::Other,
                }));
            }
            return Some(Ok(()));
        }

        if msg.id == <A::FileChunk as thincan::Message>::ID {
            let parsed = thincan::CapnpTyped::<schema::file_chunk::Owned>::new(msg.body);
            if handle_file_chunk_no_alloc(handlers, parsed).is_err() {
                return Some(Err(thincan::Error {
                    kind: thincan::ErrorKind::Other,
                }));
            }
            return Some(Ok(()));
        }

        if msg.id == <A::FileAck as thincan::Message>::ID {
            let parsed = thincan::CapnpTyped::<schema::file_ack::Owned>::new(msg.body);
            if handle_file_ack_no_alloc(handlers, parsed).is_err() {
                return Some(Err(thincan::Error {
                    kind: thincan::ErrorKind::Other,
                }));
            }
            return Some(Ok(()));
        }

        None
    }
}

/// Transport abstraction for sending raw message bodies (no-alloc).
pub trait SendMsg {
    fn send_msg<M: thincan::Message>(
        &mut self,
        body: &[u8],
        timeout: Duration,
    ) -> Result<(), thincan::Error>;
}

impl<Node, Router, TxBuf> SendMsg for thincan::Interface<Node, Router, TxBuf>
where
    Node: can_isotp_interface::IsoTpEndpoint,
    TxBuf: AsMut<[u8]>,
{
    fn send_msg<M: thincan::Message>(
        &mut self,
        body: &[u8],
        timeout: Duration,
    ) -> Result<(), thincan::Error> {
        thincan::Interface::send_msg::<M>(self, body, timeout)
    }
}

/// Transport abstraction for sending raw message bodies to a specific address (no-alloc).
pub trait SendMsgTo<A> {
    fn send_msg_to<M: thincan::Message>(
        &mut self,
        to: A,
        body: &[u8],
        timeout: Duration,
    ) -> Result<(), thincan::Error>;
}

impl<Node, Router, TxBuf> SendMsgTo<Node::ReplyTo> for thincan::Interface<Node, Router, TxBuf>
where
    Node: can_isotp_interface::IsoTpEndpointMeta,
    TxBuf: AsMut<[u8]>,
{
    fn send_msg_to<M: thincan::Message>(
        &mut self,
        to: Node::ReplyTo,
        body: &[u8],
        timeout: Duration,
    ) -> Result<(), thincan::Error> {
        thincan::Interface::send_msg_to::<M>(self, to, body, timeout)
    }
}

/// Fixed-capacity Cap'n Proto scratch (no-alloc).
pub struct CapnpScratch<const WORDS: usize> {
    words: Vec<capnp::Word, WORDS>,
}

impl<const WORDS: usize> Default for CapnpScratch<WORDS> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const WORDS: usize> CapnpScratch<WORDS> {
    pub fn new() -> Self {
        Self { words: Vec::new() }
    }

    fn with_bytes<R>(
        &mut self,
        min_len: usize,
        f: impl FnOnce(&mut [u8]) -> R,
    ) -> Result<R, thincan::Error> {
        let min_words = capnp_scratch_words_for_bytes(min_len);
        if min_words > WORDS {
            return Err(thincan::Error {
                kind: thincan::ErrorKind::Other,
            });
        }
        while self.words.len() < min_words {
            self.words
                .push(capnp::word(0, 0, 0, 0, 0, 0, 0, 0))
                .map_err(|_| thincan::Error {
                    kind: thincan::ErrorKind::Other,
                })?;
        }
        let bytes = unsafe {
            core::slice::from_raw_parts_mut(
                self.words.as_mut_ptr() as *mut u8,
                self.words.len() * 8,
            )
        };
        Ok(f(&mut bytes[..min_len]))
    }
}

fn encode_file_offer_into<A: Atlas, const WORDS: usize>(
    scratch: &mut CapnpScratch<WORDS>,
    transfer_id: u32,
    total_len: u32,
    sender_max_chunk_size: u32,
    metadata: &[u8],
    out: &mut [u8],
) -> Result<usize, thincan::Error> {
    let max_len = file_offer_max_encoded_len(metadata.len());
    if out.len() < max_len {
        return Err(thincan::Error {
            kind: thincan::ErrorKind::Other,
        });
    }
    scratch.with_bytes(max_len, |scratch_bytes| {
        let mut message = capnp::message::Builder::new(
            capnp::message::SingleSegmentAllocator::new(scratch_bytes),
        );
        let mut root: schema::file_req::Builder = message.init_root();
        root.set_transfer_id(transfer_id);
        root.set_total_len(total_len);
        root.set_file_metadata(metadata);
        root.set_sender_max_chunk_size(sender_max_chunk_size);

        let segments = message.get_segments_for_output();
        if segments.len() != 1 {
            return Err(thincan::Error {
                kind: thincan::ErrorKind::Other,
            });
        }
        let bytes = segments[0];
        out[..bytes.len()].copy_from_slice(bytes);
        Ok(bytes.len())
    })?
}

fn encode_file_chunk_into<const WORDS: usize>(
    scratch: &mut CapnpScratch<WORDS>,
    transfer_id: u32,
    offset: u32,
    data: &[u8],
    out: &mut [u8],
) -> Result<usize, thincan::Error> {
    let max_len = file_chunk_max_encoded_len(data.len());
    if out.len() < max_len {
        return Err(thincan::Error {
            kind: thincan::ErrorKind::Other,
        });
    }
    scratch.with_bytes(max_len, |scratch_bytes| {
        let mut message = capnp::message::Builder::new(
            capnp::message::SingleSegmentAllocator::new(scratch_bytes),
        );
        let mut root: schema::file_chunk::Builder = message.init_root();
        root.set_transfer_id(transfer_id);
        root.set_offset(offset);
        root.set_data(data);

        let segments = message.get_segments_for_output();
        if segments.len() != 1 {
            return Err(thincan::Error {
                kind: thincan::ErrorKind::Other,
            });
        }
        let bytes = segments[0];
        out[..bytes.len()].copy_from_slice(bytes);
        Ok(bytes.len())
    })?
}

/// No-alloc sender that uses fixed buffers (`MAX_BODY`) and scratch (`SCRATCH_WORDS`).
///
/// This avoids `thincan::Encode` and instead emits raw Cap'n Proto bodies via [`SendMsg`].
pub struct SenderNoAlloc<A, const MAX_BODY: usize, const SCRATCH_WORDS: usize> {
    next_transfer_id: u32,
    chunk_size: usize,
    scratch: CapnpScratch<SCRATCH_WORDS>,
    buf: [u8; MAX_BODY],
    _atlas: PhantomData<A>,
}

impl<A, const MAX_BODY: usize, const SCRATCH_WORDS: usize> Default
    for SenderNoAlloc<A, MAX_BODY, SCRATCH_WORDS>
{
    fn default() -> Self {
        Self::new()
    }
}

impl<A, const MAX_BODY: usize, const SCRATCH_WORDS: usize>
    SenderNoAlloc<A, MAX_BODY, SCRATCH_WORDS>
{
    pub fn new() -> Self {
        Self {
            next_transfer_id: 1,
            chunk_size: DEFAULT_CHUNK_SIZE,
            scratch: CapnpScratch::new(),
            buf: [0u8; MAX_BODY],
            _atlas: PhantomData,
        }
    }

    pub fn with_chunk_size(chunk_size: usize) -> Result<Self, thincan::Error> {
        let mut sender = Self::new();
        sender.set_chunk_size(chunk_size)?;
        Ok(sender)
    }

    pub fn chunk_size(&self) -> usize {
        self.chunk_size
    }

    pub fn set_chunk_size(&mut self, chunk_size: usize) -> Result<(), thincan::Error> {
        if chunk_size == 0 {
            return Err(thincan::Error {
                kind: thincan::ErrorKind::Other,
            });
        }
        let max_body = file_chunk_max_encoded_len(chunk_size);
        if max_body > MAX_BODY {
            return Err(thincan::Error {
                kind: thincan::ErrorKind::Other,
            });
        }
        let needed_words = capnp_scratch_words_for_bytes(max_body);
        if needed_words > SCRATCH_WORDS {
            return Err(thincan::Error {
                kind: thincan::ErrorKind::Other,
            });
        }
        self.chunk_size = chunk_size;
        Ok(())
    }

    pub fn next_transfer_id(&self) -> u32 {
        self.next_transfer_id
    }

    pub fn send_file<T: SendMsg>(
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

    pub fn send_file_with_id<T: SendMsg>(
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

        let req_len = encode_file_offer_into::<A, SCRATCH_WORDS>(
            &mut self.scratch,
            transfer_id,
            total_len_u32,
            sender_max_chunk_size,
            &[],
            &mut self.buf,
        )?;
        tx.send_msg::<A::FileReq>(&self.buf[..req_len], timeout)?;

        let mut chunks = 0usize;
        for (index, chunk) in bytes.chunks(self.chunk_size).enumerate() {
            let offset = index.checked_mul(self.chunk_size).ok_or(thincan::Error {
                kind: thincan::ErrorKind::Other,
            })?;
            let offset_u32 = u32::try_from(offset).map_err(|_| thincan::Error {
                kind: thincan::ErrorKind::Other,
            })?;
            let used = encode_file_chunk_into::<SCRATCH_WORDS>(
                &mut self.scratch,
                transfer_id,
                offset_u32,
                chunk,
                &mut self.buf,
            )?;
            tx.send_msg::<A::FileChunk>(&self.buf[..used], timeout)?;
            chunks += 1;
        }

        Ok(SendResult {
            transfer_id,
            total_len,
            chunk_size: self.chunk_size,
            chunks,
        })
    }

    pub fn send_file_to<T, Addr>(
        &mut self,
        tx: &mut T,
        to: Addr,
        bytes: &[u8],
        timeout: Duration,
    ) -> Result<SendResult, thincan::Error>
    where
        A: Atlas,
        T: SendMsgTo<Addr>,
        Addr: Copy,
    {
        let transfer_id = self.next_transfer_id;
        self.next_transfer_id = self.next_transfer_id.wrapping_add(1);
        self.send_file_with_id_to(tx, to, transfer_id, bytes, timeout)
    }

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
        T: SendMsgTo<Addr>,
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

        let req_len = encode_file_offer_into::<A, SCRATCH_WORDS>(
            &mut self.scratch,
            transfer_id,
            total_len_u32,
            sender_max_chunk_size,
            &[],
            &mut self.buf,
        )?;
        tx.send_msg_to::<A::FileReq>(to, &self.buf[..req_len], timeout)?;

        let mut chunks = 0usize;
        for (index, chunk) in bytes.chunks(self.chunk_size).enumerate() {
            let offset = index.checked_mul(self.chunk_size).ok_or(thincan::Error {
                kind: thincan::ErrorKind::Other,
            })?;
            let offset_u32 = u32::try_from(offset).map_err(|_| thincan::Error {
                kind: thincan::ErrorKind::Other,
            })?;
            let used = encode_file_chunk_into::<SCRATCH_WORDS>(
                &mut self.scratch,
                transfer_id,
                offset_u32,
                chunk,
                &mut self.buf,
            )?;
            tx.send_msg_to::<A::FileChunk>(to, &self.buf[..used], timeout)?;
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
