//! Fixtures shared by the thincan integration tests: an in-memory link, a
//! `Person` value to send over it, and deadlines on Tokio's clock.
#![allow(dead_code)]

use crate::person_capnp;
use can_isotp_interface::{Destination, IsoTpAsyncWrite, TransmitError};
use capnp::message::SingleSegmentAllocator;
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
    type Error = thincan::Error;
    type Deadline = tokio::time::Instant;
    async fn send(
        &mut self,
        _: Destination,
        payload: &[u8],
        _: tokio::time::Instant,
    ) -> Result<(), TransmitError<thincan::Error>> {
        self.queues.lock().unwrap()[self.side].push_back((self.from_addr, payload.to_vec()));
        Ok(())
    }
}

/// A `Person` message value.
#[derive(Clone, Copy)]
pub struct PersonValue {
    pub name: &'static str,
    pub email: &'static str,
}

impl PersonValue {
    pub fn named(name: &'static str) -> Self {
        Self { name, email: "e" }
    }
}

impl<M> thincan::EncodeCapnp<M> for PersonValue
where
    M: thincan::CapnpMessage<Owned = person_capnp::person::Owned>,
{
    fn max_encoded_len(&self) -> usize {
        // Root pointer and a two-pointer struct, then each text NUL-terminated
        // and padded to a word.
        let text = |s: &str| (s.len() + 1).div_ceil(8) * 8;
        24 + text(self.name) + text(self.email)
    }

    fn encode(&self, out: &mut [u8]) -> Result<usize, thincan::Error> {
        let mut scratch = [0u8; 128];
        let mut msg = capnp::message::Builder::new(SingleSegmentAllocator::new(&mut scratch));
        let mut root: person_capnp::person::Builder = msg.init_root();
        root.set_name(self.name);
        root.set_email(self.email);
        let body = msg.get_segments_for_output()[0];
        if out.len() < body.len() {
            return Err(thincan::Error {
                kind: thincan::ErrorKind::BufferTooSmall {
                    needed: body.len(),
                    got: out.len(),
                },
            });
        }
        out[..body.len()].copy_from_slice(body);
        Ok(body.len())
    }
}
