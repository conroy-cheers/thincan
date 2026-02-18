#![cfg(feature = "embassy")]

use core::marker::PhantomData;
use core::time::Duration;

use embassy_sync::blocking_mutex::raw::RawMutex;
use embassy_sync::channel::Channel;
use embassy_sync::mutex::Mutex;
use embassy_time::{Instant, Timer, with_timeout};
use heapless::Deque;

use crate::{
    Atlas, FileTransferTx, file_chunk_max_encoded_len, file_offer_max_encoded_len, schema,
};

/// Parsed `FileAck` fields (sender-side).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ack {
    pub transfer_id: u32,
    pub kind: schema::FileAckKind,
    pub next_offset: u32,
    pub chunk_size: u32,
    pub error: schema::FileAckError,
}

/// Embassy channel that carries `FileAck` messages from dispatch handlers to senders.
pub type AckInboxQueue<M, const DEPTH: usize> = Channel<M, Ack, DEPTH>;

/// Receiver side of the ack mailbox.
pub struct AckStream<'a, M, const DEPTH: usize>
where
    M: RawMutex,
{
    queue: &'a AckInboxQueue<M, DEPTH>,
}

impl<'a, M, const DEPTH: usize> AckStream<'a, M, DEPTH>
where
    M: RawMutex,
{
    pub fn new(queue: &'a AckInboxQueue<M, DEPTH>) -> Self {
        Self { queue }
    }

    pub async fn receive(&mut self) -> Ack {
        self.queue.receive().await
    }

    pub fn try_receive(&mut self) -> Result<Ack, embassy_sync::channel::TryReceiveError> {
        self.queue.try_receive()
    }

    pub async fn wait_for_transfer(
        &mut self,
        transfer_id: u32,
        timeout: Duration,
    ) -> Result<Ack, thincan::Error> {
        let deadline = Instant::now() + embassy_duration(timeout);
        loop {
            let now = Instant::now();
            if now >= deadline {
                return Err(thincan::Error::timeout());
            }
            let remaining = deadline - now;
            let ack = match with_timeout(remaining, self.queue.receive()).await {
                Ok(ack) => ack,
                Err(_) => return Err(thincan::Error::timeout()),
            };
            if ack.transfer_id == transfer_id {
                return Ok(ack);
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SendResult {
    pub transfer_id: u32,
    pub total_len: usize,
    pub chunk_size: usize,
    pub chunks_sent: usize,
    pub retries: usize,
}

#[derive(Debug, Clone, Copy)]
pub struct SenderNoAllocConfig {
    pub send_timeout: Duration,
    pub ack_timeout: Duration,
    pub max_retries: usize,
}

impl Default for SenderNoAllocConfig {
    fn default() -> Self {
        Self {
            send_timeout: Duration::from_millis(2000),
            ack_timeout: Duration::from_millis(1500),
            max_retries: 5,
        }
    }
}

/// No-alloc async sender with ack-driven throttling (Embassy).
pub struct SenderNoAlloc<'a, A, const MAX_CHUNK: usize, const WINDOW_CHUNKS: usize> {
    next_transfer_id: u32,
    sender_max_chunk_size: usize,
    config: SenderNoAllocConfig,
    scratch: thincan::capnp::CapnpScratchRef<'a>,
    _atlas: PhantomData<A>,
}

impl<'a, A, const MAX_CHUNK: usize, const WINDOW_CHUNKS: usize>
    SenderNoAlloc<'a, A, MAX_CHUNK, WINDOW_CHUNKS>
{
    pub fn new(scratch: thincan::capnp::CapnpScratchRef<'a>) -> Result<Self, thincan::Error> {
        let max_body = {
            let offer = file_offer_max_encoded_len(0);
            let chunk = file_chunk_max_encoded_len(MAX_CHUNK);
            if offer > chunk { offer } else { chunk }
        };
        let min_scratch = crate::capnp_scratch_words_for_bytes(max_body) * 8;
        if scratch.capacity() < min_scratch {
            return Err(thincan::Error {
                kind: thincan::ErrorKind::BufferTooSmall {
                    needed: min_scratch,
                    got: scratch.capacity(),
                },
            });
        }
        Ok(Self {
            next_transfer_id: 1,
            sender_max_chunk_size: MAX_CHUNK,
            config: SenderNoAllocConfig::default(),
            scratch,
            _atlas: PhantomData,
        })
    }

    pub fn config(&self) -> SenderNoAllocConfig {
        self.config
    }

    pub fn set_config(&mut self, config: SenderNoAllocConfig) {
        self.config = config;
    }

    pub fn sender_max_chunk_size(&self) -> usize {
        self.sender_max_chunk_size
    }

    pub fn set_sender_max_chunk_size(&mut self, size: usize) -> Result<(), thincan::Error> {
        if size == 0 || size > MAX_CHUNK {
            return Err(thincan::Error {
                kind: thincan::ErrorKind::Other,
            });
        }
        self.sender_max_chunk_size = size;
        Ok(())
    }

    pub fn next_transfer_id(&self) -> u32 {
        self.next_transfer_id
    }

    fn alloc_transfer_id(&mut self) -> u32 {
        let id = self.next_transfer_id;
        self.next_transfer_id = self.next_transfer_id.wrapping_add(1);
        id
    }
}

pub trait AsyncChunkSource {
    type Error;
    async fn read_chunk(&mut self, buf: &mut [u8]) -> Result<usize, Self::Error>;
}

impl<'a, A, const MAX_CHUNK: usize, const WINDOW_CHUNKS: usize>
    SenderNoAlloc<'a, A, MAX_CHUNK, WINDOW_CHUNKS>
where
    A: Atlas,
{
    pub async fn send_bytes_with_acks<ReplyTo, Tx, M, const DEPTH: usize>(
        &mut self,
        tx: &mut Tx,
        acks: &mut AckStream<'_, M, DEPTH>,
        to: ReplyTo,
        bytes: &[u8],
    ) -> Result<SendResult, thincan::Error>
    where
        Tx: FileTransferTx<ReplyTo>,
        M: RawMutex,
        ReplyTo: Copy,
    {
        let transfer_id = self.alloc_transfer_id();
        self.send_bytes_with_id_and_acks(tx, acks, to, transfer_id, bytes)
            .await
    }

    pub async fn send_bytes_with_id_and_acks<ReplyTo, Tx, M, const DEPTH: usize>(
        &mut self,
        tx: &mut Tx,
        acks: &mut AckStream<'_, M, DEPTH>,
        to: ReplyTo,
        transfer_id: u32,
        bytes: &[u8],
    ) -> Result<SendResult, thincan::Error>
    where
        Tx: FileTransferTx<ReplyTo>,
        M: RawMutex,
        ReplyTo: Copy,
    {
        if transfer_id >= self.next_transfer_id {
            self.next_transfer_id = transfer_id.wrapping_add(1);
        }

        let total_len = bytes.len();
        let total_len_u32 = u32::try_from(total_len).map_err(|_| thincan::Error {
            kind: thincan::ErrorKind::Other,
        })?;
        let max_chunk_u32 =
            u32::try_from(self.sender_max_chunk_size).map_err(|_| thincan::Error {
                kind: thincan::ErrorKind::Other,
            })?;

        let value = self
            .scratch
            .builder_value::<A::FileReq, crate::schema::file_req::Owned, _>(|root| {
                root.set_transfer_id(transfer_id);
                root.set_total_len(total_len_u32);
                root.set_sender_max_chunk_size(max_chunk_u32);
                root.set_file_metadata(&[]);
                Ok(())
            });
        tx.send_encoded::<A::FileReq, _>(to, &value, self.config.send_timeout)
            .await?;

        let accept = self.wait_for_accept_with_acks(acks, transfer_id).await?;

        let chunk_size = usize::try_from(accept.chunk_size).map_err(|_| thincan::Error {
            kind: thincan::ErrorKind::Other,
        })?;
        if chunk_size == 0 || chunk_size > self.sender_max_chunk_size {
            return Err(thincan::Error {
                kind: thincan::ErrorKind::Other,
            });
        }

        let mut chunks_sent = 0usize;
        let mut retries = 0usize;

        let mut last_acked = 0u32;
        let mut next_to_send = 0u32;
        let mut in_flight: Deque<u32, WINDOW_CHUNKS> = Deque::new();
        let mut last_progress_at = Instant::now();

        while (last_acked as usize) < total_len {
            while in_flight.len() < WINDOW_CHUNKS && (next_to_send as usize) < total_len {
                let offset = next_to_send as usize;
                let end = (offset + chunk_size).min(total_len);
                let data = &bytes[offset..end];
                let value = self
                    .scratch
                    .builder_value::<A::FileChunk, crate::schema::file_chunk::Owned, _>(|root| {
                        root.set_transfer_id(transfer_id);
                        root.set_offset(next_to_send);
                        root.set_data(data);
                        Ok(())
                    });
                tx.send_encoded::<A::FileChunk, _>(to, &value, self.config.send_timeout)
                    .await?;
                let _ = in_flight.push_back(next_to_send);
                next_to_send = next_to_send
                    .checked_add(u32::try_from(data.len()).map_err(|_| thincan::Error {
                        kind: thincan::ErrorKind::Other,
                    })?)
                    .ok_or(thincan::Error {
                        kind: thincan::ErrorKind::Other,
                    })?;
                chunks_sent += 1;
            }

            let mut advanced = false;
            while let Ok(ack) = acks.try_receive() {
                if ack.transfer_id != transfer_id {
                    continue;
                }
                match ack.kind {
                    schema::FileAckKind::Ack | schema::FileAckKind::Complete => {}
                    schema::FileAckKind::Reject | schema::FileAckKind::Abort => {
                        return Err(thincan::Error {
                            kind: thincan::ErrorKind::Other,
                        });
                    }
                    schema::FileAckKind::Accept => continue,
                }

                let next = ack.next_offset;
                if next < last_acked || (next as usize) > total_len || next > next_to_send {
                    return Err(thincan::Error {
                        kind: thincan::ErrorKind::Other,
                    });
                }

                last_acked = next;
                advanced = true;
                while in_flight.front().is_some_and(|&off| off < last_acked) {
                    let _ = in_flight.pop_front();
                }

                if ack.kind == schema::FileAckKind::Complete {
                    if (last_acked as usize) != total_len {
                        return Err(thincan::Error {
                            kind: thincan::ErrorKind::Other,
                        });
                    }
                    return Ok(SendResult {
                        transfer_id,
                        total_len,
                        chunk_size,
                        chunks_sent,
                        retries,
                    });
                }
            }

            if advanced {
                last_progress_at = Instant::now();
            } else if Instant::now() - last_progress_at >= embassy_duration(self.config.ack_timeout)
            {
                retries += 1;
                if retries > self.config.max_retries {
                    return Err(thincan::Error::timeout());
                }
                next_to_send = last_acked;
                in_flight.clear();
                last_progress_at = Instant::now();
            } else {
                Timer::after_millis(0).await;
            }
        }

        Ok(SendResult {
            transfer_id,
            total_len,
            chunk_size,
            chunks_sent,
            retries,
        })
    }

    pub async fn send_bytes<Node, Router, TxBuf, M, const DEPTH: usize, Handlers>(
        &mut self,
        iface: &mut thincan::Interface<Node, Router, TxBuf>,
        handlers: &mut Handlers,
        rx_buf: &mut [u8],
        acks: &mut AckStream<'_, M, DEPTH>,
        to: <Node as can_isotp_interface::IsoTpAsyncEndpointMeta>::ReplyTo,
        bytes: &[u8],
    ) -> Result<SendResult, thincan::Error>
    where
        Node: can_isotp_interface::IsoTpAsyncEndpointMeta
            + can_isotp_interface::IsoTpAsyncEndpointMetaRecvInto<
                ReplyTo = <Node as can_isotp_interface::IsoTpAsyncEndpointMeta>::ReplyTo,
            >,
        Router: thincan::RouterDispatch<
                Handlers,
                <Node as can_isotp_interface::IsoTpAsyncEndpointMeta>::ReplyTo,
            >,
        TxBuf: AsMut<[u8]>,
        M: RawMutex,
    {
        let transfer_id = self.alloc_transfer_id();
        self.send_bytes_with_id(iface, handlers, rx_buf, acks, to, transfer_id, bytes)
            .await
    }

    pub async fn send_bytes_with_id<Node, Router, TxBuf, M, const DEPTH: usize, Handlers>(
        &mut self,
        iface: &mut thincan::Interface<Node, Router, TxBuf>,
        handlers: &mut Handlers,
        rx_buf: &mut [u8],
        acks: &mut AckStream<'_, M, DEPTH>,
        to: <Node as can_isotp_interface::IsoTpAsyncEndpointMeta>::ReplyTo,
        transfer_id: u32,
        bytes: &[u8],
    ) -> Result<SendResult, thincan::Error>
    where
        Node: can_isotp_interface::IsoTpAsyncEndpointMeta
            + can_isotp_interface::IsoTpAsyncEndpointMetaRecvInto<
                ReplyTo = <Node as can_isotp_interface::IsoTpAsyncEndpointMeta>::ReplyTo,
            >,
        Router: thincan::RouterDispatch<
                Handlers,
                <Node as can_isotp_interface::IsoTpAsyncEndpointMeta>::ReplyTo,
            >,
        TxBuf: AsMut<[u8]>,
        M: RawMutex,
    {
        if transfer_id >= self.next_transfer_id {
            self.next_transfer_id = transfer_id.wrapping_add(1);
        }

        let total_len = bytes.len();
        let total_len_u32 = u32::try_from(total_len).map_err(|_| thincan::Error {
            kind: thincan::ErrorKind::Other,
        })?;
        let max_chunk_u32 =
            u32::try_from(self.sender_max_chunk_size).map_err(|_| thincan::Error {
                kind: thincan::ErrorKind::Other,
            })?;

        let value = self
            .scratch
            .builder_value::<A::FileReq, crate::schema::file_req::Owned, _>(|root| {
                root.set_transfer_id(transfer_id);
                root.set_total_len(total_len_u32);
                root.set_sender_max_chunk_size(max_chunk_u32);
                root.set_file_metadata(&[]);
                Ok(())
            });
        iface
            .send_encoded_to_async(to, &value, self.config.send_timeout)
            .await?;

        let accept = self
            .wait_for_accept(iface, handlers, rx_buf, acks, transfer_id)
            .await?;

        let chunk_size = usize::try_from(accept.chunk_size).map_err(|_| thincan::Error {
            kind: thincan::ErrorKind::Other,
        })?;
        if chunk_size == 0 || chunk_size > self.sender_max_chunk_size {
            return Err(thincan::Error {
                kind: thincan::ErrorKind::Other,
            });
        }

        let mut chunks_sent = 0usize;
        let mut retries = 0usize;

        let mut last_acked = 0u32;
        let mut next_to_send = 0u32;
        let mut in_flight: Deque<u32, WINDOW_CHUNKS> = Deque::new();
        let mut last_progress_at = Instant::now();

        while (last_acked as usize) < total_len {
            while in_flight.len() < WINDOW_CHUNKS && (next_to_send as usize) < total_len {
                let offset = next_to_send as usize;
                let end = (offset + chunk_size).min(total_len);
                let data = &bytes[offset..end];
                let value = self
                    .scratch
                    .builder_value::<A::FileChunk, crate::schema::file_chunk::Owned, _>(|root| {
                        root.set_transfer_id(transfer_id);
                        root.set_offset(next_to_send);
                        root.set_data(data);
                        Ok(())
                    });
                iface
                    .send_encoded_to_async(to, &value, self.config.send_timeout)
                    .await?;
                let _ = in_flight.push_back(next_to_send);
                next_to_send = next_to_send
                    .checked_add(u32::try_from(data.len()).map_err(|_| thincan::Error {
                        kind: thincan::ErrorKind::Other,
                    })?)
                    .ok_or(thincan::Error {
                        kind: thincan::ErrorKind::Other,
                    })?;
                chunks_sent += 1;
            }

            let _ = pump_rx(iface, handlers, rx_buf, self.config.ack_timeout).await;

            let mut advanced = false;
            while let Ok(ack) = acks.try_receive() {
                if ack.transfer_id != transfer_id {
                    continue;
                }
                match ack.kind {
                    schema::FileAckKind::Ack | schema::FileAckKind::Complete => {}
                    schema::FileAckKind::Reject | schema::FileAckKind::Abort => {
                        return Err(thincan::Error {
                            kind: thincan::ErrorKind::Other,
                        });
                    }
                    schema::FileAckKind::Accept => continue,
                }

                let next = ack.next_offset;
                if next < last_acked || (next as usize) > total_len || next > next_to_send {
                    return Err(thincan::Error {
                        kind: thincan::ErrorKind::Other,
                    });
                }

                last_acked = next;
                advanced = true;
                while in_flight.front().is_some_and(|&off| off < last_acked) {
                    let _ = in_flight.pop_front();
                }

                if ack.kind == schema::FileAckKind::Complete {
                    if (last_acked as usize) != total_len {
                        return Err(thincan::Error {
                            kind: thincan::ErrorKind::Other,
                        });
                    }
                    return Ok(SendResult {
                        transfer_id,
                        total_len,
                        chunk_size,
                        chunks_sent,
                        retries,
                    });
                }
            }

            if advanced {
                last_progress_at = Instant::now();
            } else if Instant::now() - last_progress_at >= embassy_duration(self.config.ack_timeout)
            {
                retries += 1;
                if retries > self.config.max_retries {
                    return Err(thincan::Error::timeout());
                }
                next_to_send = last_acked;
                in_flight.clear();
                last_progress_at = Instant::now();
            } else {
                Timer::after_millis(0).await;
            }
        }

        Ok(SendResult {
            transfer_id,
            total_len,
            chunk_size,
            chunks_sent,
            retries,
        })
    }

    pub async fn send_from<Node, Router, TxBuf, M, const DEPTH: usize, S, Handlers>(
        &mut self,
        iface: &mut thincan::Interface<Node, Router, TxBuf>,
        handlers: &mut Handlers,
        rx_buf: &mut [u8],
        acks: &mut AckStream<'_, M, DEPTH>,
        to: <Node as can_isotp_interface::IsoTpAsyncEndpointMeta>::ReplyTo,
        transfer_id: u32,
        total_len: usize,
        source: &mut S,
    ) -> Result<SendResult, thincan::Error>
    where
        Node: can_isotp_interface::IsoTpAsyncEndpointMeta
            + can_isotp_interface::IsoTpAsyncEndpointMetaRecvInto<
                ReplyTo = <Node as can_isotp_interface::IsoTpAsyncEndpointMeta>::ReplyTo,
            >,
        Router: thincan::RouterDispatch<
                Handlers,
                <Node as can_isotp_interface::IsoTpAsyncEndpointMeta>::ReplyTo,
            >,
        TxBuf: AsMut<[u8]>,
        M: RawMutex,
        S: AsyncChunkSource,
    {
        if transfer_id >= self.next_transfer_id {
            self.next_transfer_id = transfer_id.wrapping_add(1);
        }

        let total_len_u32 = u32::try_from(total_len).map_err(|_| thincan::Error {
            kind: thincan::ErrorKind::Other,
        })?;
        let max_chunk_u32 =
            u32::try_from(self.sender_max_chunk_size).map_err(|_| thincan::Error {
                kind: thincan::ErrorKind::Other,
            })?;

        let value = self
            .scratch
            .builder_value::<A::FileReq, crate::schema::file_req::Owned, _>(|root| {
                root.set_transfer_id(transfer_id);
                root.set_total_len(total_len_u32);
                root.set_sender_max_chunk_size(max_chunk_u32);
                root.set_file_metadata(&[]);
                Ok(())
            });
        iface
            .send_encoded_to_async(to, &value, self.config.send_timeout)
            .await?;

        let accept = self
            .wait_for_accept(iface, handlers, rx_buf, acks, transfer_id)
            .await?;

        let chunk_size = usize::try_from(accept.chunk_size).map_err(|_| thincan::Error {
            kind: thincan::ErrorKind::Other,
        })?;
        if chunk_size == 0 || chunk_size > self.sender_max_chunk_size {
            return Err(thincan::Error {
                kind: thincan::ErrorKind::Other,
            });
        }

        let mut chunks_sent = 0usize;
        let mut retries = 0usize;
        let mut last_acked = 0u32;
        let mut next_to_send = 0u32;

        struct Chunk<const N: usize> {
            offset: u32,
            len: usize,
            data: [u8; N],
        }

        let mut in_flight: Deque<Chunk<MAX_CHUNK>, WINDOW_CHUNKS> = Deque::new();
        let mut last_progress_at = Instant::now();

        while (last_acked as usize) < total_len {
            while in_flight.len() < WINDOW_CHUNKS && (next_to_send as usize) < total_len {
                let mut data = [0u8; MAX_CHUNK];
                let to_read = (total_len - next_to_send as usize).min(chunk_size);
                let read =
                    source
                        .read_chunk(&mut data[..to_read])
                        .await
                        .map_err(|_| thincan::Error {
                            kind: thincan::ErrorKind::Other,
                        })?;
                if read == 0 {
                    return Err(thincan::Error {
                        kind: thincan::ErrorKind::Other,
                    });
                }
                let value = self
                    .scratch
                    .builder_value::<A::FileChunk, crate::schema::file_chunk::Owned, _>(|root| {
                        root.set_transfer_id(transfer_id);
                        root.set_offset(next_to_send);
                        root.set_data(&data[..read]);
                        Ok(())
                    });
                iface
                    .send_encoded_to_async(to, &value, self.config.send_timeout)
                    .await?;
                let _ = in_flight.push_back(Chunk {
                    offset: next_to_send,
                    len: read,
                    data,
                });
                next_to_send = next_to_send
                    .checked_add(u32::try_from(read).map_err(|_| thincan::Error {
                        kind: thincan::ErrorKind::Other,
                    })?)
                    .ok_or(thincan::Error {
                        kind: thincan::ErrorKind::Other,
                    })?;
                chunks_sent += 1;
            }

            let _ = pump_rx(iface, handlers, rx_buf, self.config.ack_timeout).await;

            let mut advanced = false;
            while let Ok(ack) = acks.try_receive() {
                if ack.transfer_id != transfer_id {
                    continue;
                }
                match ack.kind {
                    schema::FileAckKind::Ack | schema::FileAckKind::Complete => {}
                    schema::FileAckKind::Reject | schema::FileAckKind::Abort => {
                        return Err(thincan::Error {
                            kind: thincan::ErrorKind::Other,
                        });
                    }
                    schema::FileAckKind::Accept => continue,
                }

                let next = ack.next_offset;
                if next < last_acked || (next as usize) > total_len || next > next_to_send {
                    return Err(thincan::Error {
                        kind: thincan::ErrorKind::Other,
                    });
                }

                last_acked = next;
                advanced = true;
                while in_flight.front().is_some_and(|c| c.offset < last_acked) {
                    let _ = in_flight.pop_front();
                }

                if ack.kind == schema::FileAckKind::Complete {
                    if (last_acked as usize) != total_len {
                        return Err(thincan::Error {
                            kind: thincan::ErrorKind::Other,
                        });
                    }
                    return Ok(SendResult {
                        transfer_id,
                        total_len,
                        chunk_size,
                        chunks_sent,
                        retries,
                    });
                }
            }

            if advanced {
                last_progress_at = Instant::now();
            } else if Instant::now() - last_progress_at >= embassy_duration(self.config.ack_timeout)
            {
                retries += 1;
                if retries > self.config.max_retries {
                    return Err(thincan::Error::timeout());
                }
                for chunk in in_flight.iter() {
                    let value = self
                        .scratch
                        .builder_value::<A::FileChunk, crate::schema::file_chunk::Owned, _>(
                            |root| {
                                root.set_transfer_id(transfer_id);
                                root.set_offset(chunk.offset);
                                root.set_data(&chunk.data[..chunk.len]);
                                Ok(())
                            },
                        );
                    iface
                        .send_encoded_to_async(to, &value, self.config.send_timeout)
                        .await?;
                }
                last_progress_at = Instant::now();
            } else {
                Timer::after_millis(0).await;
            }
        }

        Ok(SendResult {
            transfer_id,
            total_len,
            chunk_size,
            chunks_sent,
            retries,
        })
    }

    pub async fn send_from_with_acks<ReplyTo, Tx, M, const DEPTH: usize, S>(
        &mut self,
        tx: &mut Tx,
        acks: &mut AckStream<'_, M, DEPTH>,
        to: ReplyTo,
        transfer_id: u32,
        total_len: usize,
        source: &mut S,
    ) -> Result<SendResult, thincan::Error>
    where
        Tx: FileTransferTx<ReplyTo>,
        M: RawMutex,
        S: AsyncChunkSource,
        ReplyTo: Copy,
    {
        if transfer_id >= self.next_transfer_id {
            self.next_transfer_id = transfer_id.wrapping_add(1);
        }

        let total_len_u32 = u32::try_from(total_len).map_err(|_| thincan::Error {
            kind: thincan::ErrorKind::Other,
        })?;
        let max_chunk_u32 =
            u32::try_from(self.sender_max_chunk_size).map_err(|_| thincan::Error {
                kind: thincan::ErrorKind::Other,
            })?;

        let value = self
            .scratch
            .builder_value::<A::FileReq, crate::schema::file_req::Owned, _>(|root| {
                root.set_transfer_id(transfer_id);
                root.set_total_len(total_len_u32);
                root.set_sender_max_chunk_size(max_chunk_u32);
                root.set_file_metadata(&[]);
                Ok(())
            });
        tx.send_encoded::<A::FileReq, _>(to, &value, self.config.send_timeout)
            .await?;

        let accept = self.wait_for_accept_with_acks(acks, transfer_id).await?;

        let chunk_size = usize::try_from(accept.chunk_size).map_err(|_| thincan::Error {
            kind: thincan::ErrorKind::Other,
        })?;
        if chunk_size == 0 || chunk_size > self.sender_max_chunk_size {
            return Err(thincan::Error {
                kind: thincan::ErrorKind::Other,
            });
        }

        let mut chunks_sent = 0usize;
        let mut retries = 0usize;
        let mut last_acked = 0u32;
        let mut next_to_send = 0u32;

        struct Chunk<const N: usize> {
            offset: u32,
            len: usize,
            data: [u8; N],
        }

        let mut in_flight: Deque<Chunk<MAX_CHUNK>, WINDOW_CHUNKS> = Deque::new();
        let mut last_progress_at = Instant::now();

        while (last_acked as usize) < total_len {
            while in_flight.len() < WINDOW_CHUNKS && (next_to_send as usize) < total_len {
                let mut data = [0u8; MAX_CHUNK];
                let to_read = (total_len - next_to_send as usize).min(chunk_size);
                let read =
                    source
                        .read_chunk(&mut data[..to_read])
                        .await
                        .map_err(|_| thincan::Error {
                            kind: thincan::ErrorKind::Other,
                        })?;
                if read == 0 {
                    return Err(thincan::Error {
                        kind: thincan::ErrorKind::Other,
                    });
                }
                let value = self
                    .scratch
                    .builder_value::<A::FileChunk, crate::schema::file_chunk::Owned, _>(|root| {
                        root.set_transfer_id(transfer_id);
                        root.set_offset(next_to_send);
                        root.set_data(&data[..read]);
                        Ok(())
                    });
                tx.send_encoded::<A::FileChunk, _>(to, &value, self.config.send_timeout)
                    .await?;
                let _ = in_flight.push_back(Chunk {
                    offset: next_to_send,
                    len: read,
                    data,
                });
                next_to_send = next_to_send
                    .checked_add(u32::try_from(read).map_err(|_| thincan::Error {
                        kind: thincan::ErrorKind::Other,
                    })?)
                    .ok_or(thincan::Error {
                        kind: thincan::ErrorKind::Other,
                    })?;
                chunks_sent += 1;
            }

            let mut advanced = false;
            while let Ok(ack) = acks.try_receive() {
                if ack.transfer_id != transfer_id {
                    continue;
                }
                match ack.kind {
                    schema::FileAckKind::Ack | schema::FileAckKind::Complete => {}
                    schema::FileAckKind::Reject | schema::FileAckKind::Abort => {
                        return Err(thincan::Error {
                            kind: thincan::ErrorKind::Other,
                        });
                    }
                    schema::FileAckKind::Accept => continue,
                }

                let next = ack.next_offset;
                if next < last_acked || (next as usize) > total_len || next > next_to_send {
                    return Err(thincan::Error {
                        kind: thincan::ErrorKind::Other,
                    });
                }

                last_acked = next;
                advanced = true;
                while in_flight.front().is_some_and(|c| c.offset < last_acked) {
                    let _ = in_flight.pop_front();
                }

                if ack.kind == schema::FileAckKind::Complete {
                    if (last_acked as usize) != total_len {
                        return Err(thincan::Error {
                            kind: thincan::ErrorKind::Other,
                        });
                    }
                    return Ok(SendResult {
                        transfer_id,
                        total_len,
                        chunk_size,
                        chunks_sent,
                        retries,
                    });
                }
            }

            if advanced {
                last_progress_at = Instant::now();
            } else if Instant::now() - last_progress_at >= embassy_duration(self.config.ack_timeout)
            {
                retries += 1;
                if retries > self.config.max_retries {
                    return Err(thincan::Error::timeout());
                }
                for chunk in in_flight.iter() {
                    let value = self
                        .scratch
                        .builder_value::<A::FileChunk, crate::schema::file_chunk::Owned, _>(
                            |root| {
                                root.set_transfer_id(transfer_id);
                                root.set_offset(chunk.offset);
                                root.set_data(&chunk.data[..chunk.len]);
                                Ok(())
                            },
                        );
                    tx.send_encoded::<A::FileChunk, _>(to, &value, self.config.send_timeout)
                        .await?;
                }
                last_progress_at = Instant::now();
            } else {
                Timer::after_millis(0).await;
            }
        }

        Ok(SendResult {
            transfer_id,
            total_len,
            chunk_size,
            chunks_sent,
            retries,
        })
    }

    async fn wait_for_accept<Node, Router, TxBuf, M, const DEPTH: usize, Handlers>(
        &mut self,
        iface: &mut thincan::Interface<Node, Router, TxBuf>,
        handlers: &mut Handlers,
        rx_buf: &mut [u8],
        acks: &mut AckStream<'_, M, DEPTH>,
        transfer_id: u32,
    ) -> Result<Ack, thincan::Error>
    where
        Node: can_isotp_interface::IsoTpAsyncEndpointMeta
            + can_isotp_interface::IsoTpAsyncEndpointMetaRecvInto<
                ReplyTo = <Node as can_isotp_interface::IsoTpAsyncEndpointMeta>::ReplyTo,
            >,
        Router: thincan::RouterDispatch<
                Handlers,
                <Node as can_isotp_interface::IsoTpAsyncEndpointMeta>::ReplyTo,
            >,
        TxBuf: AsMut<[u8]>,
        M: RawMutex,
    {
        let deadline = Instant::now() + embassy_duration(self.config.ack_timeout);
        loop {
            let _ = pump_rx(iface, handlers, rx_buf, self.config.ack_timeout).await;
            if let Ok(ack) = acks.try_receive() {
                if ack.transfer_id != transfer_id {
                    continue;
                }
                match ack.kind {
                    schema::FileAckKind::Accept => return Ok(ack),
                    schema::FileAckKind::Reject | schema::FileAckKind::Abort => {
                        return Err(thincan::Error {
                            kind: thincan::ErrorKind::Other,
                        });
                    }
                    _ => continue,
                }
            }
            if Instant::now() >= deadline {
                return Err(thincan::Error::timeout());
            }
        }
    }

    async fn wait_for_accept_with_acks<M, const DEPTH: usize>(
        &mut self,
        acks: &mut AckStream<'_, M, DEPTH>,
        transfer_id: u32,
    ) -> Result<Ack, thincan::Error>
    where
        M: RawMutex,
    {
        let deadline = Instant::now() + embassy_duration(self.config.ack_timeout);
        loop {
            let now = Instant::now();
            if now >= deadline {
                return Err(thincan::Error::timeout());
            }
            let remaining = deadline - now;
            let ack = match with_timeout(remaining, acks.receive()).await {
                Ok(ack) => ack,
                Err(_) => return Err(thincan::Error::timeout()),
            };
            if ack.transfer_id != transfer_id {
                continue;
            }
            match ack.kind {
                schema::FileAckKind::Accept => return Ok(ack),
                schema::FileAckKind::Reject | schema::FileAckKind::Abort => {
                    return Err(thincan::Error {
                        kind: thincan::ErrorKind::Other,
                    });
                }
                _ => continue,
            }
        }
    }
}

async fn pump_rx<Node, Router, TxBuf, Handlers>(
    iface: &mut thincan::Interface<Node, Router, TxBuf>,
    handlers: &mut Handlers,
    rx_buf: &mut [u8],
    timeout: Duration,
) -> Result<(), thincan::Error>
where
    Node: can_isotp_interface::IsoTpAsyncEndpointMeta
        + can_isotp_interface::IsoTpAsyncEndpointMetaRecvInto<
            ReplyTo = <Node as can_isotp_interface::IsoTpAsyncEndpointMeta>::ReplyTo,
        >,
    Router: thincan::RouterDispatch<
            Handlers,
            <Node as can_isotp_interface::IsoTpAsyncEndpointMeta>::ReplyTo,
        >,
    TxBuf: AsMut<[u8]>,
{
    let _ = iface
        .recv_one_dispatch_with_meta_async(handlers, timeout, rx_buf, |_| Ok(()))
        .await?;
    Ok(())
}

pub struct AckSender<'a, A: Atlas> {
    scratch: thincan::capnp::CapnpScratchRef<'a>,
    send_timeout: Duration,
    _atlas: PhantomData<A>,
}

impl<'a, A: Atlas> AckSender<'a, A> {
    pub fn new(scratch: thincan::capnp::CapnpScratchRef<'a>) -> Self {
        Self {
            scratch,
            send_timeout: Duration::from_millis(200),
            _atlas: PhantomData,
        }
    }

    pub fn set_send_timeout(&mut self, timeout: Duration) {
        self.send_timeout = timeout;
    }

    pub async fn send_one<Node, Router, TxBuf>(
        &mut self,
        iface: &mut thincan::Interface<Node, Router, TxBuf>,
        ack: crate::AckToSend<<Node as can_isotp_interface::IsoTpAsyncEndpointMeta>::ReplyTo>,
    ) -> Result<(), thincan::Error>
    where
        Node: can_isotp_interface::IsoTpAsyncEndpointMeta,
        TxBuf: AsMut<[u8]>,
        <Node as can_isotp_interface::IsoTpAsyncEndpointMeta>::ReplyTo: Copy,
    {
        let value = self
            .scratch
            .builder_value::<A::FileAck, crate::schema::file_ack::Owned, _>(|root| {
                root.set_transfer_id(ack.ack.transfer_id);
                root.set_kind(ack.ack.kind);
                root.set_next_offset(ack.ack.next_offset);
                root.set_chunk_size(ack.ack.chunk_size);
                root.set_error(ack.ack.error);
                Ok(())
            });
        iface
            .send_encoded_to_async(ack.reply_to, &value, self.send_timeout)
            .await
    }
}

/// `FileTransferTx` adapter that sends via a shared `Mutex<Interface<...>>`.
pub struct MutexInterfaceTx<'a, Mtx, Node, Router, TxBuf>
where
    Mtx: RawMutex,
{
    iface: &'a Mutex<Mtx, thincan::Interface<Node, Router, TxBuf>>,
}

impl<'a, Mtx, Node, Router, TxBuf> MutexInterfaceTx<'a, Mtx, Node, Router, TxBuf>
where
    Mtx: RawMutex,
{
    pub const fn new(iface: &'a Mutex<Mtx, thincan::Interface<Node, Router, TxBuf>>) -> Self {
        Self { iface }
    }
}

impl<Mtx, Node, Router, TxBuf, ReplyTo> crate::FileTransferTx<ReplyTo>
    for MutexInterfaceTx<'_, Mtx, Node, Router, TxBuf>
where
    Mtx: RawMutex,
    Node: can_isotp_interface::IsoTpAsyncEndpointMeta<ReplyTo = ReplyTo>,
    TxBuf: AsMut<[u8]>,
{
    async fn send_encoded<M: thincan::Message, V: thincan::Encode<M>>(
        &mut self,
        to: ReplyTo,
        value: &V,
        timeout: Duration,
    ) -> Result<(), thincan::Error> {
        let mut guard = self.iface.lock().await;
        guard.send_encoded_to_async(to, value, timeout).await
    }
}

pub async fn send_file_with_dispatch<
    A,
    Node,
    Router,
    TxBuf,
    Handlers,
    M,
    const ACK_DEPTH: usize,
    const MAX_CHUNK: usize,
    const WINDOW_CHUNKS: usize,
    S,
>(
    iface: &mut thincan::Interface<Node, Router, TxBuf>,
    handlers: &mut Handlers,
    sender: &mut SenderNoAlloc<'_, A, MAX_CHUNK, WINDOW_CHUNKS>,
    rx_buf: &mut [u8],
    to: <Node as can_isotp_interface::IsoTpAsyncEndpointMeta>::ReplyTo,
    total_len: usize,
    source: &mut S,
) -> Result<SendResult, thincan::Error>
where
    A: Atlas,
    Handlers: crate::FileTransferSenderState,
    <Handlers as crate::FileTransferSenderState>::AckInboxQueue:
        core::ops::Deref<Target = AckInboxQueue<M, ACK_DEPTH>>,
    M: RawMutex,
    Node: can_isotp_interface::IsoTpAsyncEndpointMeta
        + can_isotp_interface::IsoTpAsyncEndpointMetaRecvInto<
            ReplyTo = <Node as can_isotp_interface::IsoTpAsyncEndpointMeta>::ReplyTo,
        >,
    Router: thincan::RouterDispatch<
            Handlers,
            <Node as can_isotp_interface::IsoTpAsyncEndpointMeta>::ReplyTo,
        >,
    TxBuf: AsMut<[u8]>,
    S: AsyncChunkSource,
{
    let ack_queue = handlers.ack_inbox_queue();
    let mut acks = AckStream::new(&*ack_queue);
    let transfer_id = sender.next_transfer_id();

    sender
        .send_from(
            iface,
            handlers,
            rx_buf,
            &mut acks,
            to,
            transfer_id,
            total_len,
            source,
        )
        .await
}

pub async fn send_file<
    A,
    Tx,
    SenderState,
    M,
    const ACK_DEPTH: usize,
    const MAX_CHUNK: usize,
    const WINDOW_CHUNKS: usize,
    S,
    ReplyTo,
>(
    tx: &mut Tx,
    sender_state: &SenderState,
    sender: &mut SenderNoAlloc<'_, A, MAX_CHUNK, WINDOW_CHUNKS>,
    to: ReplyTo,
    total_len: usize,
    source: &mut S,
) -> Result<SendResult, thincan::Error>
where
    A: Atlas,
    Tx: FileTransferTx<ReplyTo>,
    SenderState: crate::FileTransferSenderState,
    <SenderState as crate::FileTransferSenderState>::AckInboxQueue:
        core::ops::Deref<Target = AckInboxQueue<M, ACK_DEPTH>>,
    M: RawMutex,
    S: AsyncChunkSource,
    ReplyTo: Copy,
{
    let ack_queue = sender_state.ack_inbox_queue();
    let mut acks = AckStream::new(&*ack_queue);
    let transfer_id = sender.next_transfer_id();
    sender
        .send_from_with_acks(tx, &mut acks, to, transfer_id, total_len, source)
        .await
}

fn embassy_duration(duration: Duration) -> embassy_time::Duration {
    embassy_time::Duration::from_micros(duration.as_micros().min(u128::from(u64::MAX)) as u64)
}
