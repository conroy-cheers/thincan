#![cfg(all(feature = "std", feature = "capnp"))]

#[path = "support/person_capnp.rs"]
mod person_capnp;

use core::time::Duration;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use can_isotp_interface::{
    IsoTpAsyncEndpoint, IsoTpAsyncEndpointRecvInto, RecvControl, RecvError, RecvMeta,
    RecvMetaIntoStatus, RecvStatus, SendError,
};
use capnp::message::{ReaderOptions, SingleSegmentAllocator};
use embassy_sync::blocking_mutex::raw::NoopRawMutex;

thincan::bus_atlas! {
    pub mod atlas {
        0x1001 => FileReq(capnp = crate::person_capnp::person::Owned);
        0x1002 => FileChunk(capnp = crate::person_capnp::person::Owned);
        0x3001 => LogLine(capnp = crate::person_capnp::person::Owned);
    }
}

pub mod protocol_bundle {
    #[derive(Clone, Copy, Debug, Default)]
    pub struct Bundle;

    pub const MESSAGE_COUNT: usize = 3;

    impl thincan::BundleSpec<MESSAGE_COUNT> for Bundle {
        const MESSAGE_IDS: [u16; MESSAGE_COUNT] = [
            <super::atlas::FileReq as thincan::Message>::ID,
            <super::atlas::FileChunk as thincan::Message>::ID,
            <super::atlas::LogLine as thincan::Message>::ID,
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
async fn end_to_end_file_to_logs() -> Result<(), thincan::Error> {
    let (a_node, b_node) = PipeEnd::pair(0xA0, 0xB0);
    let b_pump = b_node.clone();

    let sender_iface = maplet::Interface::<NoopRawMutex, _, _, 16, 256, 4>::new(a_node, [0u8; 256]);
    let receiver_iface =
        maplet::Interface::<NoopRawMutex, _, _, 16, 256, 4>::new(b_node, [0u8; 256]);
    let sender = sender_iface.handle().scope::<protocol_bundle::Bundle>();
    let receiver = receiver_iface.handle().scope::<protocol_bundle::Bundle>();

    sender
        .__send_capnp_to::<atlas::FileReq, _>(
            0xB0,
            &PersonValue { name: "req" },
            Duration::from_millis(10),
        )
        .await?;
    sender
        .__send_capnp_to::<atlas::FileChunk, _>(
            0xB0,
            &PersonValue { name: "chunk-1" },
            Duration::from_millis(10),
        )
        .await?;
    sender
        .__send_capnp_to::<atlas::FileChunk, _>(
            0xB0,
            &PersonValue { name: "chunk-2" },
            Duration::from_millis(10),
        )
        .await?;
    sender
        .__send_capnp_to::<atlas::LogLine, _>(
            0xB0,
            &PersonValue { name: "log-1" },
            Duration::from_millis(10),
        )
        .await?;
    sender
        .__send_capnp_to::<atlas::LogLine, _>(
            0xB0,
            &PersonValue { name: "log-2" },
            Duration::from_millis(10),
        )
        .await?;

    for (from, payload) in b_pump.drain_incoming() {
        receiver.ingest(from, &payload).await.unwrap();
    }

    let req = receiver
        .__recv_next_capnp_from::<atlas::FileReq>(0xA0)
        .await?;
    let req_name = req
        .with_root(ReaderOptions::default(), |root| {
            root.get_name().unwrap().to_str().unwrap().to_owned()
        })
        .unwrap();
    assert_eq!(req_name, "req");

    let chunk1 = receiver
        .__recv_next_capnp_from::<atlas::FileChunk>(0xA0)
        .await?;
    let chunk2 = receiver
        .__recv_next_capnp_from::<atlas::FileChunk>(0xA0)
        .await?;
    let line1 = receiver
        .__recv_next_capnp_from::<atlas::LogLine>(0xA0)
        .await?;
    let line2 = receiver
        .__recv_next_capnp_from::<atlas::LogLine>(0xA0)
        .await?;

    let chunk_names = [chunk1, chunk2].map(|m| {
        m.with_root(ReaderOptions::default(), |root| {
            root.get_name().unwrap().to_str().unwrap().to_owned()
        })
        .unwrap()
    });
    let line_names = [line1, line2].map(|m| {
        m.with_root(ReaderOptions::default(), |root| {
            root.get_name().unwrap().to_str().unwrap().to_owned()
        })
        .unwrap()
    });

    assert_eq!(chunk_names, ["chunk-1".to_owned(), "chunk-2".to_owned()]);
    assert_eq!(line_names, ["log-1".to_owned(), "log-2".to_owned()]);

    Ok(())
}
