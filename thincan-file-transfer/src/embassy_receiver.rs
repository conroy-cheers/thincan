#![cfg(feature = "embassy")]

use capnp::message::ReaderOptions;
use core::cmp::min;
use core::marker::PhantomData;

use embassy_sync::blocking_mutex::raw::RawMutex;
use embassy_sync::channel::Channel;
use heapless::Vec;

use crate::{AsyncFileStore, Atlas, Error, PendingAck, ReceiverConfig, schema};

/// Inbound events produced by the synchronous `thincan` dispatch callback and consumed by the async
/// receiver task.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InboundEvent<ReplyTo, const MAX_METADATA: usize, const MAX_CHUNK: usize> {
    FileReq {
        reply_to: ReplyTo,
        transfer_id: u32,
        total_len: u32,
        sender_max_chunk_size: u32,
        metadata_len: u16,
        metadata: [u8; MAX_METADATA],
        metadata_overflow: bool,
    },
    FileChunk {
        reply_to: ReplyTo,
        transfer_id: u32,
        offset: u32,
        data_len: u16,
        data: [u8; MAX_CHUNK],
        data_overflow: bool,
    },
}

/// Ack that should be sent back to the peer at `reply_to`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AckToSend<ReplyTo> {
    pub reply_to: ReplyTo,
    pub ack: PendingAck,
}

pub type InboundQueue<
    M,
    ReplyTo,
    const MAX_METADATA: usize,
    const MAX_CHUNK: usize,
    const DEPTH: usize,
> = Channel<M, InboundEvent<ReplyTo, MAX_METADATA, MAX_CHUNK>, DEPTH>;

pub type OutboundAckQueue<M, ReplyTo, const DEPTH: usize> = Channel<M, AckToSend<ReplyTo>, DEPTH>;

/// Synchronous ingest helper for `thincan` dispatch callbacks.
///
/// This exists so your receive loop can enqueue file-transfer messages without needing access to
/// the async store or receiver worker task.
#[derive(Clone, Copy)]
pub struct ReceiverIngress<
    'a,
    A,
    M,
    ReplyTo,
    const MAX_METADATA: usize,
    const MAX_CHUNK: usize,
    const DEPTH: usize,
> where
    A: Atlas,
    M: RawMutex,
    ReplyTo: Copy,
{
    inbound: &'a InboundQueue<M, ReplyTo, MAX_METADATA, MAX_CHUNK, DEPTH>,
    _atlas: PhantomData<A>,
}

impl<'a, A, M, ReplyTo, const MAX_METADATA: usize, const MAX_CHUNK: usize, const DEPTH: usize>
    ReceiverIngress<'a, A, M, ReplyTo, MAX_METADATA, MAX_CHUNK, DEPTH>
where
    A: Atlas,
    M: RawMutex,
    ReplyTo: Copy,
{
    pub fn new(inbound: &'a InboundQueue<M, ReplyTo, MAX_METADATA, MAX_CHUNK, DEPTH>) -> Self {
        Self {
            inbound,
            _atlas: PhantomData,
        }
    }

    /// Attempt to ingest a `thincan`-decoded message (`RawMessage`) with reply-to metadata.
    ///
    /// Returns `Ok(true)` if the message was handled (enqueued), `Ok(false)` if not a file-transfer
    /// message. Returns `Err(())` if the inbound queue is full.
    pub fn try_ingest_thincan_raw<'msg>(
        &self,
        reply_to: ReplyTo,
        raw: thincan::RawMessage<'msg>,
    ) -> Result<bool, ()> {
        if raw.id == <A::FileReq as thincan::Message>::ID {
            let msg = thincan::CapnpTyped::<schema::file_req::Owned>::new(raw.body);
            let mut metadata = [0u8; MAX_METADATA];

            let parsed = msg.with_root(ReaderOptions::default(), |root| {
                let transfer_id = root.get_transfer_id();
                let total_len = root.get_total_len();
                let sender_max_chunk_size = root.get_sender_max_chunk_size();
                let meta = root.get_file_metadata().unwrap_or(&[]);
                let overflow = meta.len() > MAX_METADATA;
                let copy_len = min(meta.len(), MAX_METADATA);
                metadata[..copy_len].copy_from_slice(&meta[..copy_len]);
                (
                    transfer_id,
                    total_len,
                    sender_max_chunk_size,
                    meta.len(),
                    overflow,
                )
            });

            let (transfer_id, total_len, sender_max_chunk_size, meta_len, overflow) = match parsed {
                Ok(v) => v,
                Err(_) => {
                    // Treat parse failures as invalid requests; enqueue a synthetic reject.
                    (0, 0, 0, 0, true)
                }
            };

            let ev = InboundEvent::FileReq {
                reply_to,
                transfer_id,
                total_len,
                sender_max_chunk_size,
                metadata_len: (min(meta_len, u16::MAX as usize) as u16),
                metadata,
                metadata_overflow: overflow,
            };

            self.inbound.try_send(ev).map_err(|_| ())?;
            return Ok(true);
        }

        if raw.id == <A::FileChunk as thincan::Message>::ID {
            let msg = thincan::CapnpTyped::<schema::file_chunk::Owned>::new(raw.body);
            let mut data = [0u8; MAX_CHUNK];

            let parsed = msg.0.with_reader(ReaderOptions::default(), |reader| {
                let typed =
                    capnp::message::TypedReader::<_, schema::file_chunk::Owned>::new(reader);
                let root = typed.get().map_err(|_| ())?;

                let transfer_id = root.get_transfer_id();
                let offset = root.get_offset();
                let d = root.get_data().unwrap_or(&[]);
                let overflow = d.len() > MAX_CHUNK;
                let copy_len = min(d.len(), MAX_CHUNK);
                data[..copy_len].copy_from_slice(&d[..copy_len]);
                Ok::<_, ()>((transfer_id, offset, d.len(), overflow))
            });

            let (transfer_id, offset, data_len, overflow) = parsed.unwrap_or((0, 0, 0, true));

            let ev = InboundEvent::FileChunk {
                reply_to,
                transfer_id,
                offset,
                data_len: (min(data_len, u16::MAX as usize) as u16),
                data,
                data_overflow: overflow,
            };

            self.inbound.try_send(ev).map_err(|_| ())?;
            return Ok(true);
        }

        Ok(false)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReceiverNoAllocConfig {
    pub receiver: ReceiverConfig,
}

impl Default for ReceiverNoAllocConfig {
    fn default() -> Self {
        Self {
            receiver: ReceiverConfig::default(),
        }
    }
}

struct InProgress<S, ReplyTo, const MAX_METADATA: usize, const MAX_RANGES: usize>
where
    S: AsyncFileStore,
{
    reply_to: ReplyTo,
    transfer_id: u32,
    total_len: u32,
    max_chunk_size: u32,
    file_metadata: Vec<u8, MAX_METADATA>,
    handle: S::WriteHandle,
    received: Vec<(u32, u32), MAX_RANGES>,
}

/// Async, no-alloc receiver for file transfers.
///
/// This type is intended for embassy-based firmware where storage I/O must be async. It decouples
/// synchronous ISO-TP receive/dispatch from async store writes:
/// - Inbound `FileReq`/`FileChunk` messages are parsed in the dispatch callback and *enqueued* via
///   `try_ingest_thincan_raw`.
/// - The async task calls [`process_one`] to drain the queue, perform store writes, and emit acks
///   to the `acks` queue. Acks are only emitted after the corresponding store operation completes,
///   naturally applying backpressure to an ack-driven sender.
pub struct ReceiverNoAlloc<
    'a,
    A,
    S,
    M,
    ReplyTo,
    const MAX_METADATA: usize,
    const MAX_RANGES: usize,
    const MAX_CHUNK: usize,
    const IN_DEPTH: usize,
    const ACK_DEPTH: usize,
> where
    A: Atlas,
    S: AsyncFileStore,
    M: RawMutex,
    ReplyTo: Copy + PartialEq,
{
    inbound: &'a InboundQueue<M, ReplyTo, MAX_METADATA, MAX_CHUNK, IN_DEPTH>,
    acks: &'a OutboundAckQueue<M, ReplyTo, ACK_DEPTH>,
    store: S,
    in_progress: Option<InProgress<S, ReplyTo, MAX_METADATA, MAX_RANGES>>,
    config: ReceiverNoAllocConfig,
    _atlas: PhantomData<A>,
}

impl<
    'a,
    A,
    S,
    M,
    ReplyTo,
    const MAX_METADATA: usize,
    const MAX_RANGES: usize,
    const MAX_CHUNK: usize,
    const IN_DEPTH: usize,
    const ACK_DEPTH: usize,
> ReceiverNoAlloc<'a, A, S, M, ReplyTo, MAX_METADATA, MAX_RANGES, MAX_CHUNK, IN_DEPTH, ACK_DEPTH>
where
    A: Atlas,
    S: AsyncFileStore,
    M: RawMutex,
    ReplyTo: Copy + PartialEq,
{
    pub fn new(
        store: S,
        inbound: &'a InboundQueue<M, ReplyTo, MAX_METADATA, MAX_CHUNK, IN_DEPTH>,
        acks: &'a OutboundAckQueue<M, ReplyTo, ACK_DEPTH>,
    ) -> Self {
        Self {
            inbound,
            acks,
            store,
            in_progress: None,
            config: ReceiverNoAllocConfig::default(),
            _atlas: PhantomData,
        }
    }

    pub fn set_config(&mut self, config: ReceiverNoAllocConfig) {
        self.config = config;
    }

    pub fn config(&self) -> ReceiverNoAllocConfig {
        self.config
    }

    /// Access metadata bytes for the currently in-progress transfer (if any).
    pub fn in_progress_metadata(&self) -> Option<&[u8]> {
        self.in_progress
            .as_ref()
            .map(|ip| ip.file_metadata.as_slice())
    }

    /// Create a synchronous ingest helper suitable for `thincan` dispatch callbacks.
    pub fn ingress(&self) -> ReceiverIngress<'a, A, M, ReplyTo, MAX_METADATA, MAX_CHUNK, IN_DEPTH> {
        ReceiverIngress::new(self.inbound)
    }

    /// Process exactly one inbound event, performing async store I/O and emitting any resulting ack.
    pub async fn process_one(&mut self) -> Result<(), Error<S::Error>> {
        let ev = self.inbound.receive().await;
        match ev {
            InboundEvent::FileReq {
                reply_to,
                transfer_id,
                total_len,
                sender_max_chunk_size,
                metadata_len,
                metadata,
                metadata_overflow,
            } => {
                if metadata_overflow {
                    let ack = PendingAck {
                        transfer_id,
                        kind: schema::FileAckKind::Reject,
                        next_offset: 0,
                        chunk_size: 0,
                        error: schema::FileAckError::InvalidRequest,
                    };
                    self.acks.send(AckToSend { reply_to, ack }).await;
                    return Ok(());
                }

                if let Some(in_progress) = self.in_progress.take() {
                    self.store.abort(in_progress.handle).await;
                }

                let receiver_max_cfg = self.config.receiver.max_chunk_size;
                let receiver_max = if receiver_max_cfg == 0 {
                    MAX_CHUNK as u32
                } else {
                    receiver_max_cfg
                };

                if receiver_max > (MAX_CHUNK as u32) {
                    let ack = PendingAck {
                        transfer_id,
                        kind: schema::FileAckKind::Reject,
                        next_offset: 0,
                        chunk_size: 0,
                        error: schema::FileAckError::ChunkSizeTooLarge,
                    };
                    self.acks.send(AckToSend { reply_to, ack }).await;
                    return Ok(());
                }

                let negotiated_chunk_size = match (receiver_max, sender_max_chunk_size) {
                    (0, 0) => 0,
                    (0, s) => s,
                    (r, 0) => r,
                    (r, s) => r.min(s),
                };

                if negotiated_chunk_size == 0 {
                    let ack = PendingAck {
                        transfer_id,
                        kind: schema::FileAckKind::Reject,
                        next_offset: 0,
                        chunk_size: 0,
                        error: schema::FileAckError::ChunkSizeTooSmall,
                    };
                    self.acks.send(AckToSend { reply_to, ack }).await;
                    return Ok(());
                }

                if negotiated_chunk_size > receiver_max {
                    let ack = PendingAck {
                        transfer_id,
                        kind: schema::FileAckKind::Reject,
                        next_offset: 0,
                        chunk_size: 0,
                        error: schema::FileAckError::ChunkSizeTooLarge,
                    };
                    self.acks.send(AckToSend { reply_to, ack }).await;
                    return Ok(());
                }

                let mut meta_vec: Vec<u8, MAX_METADATA> = Vec::new();
                let meta_len = (metadata_len as usize).min(MAX_METADATA);
                // Since `metadata_overflow` is false, metadata fits in `MAX_METADATA`.
                let _ = meta_vec.extend_from_slice(&metadata[..meta_len]);

                let handle = self
                    .store
                    .begin_write(transfer_id, total_len)
                    .await
                    .map_err(Error::Store)?;

                if total_len == 0 {
                    self.store.commit(handle).await.map_err(Error::Store)?;
                } else {
                    self.in_progress = Some(InProgress {
                        reply_to,
                        transfer_id,
                        total_len,
                        max_chunk_size: negotiated_chunk_size,
                        file_metadata: meta_vec,
                        handle,
                        received: Vec::new(),
                    });
                }

                let ack = PendingAck {
                    transfer_id,
                    kind: schema::FileAckKind::Accept,
                    next_offset: 0,
                    chunk_size: negotiated_chunk_size,
                    error: schema::FileAckError::None,
                };
                self.acks.send(AckToSend { reply_to, ack }).await;
                Ok(())
            }
            InboundEvent::FileChunk {
                reply_to,
                transfer_id,
                offset,
                data_len,
                data,
                data_overflow,
            } => {
                let Some(in_progress) = self.in_progress.as_mut() else {
                    let ack = PendingAck {
                        transfer_id,
                        kind: schema::FileAckKind::Abort,
                        next_offset: 0,
                        chunk_size: 0,
                        error: schema::FileAckError::InvalidRequest,
                    };
                    self.acks.send(AckToSend { reply_to, ack }).await;
                    return Ok(());
                };

                if in_progress.reply_to != reply_to || in_progress.transfer_id != transfer_id {
                    let in_progress = self.in_progress.take().unwrap();
                    self.store.abort(in_progress.handle).await;
                    let ack = PendingAck {
                        transfer_id,
                        kind: schema::FileAckKind::Abort,
                        next_offset: 0,
                        chunk_size: 0,
                        error: schema::FileAckError::InvalidRequest,
                    };
                    self.acks.send(AckToSend { reply_to, ack }).await;
                    return Ok(());
                }

                if data_overflow {
                    let in_progress = self.in_progress.take().unwrap();
                    self.store.abort(in_progress.handle).await;
                    let ack = PendingAck {
                        transfer_id,
                        kind: schema::FileAckKind::Abort,
                        next_offset: 0,
                        chunk_size: 0,
                        error: schema::FileAckError::ChunkSizeTooLarge,
                    };
                    self.acks.send(AckToSend { reply_to, ack }).await;
                    return Ok(());
                }

                let data_len = (data_len as usize).min(MAX_CHUNK);
                if in_progress.max_chunk_size != 0
                    && data_len > (in_progress.max_chunk_size as usize)
                {
                    let in_progress = self.in_progress.take().unwrap();
                    self.store.abort(in_progress.handle).await;
                    let ack = PendingAck {
                        transfer_id,
                        kind: schema::FileAckKind::Abort,
                        next_offset: 0,
                        chunk_size: 0,
                        error: schema::FileAckError::ChunkSizeTooLarge,
                    };
                    self.acks.send(AckToSend { reply_to, ack }).await;
                    return Ok(());
                }

                let remaining = in_progress.total_len.saturating_sub(offset);
                if remaining == 0 {
                    return Ok(());
                }

                let chunk_len = (data_len as u32).min(remaining) as usize;
                self.store
                    .write_at(&mut in_progress.handle, offset, &data[..chunk_len])
                    .await
                    .map_err(Error::Store)?;

                if insert_received_range::<MAX_RANGES>(
                    &mut in_progress.received,
                    offset,
                    offset + (chunk_len as u32),
                )
                .is_err()
                {
                    let in_progress = self.in_progress.take().unwrap();
                    self.store.abort(in_progress.handle).await;
                    let ack = PendingAck {
                        transfer_id,
                        kind: schema::FileAckKind::Abort,
                        next_offset: 0,
                        chunk_size: 0,
                        error: schema::FileAckError::Internal,
                    };
                    self.acks.send(AckToSend { reply_to, ack }).await;
                    return Ok(());
                }

                let mut next_offset = 0u32;
                if in_progress.received.first().is_some_and(|r| r.0 == 0) {
                    next_offset = in_progress.received[0].1.min(in_progress.total_len);
                }

                let mut ack = PendingAck {
                    transfer_id,
                    kind: schema::FileAckKind::Ack,
                    next_offset,
                    chunk_size: 0,
                    error: schema::FileAckError::None,
                };

                if in_progress.received.len() == 1
                    && in_progress.received[0].0 == 0
                    && in_progress.received[0].1 >= in_progress.total_len
                {
                    let in_progress = self.in_progress.take().unwrap();
                    self.store
                        .commit(in_progress.handle)
                        .await
                        .map_err(Error::Store)?;
                    ack = PendingAck {
                        transfer_id,
                        kind: schema::FileAckKind::Complete,
                        next_offset: in_progress.total_len,
                        chunk_size: 0,
                        error: schema::FileAckError::None,
                    };
                }

                self.acks.send(AckToSend { reply_to, ack }).await;
                Ok(())
            }
        }
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
