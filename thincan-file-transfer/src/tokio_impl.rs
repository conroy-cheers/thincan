#![cfg(feature = "tokio")]

use core::marker::PhantomData;
use core::time::Duration;

use alloc::collections::VecDeque;

use capnp::message::ReaderOptions;
use std::sync::Arc;
use tokio::sync::watch;

use crate::{Atlas, file_chunk, file_offer, schema};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SeqAck {
    seq: u64,
    ack: Option<Ack>,
}

/// Parsed `FileAck` fields (sender-side).
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

/// Tokio watch-based ack mailbox.
///
/// This is designed to be updated from dispatch handlers via [`AckInbox::push`].
#[derive(Clone, Debug)]
pub struct AckInbox {
    tx: watch::Sender<SeqAck>,
    next_seq: Arc<core::sync::atomic::AtomicU64>,
}

/// Receiver side of the ack mailbox.
#[derive(Debug)]
pub struct AckStream {
    rx: watch::Receiver<SeqAck>,
    last_seq: u64,
}

pub fn ack_channel() -> (AckInbox, AckStream) {
    let (tx, rx) = watch::channel(SeqAck { seq: 0, ack: None });
    let next_seq = Arc::new(core::sync::atomic::AtomicU64::new(1));
    (
        AckInbox {
            tx,
            next_seq: next_seq.clone(),
        },
        AckStream { rx, last_seq: 0 },
    )
}

impl AckInbox {
    pub fn push(&self, ack: Ack) {
        let seq = self
            .next_seq
            .fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        let _ = self.tx.send_replace(SeqAck {
            seq,
            ack: Some(ack),
        });
    }
}

impl AckStream {
    pub async fn wait_for_transfer(
        &mut self,
        transfer_id: u32,
        timeout: Duration,
    ) -> Result<Ack, thincan::Error> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let cur = *self.rx.borrow();
            if cur.seq != self.last_seq {
                self.last_seq = cur.seq;
                if let Some(ack) = cur.ack {
                    if ack.transfer_id == transfer_id {
                        return Ok(ack);
                    }
                }
            }

            let now = tokio::time::Instant::now();
            if now >= deadline {
                return Err(thincan::Error::timeout());
            }
            let remaining = deadline - now;
            match tokio::time::timeout(remaining, self.rx.changed()).await {
                Ok(Ok(())) => continue,
                Ok(Err(_closed)) => {
                    return Err(thincan::Error {
                        kind: thincan::ErrorKind::Other,
                    });
                }
                Err(_) => return Err(thincan::Error::timeout()),
            }
        }
    }
}

/// High-level async sender that performs ack-driven throttling.
#[derive(Debug, Clone)]
pub struct AsyncSender<A> {
    next_transfer_id: u32,
    sender_max_chunk_size: usize,
    window_chunks: usize,
    max_retries: usize,
    _atlas: PhantomData<A>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AsyncSendResult {
    pub transfer_id: u32,
    pub total_len: usize,
    pub chunk_size: usize,
    pub chunks_sent: usize,
    pub retries: usize,
}

impl<A> Default for AsyncSender<A> {
    fn default() -> Self {
        Self::new()
    }
}

impl<A> AsyncSender<A> {
    pub fn new() -> Self {
        Self {
            next_transfer_id: 1,
            sender_max_chunk_size: crate::DEFAULT_CHUNK_SIZE,
            window_chunks: 4,
            max_retries: 5,
            _atlas: PhantomData,
        }
    }

    pub fn sender_max_chunk_size(&self) -> usize {
        self.sender_max_chunk_size
    }

    pub fn set_sender_max_chunk_size(&mut self, size: usize) -> Result<(), thincan::Error> {
        if size == 0 {
            return Err(thincan::Error {
                kind: thincan::ErrorKind::Other,
            });
        }
        self.sender_max_chunk_size = size;
        Ok(())
    }

    pub fn window_chunks(&self) -> usize {
        self.window_chunks
    }

    pub fn set_window_chunks(&mut self, window: usize) -> Result<(), thincan::Error> {
        if window == 0 {
            return Err(thincan::Error {
                kind: thincan::ErrorKind::Other,
            });
        }
        self.window_chunks = window;
        Ok(())
    }

    pub fn max_retries(&self) -> usize {
        self.max_retries
    }

    pub fn set_max_retries(&mut self, max_retries: usize) {
        self.max_retries = max_retries;
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

impl<A> AsyncSender<A>
where
    A: Atlas,
{
    pub async fn send_file<Node, Router, TxBuf>(
        &mut self,
        iface: &mut thincan::Interface<Node, Router, TxBuf>,
        to: u8,
        acks: &mut AckStream,
        bytes: &[u8],
        timeout: Duration,
    ) -> Result<AsyncSendResult, thincan::Error>
    where
        Node: can_isotp_interface::IsoTpAsyncEndpoint,
        TxBuf: AsMut<[u8]>,
    {
        let transfer_id = self.alloc_transfer_id();
        self.send_file_with_id(iface, to, acks, transfer_id, bytes, timeout)
            .await
    }

    pub async fn send_file_with_id<Node, Router, TxBuf>(
        &mut self,
        iface: &mut thincan::Interface<Node, Router, TxBuf>,
        to: u8,
        acks: &mut AckStream,
        transfer_id: u32,
        bytes: &[u8],
        timeout: Duration,
    ) -> Result<AsyncSendResult, thincan::Error>
    where
        Node: can_isotp_interface::IsoTpAsyncEndpoint,
        TxBuf: AsMut<[u8]>,
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

        iface
            .send_encoded_to_async::<A::FileReq, _>(
                to,
                &file_offer::<A>(transfer_id, total_len_u32, max_chunk_u32, &[]),
                timeout,
            )
            .await?;

        let accept = loop {
            let ack = acks.wait_for_transfer(transfer_id, timeout).await?;
            match ack.kind {
                schema::FileAckKind::Accept => break ack,
                schema::FileAckKind::Reject | schema::FileAckKind::Abort => {
                    return Err(thincan::Error {
                        kind: thincan::ErrorKind::Other,
                    });
                }
                _ => continue,
            }
        };

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
        let mut in_flight: VecDeque<u32> = VecDeque::new();

        while (last_acked as usize) < total_len {
            while in_flight.len() < self.window_chunks && (next_to_send as usize) < total_len {
                let offset = next_to_send as usize;
                let end = (offset + chunk_size).min(total_len);
                let data = &bytes[offset..end];
                iface
                    .send_encoded_to_async::<A::FileChunk, _>(
                        to,
                        &file_chunk::<A>(transfer_id, next_to_send, data),
                        timeout,
                    )
                    .await?;
                in_flight.push_back(next_to_send);
                next_to_send = next_to_send
                    .checked_add(u32::try_from(data.len()).map_err(|_| thincan::Error {
                        kind: thincan::ErrorKind::Other,
                    })?)
                    .ok_or(thincan::Error {
                        kind: thincan::ErrorKind::Other,
                    })?;
                chunks_sent += 1;
            }

            let ack = match acks.wait_for_transfer(transfer_id, timeout).await {
                Ok(a) => a,
                Err(e) if e.kind == thincan::ErrorKind::Timeout => {
                    retries += 1;
                    if retries > self.max_retries {
                        return Err(e);
                    }
                    next_to_send = last_acked;
                    in_flight.clear();
                    continue;
                }
                Err(e) => return Err(e),
            };

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
            while in_flight.front().is_some_and(|&off| off < last_acked) {
                let _ = in_flight.pop_front();
            }

            if ack.kind == schema::FileAckKind::Complete {
                if (last_acked as usize) != total_len {
                    return Err(thincan::Error {
                        kind: thincan::ErrorKind::Other,
                    });
                }
                break;
            }
        }

        Ok(AsyncSendResult {
            transfer_id,
            total_len,
            chunk_size,
            chunks_sent,
            retries,
        })
    }
}
