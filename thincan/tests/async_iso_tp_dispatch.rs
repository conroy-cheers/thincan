#![cfg(all(feature = "std", feature = "capnp"))]

use core::time::Duration;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

#[path = "support/person_capnp.rs"]
mod person_capnp;

use can_isotp_interface::{
    IsoTpAsyncEndpoint, IsoTpAsyncEndpointRecvInto, RecvControl, RecvError, RecvMeta,
    RecvMetaIntoStatus, RecvStatus, SendError,
};
use capnp::message::{ReaderOptions, SingleSegmentAllocator};
use embassy_sync::blocking_mutex::raw::NoopRawMutex;

thincan::bus_atlas! {
    pub mod atlas {
        0x0100 => A(capnp = crate::person_capnp::person::Owned);
        0x0101 => B(capnp = crate::person_capnp::person::Owned);
    }
}

pub mod protocol_bundle {
    #[derive(Clone, Copy, Debug, Default)]
    pub struct Bundle;

    pub const MESSAGE_COUNT: usize = 2;

    impl thincan::BundleSpec<MESSAGE_COUNT> for Bundle {
        const MESSAGE_IDS: [u16; MESSAGE_COUNT] = [
            <super::atlas::A as thincan::Message>::ID,
            <super::atlas::B as thincan::Message>::ID,
        ];
    }
}

thincan::maplet! {
    pub mod maplet: atlas {
        bundles [protocol_bundle];
    }
}

#[derive(Default)]
struct SharedPipe {
    a_to_b: VecDeque<(u8, Vec<u8>)>,
    b_to_a: VecDeque<(u8, Vec<u8>)>,
}

#[derive(Clone, Copy)]
enum Direction {
    A,
    B,
}

#[derive(Clone)]
struct PipeEnd {
    shared: Arc<Mutex<SharedPipe>>,
    dir: Direction,
    from_addr: u8,
}

impl PipeEnd {
    fn pair(a_addr: u8, b_addr: u8) -> (Self, Self) {
        let shared = Arc::new(Mutex::new(SharedPipe::default()));
        (
            Self {
                shared: shared.clone(),
                dir: Direction::A,
                from_addr: a_addr,
            },
            Self {
                shared,
                dir: Direction::B,
                from_addr: b_addr,
            },
        )
    }

    fn drain_incoming(&self) -> Vec<(u8, Vec<u8>)> {
        let mut shared = self.shared.lock().unwrap();
        let queue = match self.dir {
            Direction::A => &mut shared.b_to_a,
            Direction::B => &mut shared.a_to_b,
        };
        queue.drain(..).collect()
    }
}

impl IsoTpAsyncEndpoint for PipeEnd {
    type Error = thincan::Error;

    async fn send_to(
        &mut self,
        _to: u8,
        payload: &[u8],
        _timeout: Duration,
    ) -> Result<(), SendError<Self::Error>> {
        let mut shared = self.shared.lock().unwrap();
        match self.dir {
            Direction::A => shared.a_to_b.push_back((self.from_addr, payload.to_vec())),
            Direction::B => shared.b_to_a.push_back((self.from_addr, payload.to_vec())),
        }
        Ok(())
    }

    async fn send_functional_to(
        &mut self,
        _functional_to: u8,
        payload: &[u8],
        _timeout: Duration,
    ) -> Result<(), SendError<Self::Error>> {
        let mut shared = self.shared.lock().unwrap();
        match self.dir {
            Direction::A => shared.a_to_b.push_back((self.from_addr, payload.to_vec())),
            Direction::B => shared.b_to_a.push_back((self.from_addr, payload.to_vec())),
        }
        Ok(())
    }

    async fn recv_one<Cb>(
        &mut self,
        _timeout: Duration,
        mut on_payload: Cb,
    ) -> Result<RecvStatus, RecvError<Self::Error>>
    where
        Cb: FnMut(RecvMeta, &[u8]) -> Result<RecvControl, Self::Error>,
    {
        let mut shared = self.shared.lock().unwrap();
        let queue = match self.dir {
            Direction::A => &mut shared.b_to_a,
            Direction::B => &mut shared.a_to_b,
        };

        let Some((from, payload)) = queue.pop_front() else {
            return Ok(RecvStatus::TimedOut);
        };

        let _ = on_payload(RecvMeta { reply_to: from }, &payload).map_err(RecvError::Backend)?;
        Ok(RecvStatus::DeliveredOne)
    }
}

impl IsoTpAsyncEndpointRecvInto for PipeEnd {
    type Error = thincan::Error;

    async fn recv_one_into(
        &mut self,
        _timeout: Duration,
        out: &mut [u8],
    ) -> Result<RecvMetaIntoStatus, RecvError<Self::Error>> {
        let mut shared = self.shared.lock().unwrap();
        let queue = match self.dir {
            Direction::A => &mut shared.b_to_a,
            Direction::B => &mut shared.a_to_b,
        };

        let Some((from, payload)) = queue.pop_front() else {
            return Ok(RecvMetaIntoStatus::TimedOut);
        };

        if out.len() < payload.len() {
            return Err(RecvError::BufferTooSmall {
                needed: payload.len(),
                got: out.len(),
            });
        }

        out[..payload.len()].copy_from_slice(&payload);
        Ok(RecvMetaIntoStatus::DeliveredOne {
            meta: RecvMeta { reply_to: from },
            len: payload.len(),
        })
    }
}

#[derive(Clone, Copy)]
struct PersonValue {
    name: &'static str,
}

impl<M> thincan::EncodeCapnp<M> for PersonValue
where
    M: thincan::CapnpMessage<Owned = person_capnp::person::Owned>,
{
    fn max_encoded_len(&self) -> usize {
        96
    }

    fn encode(&self, out: &mut [u8]) -> Result<usize, thincan::Error> {
        let mut scratch = [0u8; 96];
        let mut msg = capnp::message::Builder::new(SingleSegmentAllocator::new(&mut scratch));
        let mut root: person_capnp::person::Builder = msg.init_root();
        root.set_name(self.name);
        root.set_email("e");

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

#[tokio::test(flavor = "current_thread")]
async fn async_iso_tp_recv_one_dispatch_roundtrips() -> Result<(), thincan::Error> {
    let (a_node, b_node) = PipeEnd::pair(0xAA, 0xBB);
    let b_pump = b_node.clone();
    let a_iface = maplet::Interface::<NoopRawMutex, _, _, 8, 256, 4>::new(a_node, [0u8; 256]);
    let b_iface = maplet::Interface::<NoopRawMutex, _, _, 8, 256, 4>::new(b_node, [0u8; 256]);
    let a = a_iface.handle().scope::<protocol_bundle::Bundle>();
    let b = b_iface.handle().scope::<protocol_bundle::Bundle>();

    // Send B first, then A. Receiver asks for A first; B should be buffered.
    a.__send_capnp_to::<atlas::B, _>(0xBB, &PersonValue { name: "B" }, Duration::from_millis(10))
        .await?;
    a.__send_capnp_to::<atlas::A, _>(0xBB, &PersonValue { name: "A" }, Duration::from_millis(10))
        .await?;

    for (from, payload) in b_pump.drain_incoming() {
        b.ingest(from, &payload).await.unwrap();
    }

    let got_a = b.__recv_next_capnp_from::<atlas::A>(0xAA).await?;
    let got_a_name = got_a
        .with_root(ReaderOptions::default(), |root| {
            root.get_name().unwrap().to_str().unwrap().to_owned()
        })
        .unwrap();
    assert_eq!(got_a_name, "A");

    let got_b = b.__recv_next_capnp_from::<atlas::B>(0xAA).await?;
    let got_b_name = got_b
        .with_root(ReaderOptions::default(), |root| {
            root.get_name().unwrap().to_str().unwrap().to_owned()
        })
        .unwrap();
    assert_eq!(got_b_name, "B");

    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn filtered_recv_allows_concurrent_waiters() -> Result<(), thincan::Error> {
    let (a_node, b_node) = PipeEnd::pair(0xAA, 0xBB);
    let b_pump = b_node.clone();
    let a_iface = maplet::Interface::<NoopRawMutex, _, _, 8, 256, 4>::new(a_node, [0u8; 256]);
    let b_iface = maplet::Interface::<NoopRawMutex, _, _, 8, 256, 4>::new(b_node, [0u8; 256]);
    let a = a_iface.handle().scope::<protocol_bundle::Bundle>();
    let b = b_iface.handle().scope::<protocol_bundle::Bundle>();

    a.__send_capnp_to::<atlas::A, _>(
        0xBB,
        &PersonValue { name: "one" },
        Duration::from_millis(10),
    )
    .await?;
    a.__send_capnp_to::<atlas::A, _>(
        0xBB,
        &PersonValue { name: "two" },
        Duration::from_millis(10),
    )
    .await?;

    for (from, payload) in b_pump.drain_incoming() {
        b.ingest(from, &payload).await.unwrap();
    }

    let b_one = b.clone();
    let wait_one = async move {
        b_one
            .__recv_next_capnp_from_where::<atlas::A, _>(0xAA, |body| {
                thincan::CapnpTyped::<person_capnp::person::Owned>::new(body)
                    .with_root(ReaderOptions::default(), |root| {
                        root.get_name().ok().and_then(|v| v.to_str().ok()) == Some("one")
                    })
                    .unwrap_or(false)
            })
            .await
    };

    let b_two = b.clone();
    let wait_two = async move {
        b_two
            .__recv_next_capnp_from_where::<atlas::A, _>(0xAA, |body| {
                thincan::CapnpTyped::<person_capnp::person::Owned>::new(body)
                    .with_root(ReaderOptions::default(), |root| {
                        root.get_name().ok().and_then(|v| v.to_str().ok()) == Some("two")
                    })
                    .unwrap_or(false)
            })
            .await
    };

    let (one, two) = tokio::join!(wait_one, wait_two);
    let one: thincan::Received<atlas::A, 256> = one?;
    let two: thincan::Received<atlas::A, 256> = two?;

    let one_name = one
        .with_root(ReaderOptions::default(), |root| {
            root.get_name().unwrap().to_str().unwrap().to_owned()
        })
        .unwrap();
    let two_name = two
        .with_root(ReaderOptions::default(), |root| {
            root.get_name().unwrap().to_str().unwrap().to_owned()
        })
        .unwrap();

    assert_eq!(one_name, "one");
    assert_eq!(two_name, "two");

    Ok(())
}
