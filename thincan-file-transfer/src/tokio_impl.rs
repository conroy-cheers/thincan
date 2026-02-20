#![cfg(any(feature = "tokio", feature = "embassy"))]

use core::time::Duration;

use capnp::message::ReaderOptions;

#[cfg(feature = "alloc")]
use alloc::vec::Vec;

#[cfg(feature = "heapless")]
use heapless::Vec as HeaplessVec;

#[cfg(feature = "heapless")]
use core::cmp::min;

use crate::{
    Atlas, Error, FileAckValue, FileTransferBundle, ReceiverConfig, file_ack_accept,
    file_ack_progress, file_ack_reject, file_chunk, file_offer, schema,
};

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
    pub max_retries: usize,
}

impl Default for SendConfig {
    fn default() -> Self {
        Self {
            sender_max_chunk_size: crate::DEFAULT_CHUNK_SIZE,
            window_chunks: 4,
            max_retries: 5,
        }
    }
}

impl SendConfig {
    fn validate(self) -> Result<Self, thincan::Error> {
        if self.sender_max_chunk_size == 0 || self.window_chunks == 0 {
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
{
    doodad: thincan::DoodadHandle<
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
    >,
    send_state: SendState,
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
    Node: 'a,
    TxBuf: 'a,
{
    pub fn new(
        doodad: thincan::DoodadHandle<
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
        >,
    ) -> Self {
        Self {
            doodad,
            send_state: SendState::new(),
        }
    }

    pub fn send_state(&self) -> &SendState {
        &self.send_state
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

/// Result of a bundle-driven send operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SendFileResult {
    pub transfer_id: u32,
    pub total_len: usize,
    pub chunk_size: usize,
    pub chunks_sent: usize,
    pub retries: usize,
}

/// Result of a one-shot bundle-driven receive operation.
#[cfg(feature = "alloc")]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecvFileResult {
    pub transfer_id: u32,
    pub total_len: u32,
    pub received_len: u32,
    pub sender_max_chunk_size: u32,
    pub negotiated_chunk_size: u32,
    pub metadata: Vec<u8>,
}

/// Result of a one-shot bundle-driven receive operation without allocation.
#[cfg(feature = "heapless")]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecvFileResultNoAlloc<const MAX_METADATA: usize> {
    pub transfer_id: u32,
    pub total_len: u32,
    pub received_len: u32,
    pub sender_max_chunk_size: u32,
    pub negotiated_chunk_size: u32,
    pub metadata: HeaplessVec<u8, MAX_METADATA>,
}

fn other_error() -> thincan::Error {
    thincan::Error {
        kind: thincan::ErrorKind::Other,
    }
}

trait DoodadCapnpExt<const MAX_BODY: usize> {
    async fn send_to<M: thincan::CapnpMessage, V: thincan::EncodeCapnp<M>>(
        &self,
        to: u8,
        value: &V,
        timeout: Duration,
    ) -> Result<(), thincan::Error>;

    async fn recv_next_from<M: thincan::CapnpMessage>(
        &self,
        from: u8,
    ) -> Result<thincan::Received<M, MAX_BODY>, thincan::Error>;

    async fn recv_next_from_where<M: thincan::CapnpMessage, F>(
        &self,
        from: u8,
        predicate: F,
    ) -> Result<thincan::Received<M, MAX_BODY>, thincan::Error>
    where
        F: FnMut(&[u8]) -> bool;
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
> DoodadCapnpExt<MAX_BODY>
    for thincan::DoodadHandle<
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
    >
where
    A: Atlas,
    Maplet: thincan::MapletSpec<MAX_TYPES> + thincan::MapletHasBundle<FileTransferBundle<A>>,
    RM: thincan::RawMutex,
    Node: can_isotp_interface::IsoTpAsyncEndpoint,
    TxBuf: AsMut<[u8]>,
{
    async fn send_to<M: thincan::CapnpMessage, V: thincan::EncodeCapnp<M>>(
        &self,
        to: u8,
        value: &V,
        timeout: Duration,
    ) -> Result<(), thincan::Error> {
        self.__send_capnp_to::<M, V>(to, value, timeout).await
    }

    async fn recv_next_from<M: thincan::CapnpMessage>(
        &self,
        from: u8,
    ) -> Result<thincan::Received<M, MAX_BODY>, thincan::Error> {
        self.__recv_next_capnp_from::<M>(from).await
    }

    async fn recv_next_from_where<M: thincan::CapnpMessage, F>(
        &self,
        from: u8,
        predicate: F,
    ) -> Result<thincan::Received<M, MAX_BODY>, thincan::Error>
    where
        F: FnMut(&[u8]) -> bool,
    {
        self.__recv_next_capnp_from_where::<M, F>(from, predicate)
            .await
    }
}

fn ack_transfer_id(body: &[u8]) -> Result<u32, thincan::Error> {
    thincan::CapnpTyped::<schema::file_ack::Owned>::new(body)
        .with_root(ReaderOptions::default(), |root| root.get_transfer_id())
        .map_err(|_| other_error())
}

async fn wait_for_transfer_ack<
    A,
    Maplet,
    RM,
    Node,
    TxBuf,
    const MAX_TYPES: usize,
    const DEPTH: usize,
    const MAX_BODY: usize,
    const MAX_WAITERS: usize,
>(
    doodad: &thincan::DoodadHandle<
        '_,
        Maplet,
        RM,
        Node,
        TxBuf,
        MAX_TYPES,
        DEPTH,
        MAX_BODY,
        MAX_WAITERS,
        FileTransferBundle<A>,
    >,
    from: u8,
    transfer_id: u32,
    timeout: Duration,
) -> Result<Ack, thincan::Error>
where
    A: Atlas,
    Maplet: thincan::MapletSpec<MAX_TYPES> + thincan::MapletHasBundle<FileTransferBundle<A>>,
    RM: thincan::RawMutex,
    Node: can_isotp_interface::IsoTpAsyncEndpoint,
    TxBuf: AsMut<[u8]>,
{
    #[cfg(feature = "tokio")]
    let received = {
        let wait = doodad.recv_next_from_where::<A::FileAck, _>(from, |body| {
            ack_transfer_id(body)
                .map(|id| id == transfer_id)
                .unwrap_or(true)
        });
        match tokio::time::timeout(timeout, wait).await {
            Ok(v) => v?,
            Err(_) => return Err(thincan::Error::timeout()),
        }
    };

    #[cfg(not(feature = "tokio"))]
    let received = {
        let _ = timeout;
        doodad
            .recv_next_from_where::<A::FileAck, _>(from, |body| {
                ack_transfer_id(body)
                    .map(|id| id == transfer_id)
                    .unwrap_or(true)
            })
            .await?
    };

    let ack = decode_file_ack(thincan::CapnpTyped::<schema::file_ack::Owned>::new(
        received.body(),
    ))?;

    if ack.transfer_id != transfer_id {
        return Err(other_error());
    }

    Ok(ack)
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

async fn send_file_impl<
    A,
    Maplet,
    RM,
    Node,
    TxBuf,
    const MAX_TYPES: usize,
    const DEPTH: usize,
    const MAX_BODY: usize,
    const MAX_WAITERS: usize,
>(
    doodad: &thincan::DoodadHandle<
        '_,
        Maplet,
        RM,
        Node,
        TxBuf,
        MAX_TYPES,
        DEPTH,
        MAX_BODY,
        MAX_WAITERS,
        FileTransferBundle<A>,
    >,
    to: u8,
    state: &mut SendState,
    bytes: &[u8],
    timeout: Duration,
    config: SendConfig,
) -> Result<SendFileResult, thincan::Error>
where
    A: Atlas,
    Maplet: thincan::MapletSpec<MAX_TYPES> + thincan::MapletHasBundle<FileTransferBundle<A>>,
    RM: thincan::RawMutex,
    Node: can_isotp_interface::IsoTpAsyncEndpoint,
    TxBuf: AsMut<[u8]>,
{
    let transfer_id = state.alloc_transfer_id();
    send_file_with_id_impl(doodad, to, state, transfer_id, bytes, timeout, config).await
}

async fn send_file_with_id_impl<
    A,
    Maplet,
    RM,
    Node,
    TxBuf,
    const MAX_TYPES: usize,
    const DEPTH: usize,
    const MAX_BODY: usize,
    const MAX_WAITERS: usize,
>(
    doodad: &thincan::DoodadHandle<
        '_,
        Maplet,
        RM,
        Node,
        TxBuf,
        MAX_TYPES,
        DEPTH,
        MAX_BODY,
        MAX_WAITERS,
        FileTransferBundle<A>,
    >,
    to: u8,
    state: &mut SendState,
    transfer_id: u32,
    bytes: &[u8],
    timeout: Duration,
    config: SendConfig,
) -> Result<SendFileResult, thincan::Error>
where
    A: Atlas,
    Maplet: thincan::MapletSpec<MAX_TYPES> + thincan::MapletHasBundle<FileTransferBundle<A>>,
    RM: thincan::RawMutex,
    Node: can_isotp_interface::IsoTpAsyncEndpoint,
    TxBuf: AsMut<[u8]>,
{
    let config = config.validate()?;
    state.observe_transfer_id(transfer_id);

    let total_len = bytes.len();
    let total_len_u32 = u32::try_from(total_len).map_err(|_| other_error())?;
    let max_chunk_u32 = u32::try_from(config.sender_max_chunk_size).map_err(|_| other_error())?;

    doodad
        .send_to::<A::FileReq, _>(
            to,
            &file_offer::<A>(transfer_id, total_len_u32, max_chunk_u32, &[]),
            timeout,
        )
        .await?;

    let accept = loop {
        let ack =
            wait_for_transfer_ack::<A, _, _, _, _, _, _, _, _>(doodad, to, transfer_id, timeout)
                .await?;
        match ack.kind {
            schema::FileAckKind::Accept => break ack,
            schema::FileAckKind::Reject | schema::FileAckKind::Abort => {
                return Err(other_error());
            }
            _ => continue,
        }
    };

    let chunk_size = usize::try_from(accept.chunk_size).map_err(|_| other_error())?;
    if chunk_size == 0 || chunk_size > config.sender_max_chunk_size {
        return Err(other_error());
    }

    let mut chunks_sent = 0usize;
    let mut retries = 0usize;

    let mut last_acked = 0u32;
    let mut next_to_send = 0u32;

    while (last_acked as usize) < total_len {
        while inflight_chunk_count(last_acked, next_to_send, chunk_size)? < config.window_chunks
            && (next_to_send as usize) < total_len
        {
            let offset = next_to_send as usize;
            let end = (offset + chunk_size).min(total_len);
            let data = &bytes[offset..end];

            doodad
                .send_to::<A::FileChunk, _>(
                    to,
                    &file_chunk::<A>(transfer_id, next_to_send, data),
                    timeout,
                )
                .await?;

            next_to_send = next_to_send
                .checked_add(u32::try_from(data.len()).map_err(|_| other_error())?)
                .ok_or_else(other_error)?;
            chunks_sent += 1;
        }

        let ack = match wait_for_transfer_ack::<A, _, _, _, _, _, _, _, _>(
            doodad,
            to,
            transfer_id,
            timeout,
        )
        .await
        {
            Ok(a) => a,
            Err(e) if e.kind == thincan::ErrorKind::Timeout => {
                retries += 1;
                if retries > config.max_retries {
                    return Err(e);
                }
                next_to_send = last_acked;
                continue;
            }
            Err(e) => return Err(e),
        };

        match ack.kind {
            schema::FileAckKind::Ack | schema::FileAckKind::Complete => {}
            schema::FileAckKind::Reject | schema::FileAckKind::Abort => {
                return Err(other_error());
            }
            schema::FileAckKind::Accept => continue,
        }

        let next = ack.next_offset;
        if next < last_acked || (next as usize) > total_len || next > next_to_send {
            return Err(other_error());
        }

        last_acked = next;

        if ack.kind == schema::FileAckKind::Complete {
            if (last_acked as usize) != total_len {
                return Err(other_error());
            }
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

#[cfg(feature = "alloc")]
async fn recv_file_impl<
    A,
    Maplet,
    RM,
    Node,
    TxBuf,
    S,
    const MAX_TYPES: usize,
    const DEPTH: usize,
    const MAX_BODY: usize,
    const MAX_WAITERS: usize,
>(
    doodad: &thincan::DoodadHandle<
        '_,
        Maplet,
        RM,
        Node,
        TxBuf,
        MAX_TYPES,
        DEPTH,
        MAX_BODY,
        MAX_WAITERS,
        FileTransferBundle<A>,
    >,
    from: u8,
    store: &mut S,
    send_timeout: Duration,
    receiver: ReceiverConfig,
) -> Result<RecvFileResult, Error<S::Error>>
where
    A: Atlas,
    Maplet: thincan::MapletSpec<MAX_TYPES> + thincan::MapletHasBundle<FileTransferBundle<A>>,
    RM: thincan::RawMutex,
    Node: can_isotp_interface::IsoTpAsyncEndpoint,
    TxBuf: AsMut<[u8]>,
    S: crate::AsyncFileStore,
{
    let req = doodad
        .recv_next_from::<A::FileReq>(from)
        .await
        .map_err(|_| Error::Protocol)?;

    let (transfer_id, total_len, sender_max_chunk_size, metadata) =
        thincan::CapnpTyped::<schema::file_req::Owned>::new(req.body())
            .with_root(ReaderOptions::default(), |root| {
                (
                    root.get_transfer_id(),
                    root.get_total_len(),
                    root.get_sender_max_chunk_size(),
                    root.get_file_metadata().unwrap_or(&[]).to_vec(),
                )
            })
            .map_err(Error::Capnp)?;

    let negotiated_chunk_size = match (receiver.max_chunk_size, sender_max_chunk_size) {
        (0, 0) => 0,
        (0, s) => s,
        (r, 0) => r,
        (r, s) => r.min(s),
    };

    if negotiated_chunk_size == 0 {
        let reject = file_ack_reject::<A>(transfer_id, schema::FileAckError::ChunkSizeTooSmall);
        let _ = doodad
            .send_to::<A::FileAck, _>(from, &reject, send_timeout)
            .await;
        return Err(Error::Protocol);
    }

    let mut handle = store
        .begin_write(transfer_id, total_len)
        .await
        .map_err(Error::Store)?;

    let accept = file_ack_accept::<A>(transfer_id, negotiated_chunk_size);
    if doodad
        .send_to::<A::FileAck, _>(from, &accept, send_timeout)
        .await
        .is_err()
    {
        store.abort(handle).await;
        return Err(Error::Protocol);
    }

    if total_len == 0 {
        store.commit(handle).await.map_err(Error::Store)?;
        return Ok(RecvFileResult {
            transfer_id,
            total_len,
            received_len: 0,
            sender_max_chunk_size,
            negotiated_chunk_size,
            metadata,
        });
    }

    let mut next_offset = 0u32;
    while next_offset < total_len {
        let chunk = match doodad.recv_next_from::<A::FileChunk>(from).await {
            Ok(v) => v,
            Err(_) => {
                store.abort(handle).await;
                return Err(Error::Protocol);
            }
        };

        let parsed = thincan::CapnpTyped::<schema::file_chunk::Owned>::new(chunk.body())
            .with_root(ReaderOptions::default(), |root| {
                (
                    root.get_transfer_id(),
                    root.get_offset(),
                    root.get_data().unwrap_or(&[]).to_vec(),
                )
            })
            .map_err(Error::Capnp);

        let (chunk_transfer_id, offset, data) = match parsed {
            Ok(v) => v,
            Err(e) => {
                store.abort(handle).await;
                let abort = FileAckValue::<A>::new(
                    transfer_id,
                    schema::FileAckKind::Abort,
                    next_offset,
                    0,
                    schema::FileAckError::InvalidRequest,
                );
                let _ = doodad
                    .send_to::<A::FileAck, _>(from, &abort, send_timeout)
                    .await;
                return Err(e);
            }
        };

        if chunk_transfer_id != transfer_id {
            store.abort(handle).await;
            let abort = FileAckValue::<A>::new(
                transfer_id,
                schema::FileAckKind::Abort,
                next_offset,
                0,
                schema::FileAckError::InvalidRequest,
            );
            let _ = doodad
                .send_to::<A::FileAck, _>(from, &abort, send_timeout)
                .await;
            return Err(Error::Protocol);
        }

        if (data.len() as u32) > negotiated_chunk_size {
            store.abort(handle).await;
            let abort = FileAckValue::<A>::new(
                transfer_id,
                schema::FileAckKind::Abort,
                next_offset,
                0,
                schema::FileAckError::ChunkSizeTooLarge,
            );
            let _ = doodad
                .send_to::<A::FileAck, _>(from, &abort, send_timeout)
                .await;
            return Err(Error::Protocol);
        }

        if offset != next_offset {
            let progress = file_ack_progress::<A>(transfer_id, next_offset);
            if doodad
                .send_to::<A::FileAck, _>(from, &progress, send_timeout)
                .await
                .is_err()
            {
                store.abort(handle).await;
                return Err(Error::Protocol);
            }
            continue;
        }

        let remaining = total_len.saturating_sub(next_offset);
        let write_len = (data.len() as u32).min(remaining) as usize;
        if write_len == 0 {
            continue;
        }

        if let Err(e) = store
            .write_at(&mut handle, offset, &data[..write_len])
            .await
        {
            store.abort(handle).await;
            return Err(Error::Store(e));
        }
        next_offset = next_offset.saturating_add(write_len as u32);

        if next_offset >= total_len {
            store.commit(handle).await.map_err(Error::Store)?;
            let complete = FileAckValue::<A>::new(
                transfer_id,
                schema::FileAckKind::Complete,
                total_len,
                0,
                schema::FileAckError::None,
            );
            if doodad
                .send_to::<A::FileAck, _>(from, &complete, send_timeout)
                .await
                .is_err()
            {
                return Err(Error::Protocol);
            }

            return Ok(RecvFileResult {
                transfer_id,
                total_len,
                received_len: total_len,
                sender_max_chunk_size,
                negotiated_chunk_size,
                metadata,
            });
        }

        let progress = file_ack_progress::<A>(transfer_id, next_offset);
        if doodad
            .send_to::<A::FileAck, _>(from, &progress, send_timeout)
            .await
            .is_err()
        {
            store.abort(handle).await;
            return Err(Error::Protocol);
        }
    }

    Err(Error::Protocol)
}

#[cfg(feature = "heapless")]
async fn recv_file_no_alloc_impl<
    A,
    Maplet,
    RM,
    Node,
    TxBuf,
    S,
    const MAX_TYPES: usize,
    const DEPTH: usize,
    const MAX_BODY: usize,
    const MAX_WAITERS: usize,
    const MAX_METADATA: usize,
    const MAX_CHUNK: usize,
>(
    doodad: &thincan::DoodadHandle<
        '_,
        Maplet,
        RM,
        Node,
        TxBuf,
        MAX_TYPES,
        DEPTH,
        MAX_BODY,
        MAX_WAITERS,
        FileTransferBundle<A>,
    >,
    from: u8,
    store: &mut S,
    send_timeout: Duration,
    receiver: ReceiverConfig,
) -> Result<RecvFileResultNoAlloc<MAX_METADATA>, Error<S::Error>>
where
    A: Atlas,
    Maplet: thincan::MapletSpec<MAX_TYPES> + thincan::MapletHasBundle<FileTransferBundle<A>>,
    RM: thincan::RawMutex,
    Node: can_isotp_interface::IsoTpAsyncEndpoint,
    TxBuf: AsMut<[u8]>,
    S: crate::AsyncFileStore,
{
    let req = doodad
        .recv_next_from::<A::FileReq>(from)
        .await
        .map_err(|_| Error::Protocol)?;

    let (transfer_id, total_len, sender_max_chunk_size, metadata, metadata_fits) =
        thincan::CapnpTyped::<schema::file_req::Owned>::new(req.body())
            .with_root(ReaderOptions::default(), |root| {
                let transfer_id = root.get_transfer_id();
                let total_len = root.get_total_len();
                let sender_max_chunk_size = root.get_sender_max_chunk_size();
                let mut metadata = HeaplessVec::<u8, MAX_METADATA>::new();
                let metadata_fits = metadata
                    .extend_from_slice(root.get_file_metadata().unwrap_or(&[]))
                    .is_ok();
                (
                    transfer_id,
                    total_len,
                    sender_max_chunk_size,
                    metadata,
                    metadata_fits,
                )
            })
            .map_err(Error::Capnp)?;

    if !metadata_fits {
        let reject = file_ack_reject::<A>(transfer_id, schema::FileAckError::InvalidRequest);
        let _ = doodad
            .send_to::<A::FileAck, _>(from, &reject, send_timeout)
            .await;
        return Err(Error::Protocol);
    }

    let negotiated_chunk_size = match (receiver.max_chunk_size, sender_max_chunk_size) {
        (0, 0) => 0,
        (0, s) => s,
        (r, 0) => r,
        (r, s) => r.min(s),
    };

    if negotiated_chunk_size == 0 {
        let reject = file_ack_reject::<A>(transfer_id, schema::FileAckError::ChunkSizeTooSmall);
        let _ = doodad
            .send_to::<A::FileAck, _>(from, &reject, send_timeout)
            .await;
        return Err(Error::Protocol);
    }

    let mut handle = store
        .begin_write(transfer_id, total_len)
        .await
        .map_err(Error::Store)?;

    let accept = file_ack_accept::<A>(transfer_id, negotiated_chunk_size);
    if doodad
        .send_to::<A::FileAck, _>(from, &accept, send_timeout)
        .await
        .is_err()
    {
        store.abort(handle).await;
        return Err(Error::Protocol);
    }

    if total_len == 0 {
        store.commit(handle).await.map_err(Error::Store)?;
        return Ok(RecvFileResultNoAlloc {
            transfer_id,
            total_len,
            received_len: 0,
            sender_max_chunk_size,
            negotiated_chunk_size,
            metadata,
        });
    }

    let mut next_offset = 0u32;
    while next_offset < total_len {
        let chunk = match doodad.recv_next_from::<A::FileChunk>(from).await {
            Ok(v) => v,
            Err(_) => {
                store.abort(handle).await;
                return Err(Error::Protocol);
            }
        };

        let mut chunk_data = [0u8; MAX_CHUNK];
        let parsed = thincan::CapnpTyped::<schema::file_chunk::Owned>::new(chunk.body())
            .with_root(ReaderOptions::default(), |root| {
                let chunk_transfer_id = root.get_transfer_id();
                let offset = root.get_offset();
                let data = root.get_data().unwrap_or(&[]);
                let copy_len = min(data.len(), MAX_CHUNK);
                chunk_data[..copy_len].copy_from_slice(&data[..copy_len]);
                (chunk_transfer_id, offset, data.len(), copy_len)
            })
            .map_err(Error::Capnp);

        let (chunk_transfer_id, offset, data_len, copy_len) = match parsed {
            Ok(v) => v,
            Err(e) => {
                store.abort(handle).await;
                let abort = FileAckValue::<A>::new(
                    transfer_id,
                    schema::FileAckKind::Abort,
                    next_offset,
                    0,
                    schema::FileAckError::InvalidRequest,
                );
                let _ = doodad
                    .send_to::<A::FileAck, _>(from, &abort, send_timeout)
                    .await;
                return Err(e);
            }
        };

        if chunk_transfer_id != transfer_id {
            store.abort(handle).await;
            let abort = FileAckValue::<A>::new(
                transfer_id,
                schema::FileAckKind::Abort,
                next_offset,
                0,
                schema::FileAckError::InvalidRequest,
            );
            let _ = doodad
                .send_to::<A::FileAck, _>(from, &abort, send_timeout)
                .await;
            return Err(Error::Protocol);
        }

        if data_len > MAX_CHUNK || (data_len as u32) > negotiated_chunk_size {
            store.abort(handle).await;
            let abort = FileAckValue::<A>::new(
                transfer_id,
                schema::FileAckKind::Abort,
                next_offset,
                0,
                schema::FileAckError::ChunkSizeTooLarge,
            );
            let _ = doodad
                .send_to::<A::FileAck, _>(from, &abort, send_timeout)
                .await;
            return Err(Error::Protocol);
        }

        if offset != next_offset {
            let progress = file_ack_progress::<A>(transfer_id, next_offset);
            if doodad
                .send_to::<A::FileAck, _>(from, &progress, send_timeout)
                .await
                .is_err()
            {
                store.abort(handle).await;
                return Err(Error::Protocol);
            }
            continue;
        }

        let remaining = total_len.saturating_sub(next_offset);
        let write_len = min(copy_len, remaining as usize);
        if write_len == 0 {
            continue;
        }

        if let Err(e) = store
            .write_at(&mut handle, offset, &chunk_data[..write_len])
            .await
        {
            store.abort(handle).await;
            return Err(Error::Store(e));
        }
        next_offset = next_offset.saturating_add(write_len as u32);

        if next_offset >= total_len {
            store.commit(handle).await.map_err(Error::Store)?;
            let complete = FileAckValue::<A>::new(
                transfer_id,
                schema::FileAckKind::Complete,
                total_len,
                0,
                schema::FileAckError::None,
            );
            if doodad
                .send_to::<A::FileAck, _>(from, &complete, send_timeout)
                .await
                .is_err()
            {
                return Err(Error::Protocol);
            }

            return Ok(RecvFileResultNoAlloc {
                transfer_id,
                total_len,
                received_len: total_len,
                sender_max_chunk_size,
                negotiated_chunk_size,
                metadata,
            });
        }

        let progress = file_ack_progress::<A>(transfer_id, next_offset);
        if doodad
            .send_to::<A::FileAck, _>(from, &progress, send_timeout)
            .await
            .is_err()
        {
            store.abort(handle).await;
            return Err(Error::Protocol);
        }
    }

    Err(Error::Protocol)
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
    Node: can_isotp_interface::IsoTpAsyncEndpoint,
    TxBuf: AsMut<[u8]>,
{
    pub async fn send_file(
        &mut self,
        to: u8,
        bytes: &[u8],
        timeout: Duration,
        config: SendConfig,
    ) -> Result<SendFileResult, thincan::Error> {
        send_file_impl(
            &self.doodad,
            to,
            &mut self.send_state,
            bytes,
            timeout,
            config,
        )
        .await
    }

    pub async fn send_file_with_id(
        &mut self,
        to: u8,
        transfer_id: u32,
        bytes: &[u8],
        timeout: Duration,
        config: SendConfig,
    ) -> Result<SendFileResult, thincan::Error> {
        send_file_with_id_impl(
            &self.doodad,
            to,
            &mut self.send_state,
            transfer_id,
            bytes,
            timeout,
            config,
        )
        .await
    }

    #[cfg(feature = "alloc")]
    pub async fn recv_file<S>(
        &self,
        from: u8,
        store: &mut S,
        send_timeout: Duration,
        receiver: ReceiverConfig,
    ) -> Result<RecvFileResult, Error<S::Error>>
    where
        S: crate::AsyncFileStore,
    {
        recv_file_impl(&self.doodad, from, store, send_timeout, receiver).await
    }

    #[cfg(feature = "heapless")]
    pub async fn recv_file_no_alloc<S, const MAX_METADATA: usize, const MAX_CHUNK: usize>(
        &self,
        from: u8,
        store: &mut S,
        send_timeout: Duration,
        receiver: ReceiverConfig,
    ) -> Result<RecvFileResultNoAlloc<MAX_METADATA>, Error<S::Error>>
    where
        S: crate::AsyncFileStore,
    {
        recv_file_no_alloc_impl::<
            A,
            _,
            _,
            _,
            _,
            _,
            MAX_TYPES,
            DEPTH,
            MAX_BODY,
            MAX_WAITERS,
            MAX_METADATA,
            MAX_CHUNK,
        >(&self.doodad, from, store, send_timeout, receiver)
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
    Node: 'a,
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
        handle: thincan::DoodadHandle<
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
        FileTransferBundleInstance::new(handle)
    }
}
