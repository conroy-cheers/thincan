//! Integration test for the `bundle_instance!` macro.
//!
//! Verifies that:
//! - The macro generates a valid bundle instance struct.
//! - The generated `BundleFactory` impl integrates with `maplet::Bundles::new()`.
//! - Protocol method `impl` blocks in the same module can access `self.bus`.
//! - An end-to-end send/recv roundtrip works through the macro-generated instance.

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
use embassy_sync::blocking_mutex::raw::NoopRawMutex;

thincan::bus_atlas! {
    pub mod atlas {
        0x0200 => Greeting(capnp = crate::person_capnp::person::Owned);
    }
}

pub mod greeting_bundle {
    use capnp::message::{ReaderOptions, SingleSegmentAllocator};
    use core::time::Duration;

    #[derive(Clone, Copy, Debug, Default)]
    pub struct Bundle;

    pub const MESSAGE_COUNT: usize = 1;

    impl thincan::BundleSpec<MESSAGE_COUNT> for Bundle {
        const MESSAGE_IDS: [u16; MESSAGE_COUNT] =
            [<super::atlas::Greeting as thincan::Message>::ID];
    }

    // The macro under test: generates the instance struct + BundleFactory impl.
    thincan::bundle_instance! {
        pub struct GreetingBundleInstance for Bundle;
    }

    // Protocol methods live in a separate impl block in the same module.
    // This block validates that `self.bus` is accessible from here.
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
    >
        GreetingBundleInstance<'a, Maplet, RM, Node, TxBuf, MAX_TYPES, DEPTH, MAX_BODY, MAX_WAITERS>
    where
        Maplet: thincan::MapletSpec<MAX_TYPES> + thincan::MapletHasBundle<Bundle>,
        RM: thincan::RawMutex,
        Node: can_isotp_interface::IsoTpAsyncEndpoint,
        TxBuf: AsMut<[u8]>,
    {
        pub async fn send_greeting_to(
            &self,
            to: u8,
            value: &GreetingValue,
            timeout: Duration,
        ) -> Result<(), thincan::Error> {
            self.bus
                .__send_capnp_to::<super::atlas::Greeting, _>(to, value, timeout)
                .await
        }

        pub async fn recv_greeting_from(
            &self,
            from: u8,
        ) -> Result<String, thincan::Error> {
            let msg = self
                .bus
                .__recv_next_capnp_from::<super::atlas::Greeting>(from)
                .await?;
            msg.with_root(ReaderOptions::default(), |root| {
                root.get_name().unwrap().to_str().unwrap().to_owned()
            })
            .map_err(|_| thincan::Error {
                kind: thincan::ErrorKind::Other,
            })
        }
    }

    #[derive(Clone, Copy)]
    pub struct GreetingValue {
        pub name: &'static str,
    }

    impl thincan::EncodeCapnp<super::atlas::Greeting> for GreetingValue {
        fn max_encoded_len(&self) -> usize {
            96
        }

        fn encode(&self, out: &mut [u8]) -> Result<usize, thincan::Error> {
            let mut scratch = [0u8; 96];
            let mut msg =
                capnp::message::Builder::new(SingleSegmentAllocator::new(&mut scratch));
            let mut root: crate::person_capnp::person::Builder = msg.init_root();
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
}

thincan::maplet! {
    pub mod maplet: atlas {
        bundles [greeting = greeting_bundle];
    }
}

// --- Minimal test node (pipe-based) ---

#[derive(Default)]
struct SharedPipe {
    a_to_b: VecDeque<(u8, Vec<u8>)>,
    b_to_a: VecDeque<(u8, Vec<u8>)>,
}

#[derive(Clone, Copy)]
enum Dir {
    A,
    B,
}

#[derive(Clone)]
struct PipeEnd {
    shared: Arc<Mutex<SharedPipe>>,
    dir: Dir,
    addr: u8,
}

impl PipeEnd {
    fn pair(a_addr: u8, b_addr: u8) -> (Self, Self) {
        let shared = Arc::new(Mutex::new(SharedPipe::default()));
        (
            Self { shared: shared.clone(), dir: Dir::A, addr: a_addr },
            Self { shared, dir: Dir::B, addr: b_addr },
        )
    }

    fn drain_incoming(&self) -> Vec<(u8, Vec<u8>)> {
        let mut s = self.shared.lock().unwrap();
        let q = match self.dir {
            Dir::A => &mut s.b_to_a,
            Dir::B => &mut s.a_to_b,
        };
        q.drain(..).collect()
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
        let mut s = self.shared.lock().unwrap();
        match self.dir {
            Dir::A => s.a_to_b.push_back((self.addr, payload.to_vec())),
            Dir::B => s.b_to_a.push_back((self.addr, payload.to_vec())),
        }
        Ok(())
    }

    async fn send_functional_to(
        &mut self,
        _to: u8,
        payload: &[u8],
        _timeout: Duration,
    ) -> Result<(), SendError<Self::Error>> {
        let mut s = self.shared.lock().unwrap();
        match self.dir {
            Dir::A => s.a_to_b.push_back((self.addr, payload.to_vec())),
            Dir::B => s.b_to_a.push_back((self.addr, payload.to_vec())),
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
        let mut s = self.shared.lock().unwrap();
        let q = match self.dir {
            Dir::A => &mut s.b_to_a,
            Dir::B => &mut s.a_to_b,
        };
        let Some((from, payload)) = q.pop_front() else {
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
        let mut s = self.shared.lock().unwrap();
        let q = match self.dir {
            Dir::A => &mut s.b_to_a,
            Dir::B => &mut s.a_to_b,
        };
        let Some((from, payload)) = q.pop_front() else {
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

// --- Tests ---

#[test]
fn bundle_instance_macro_bundles_new_constructs() {
    // Verifies Bundles::new() works with a macro-generated BundleFactory impl.
    let (node, _) = PipeEnd::pair(0xAA, 0xBB);
    let iface =
        maplet::Interface::<NoopRawMutex, _, _, 4, 256, 2>::new(node.clone(), node, [0u8; 256]);
    let _bundles = maplet::Bundles::new(&iface);
    // If this compiles and runs, the BundleFactory impl is wired up correctly.
}

#[tokio::test(flavor = "current_thread")]
async fn bundle_instance_macro_send_recv_roundtrip() -> Result<(), thincan::Error> {
    let (a_node, b_node) = PipeEnd::pair(0xAA, 0xBB);
    let b_pump = b_node.clone();

    let a_iface =
        maplet::Interface::<NoopRawMutex, _, _, 8, 256, 4>::new(a_node.clone(), a_node, [0u8; 256]);
    let b_iface =
        maplet::Interface::<NoopRawMutex, _, _, 8, 256, 4>::new(b_node.clone(), b_node, [0u8; 256]);

    let a_bundles = maplet::Bundles::new(&a_iface);
    let b_bundles = maplet::Bundles::new(&b_iface);

    // Send a greeting from A to B via the macro-generated instance.
    a_bundles
        .greeting
        .send_greeting_to(
            0xBB,
            &greeting_bundle::GreetingValue { name: "hello" },
            Duration::from_millis(10),
        )
        .await?;

    // Pump the message into B's interface via a scoped ingest bus.
    let b_ingest = b_iface.bus().scope::<greeting_bundle::Bundle>();
    for (from, payload) in b_pump.drain_incoming() {
        b_ingest.ingest(from, &payload).await.unwrap();
    }

    // Receive via the macro-generated instance's protocol method.
    let name = b_bundles.greeting.recv_greeting_from(0xAA).await?;
    assert_eq!(name, "hello");

    Ok(())
}
