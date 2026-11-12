//! Fixtures shared by the file-transfer tests: an in-memory link, a lossy
//! writer, a memory store, and deadlines on Tokio's clock.
#![allow(dead_code)]

use can_isotp_interface::{Destination, IsoTpAsyncWrite, TransmitError};
use core::time::Duration;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

/// The absolute deadline `duration` from now.
pub fn deadline(duration: Duration) -> tokio::time::Instant {
    tokio::time::Instant::now() + duration
}

/// Messages in flight, as `(sender address, payload)`, one queue per direction.
type Queues = Arc<Mutex<[VecDeque<(u8, Vec<u8>)>; 2]>>;

/// One end of an in-memory link.
#[derive(Clone)]
pub struct PipeEnd {
    queues: Queues,
    side: usize,
    from_addr: u8,
}

impl PipeEnd {
    /// Both ends of a link between nodes at `a_addr` and `b_addr`.
    pub fn pair(a_addr: u8, b_addr: u8) -> (Self, Self) {
        let queues = Arc::new(Mutex::new([VecDeque::new(), VecDeque::new()]));
        (
            Self {
                queues: queues.clone(),
                side: 0,
                from_addr: a_addr,
            },
            Self {
                queues,
                side: 1,
                from_addr: b_addr,
            },
        )
    }

    /// Everything the other end has sent so far.
    pub fn drain_incoming(&self) -> Vec<(u8, Vec<u8>)> {
        self.queues.lock().unwrap()[1 - self.side]
            .drain(..)
            .collect()
    }
}

impl IsoTpAsyncWrite for PipeEnd {
    type Error = ();
    type Deadline = tokio::time::Instant;
    async fn send(
        &mut self,
        _: Destination,
        payload: &[u8],
        _: tokio::time::Instant,
    ) -> Result<(), TransmitError<()>> {
        self.queues.lock().unwrap()[self.side].push_back((self.from_addr, payload.to_vec()));
        Ok(())
    }
}

/// Forwards through a pipe, delaying chunks by `chunk_delay` and dropping the
/// messages `drop(id, body)` selects.
#[derive(Clone)]
pub struct Faulty<F> {
    pub pipe: PipeEnd,
    pub drop: F,
    pub chunk_delay: Duration,
}

impl<F: Fn(u16, &[u8]) -> bool> IsoTpAsyncWrite for Faulty<F> {
    type Error = ();
    type Deadline = tokio::time::Instant;
    async fn send(
        &mut self,
        destination: Destination,
        payload: &[u8],
        deadline: tokio::time::Instant,
    ) -> Result<(), TransmitError<()>> {
        let id = u16::from_le_bytes([payload[0], payload[1]]);
        if id == 0x1002 && !self.chunk_delay.is_zero() {
            tokio::time::sleep(self.chunk_delay).await;
        }
        if (self.drop)(id, &payload[2..]) {
            return Ok(());
        }
        self.pipe.send(destination, payload, deadline).await
    }
}

/// A `FileAck` (id 0x1003) of kind `Complete`.
pub fn is_complete(id: u16, body: &[u8]) -> bool {
    id == 0x1003
        && thincan_file_transfer::decode_file_ack(thincan::CapnpTyped::<
            thincan_file_transfer::schema::file_ack::Owned,
        >::new(body))
        .is_ok_and(|ack| ack.kind == thincan_file_transfer::schema::FileAckKind::Complete)
}

#[derive(Debug, Default)]
pub struct MemoryStore {
    pub bytes: Vec<u8>,
}

impl thincan_file_transfer::AsyncFileStore for MemoryStore {
    type Error = ();
    type WriteHandle = ();

    async fn begin_write(&mut self, _: u32, total_len: u32) -> Result<(), ()> {
        self.bytes = vec![0; total_len as usize];
        Ok(())
    }

    async fn write_at(&mut self, _: &mut (), offset: u32, bytes: &[u8]) -> Result<(), ()> {
        let offset = offset as usize;
        self.bytes[offset..offset + bytes.len()].copy_from_slice(bytes);
        Ok(())
    }

    async fn commit(&mut self, _: ()) -> Result<(), ()> {
        Ok(())
    }

    async fn abort(&mut self, _: ()) {}
}
