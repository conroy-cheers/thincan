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
mod support;
use support::*;

use core::time::Duration;

use embassy_sync::blocking_mutex::raw::NoopRawMutex;

thincan::bus_atlas! {
    pub mod atlas {
        0x0200 => Greeting(capnp = crate::person_capnp::person::Owned);
    }
}

pub mod greeting_bundle {
    use capnp::message::{ReaderOptions, SingleSegmentAllocator};

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
    > GreetingBundleInstance<'a, Maplet, RM, Node, TxBuf, MAX_TYPES, DEPTH, MAX_BODY, MAX_WAITERS>
    where
        Maplet: thincan::MapletSpec<MAX_TYPES> + thincan::MapletHasBundle<Bundle>,
        RM: thincan::RawMutex,
        Node: can_isotp_interface::IsoTpAsyncWrite,
        TxBuf: AsMut<[u8]>,
    {
        pub async fn send_greeting_to(
            &self,
            to: u8,
            value: &GreetingValue,
            timeout: Node::Deadline,
        ) -> Result<(), thincan::SendMessageError<Node::Error>> {
            self.bus
                .__send_capnp_to::<super::atlas::Greeting, _>(to, value, timeout)
                .await
        }

        pub async fn recv_greeting_from(&self, from: u8) -> Result<String, thincan::Error> {
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
            let mut msg = capnp::message::Builder::new(SingleSegmentAllocator::new(&mut scratch));
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

// --- Tests ---

#[test]
fn bundle_instance_macro_bundles_new_constructs() {
    // Verifies Bundles::new() works with a macro-generated BundleFactory impl.
    let (node, _) = PipeEnd::pair(0xAA, 0xBB);
    let iface = maplet::Interface::<NoopRawMutex, _, _, 4, 256, 2>::new(node.clone(), [0u8; 256]);
    let _bundles = maplet::Bundles::new(&iface);
    // If this compiles and runs, the BundleFactory impl is wired up correctly.
}

#[tokio::test(flavor = "current_thread")]
async fn bundle_instance_macro_send_recv_roundtrip()
-> Result<(), thincan::SendMessageError<thincan::Error>> {
    let (a_node, b_node) = PipeEnd::pair(0xAA, 0xBB);
    let b_pump = b_node.clone();

    let a_iface =
        maplet::Interface::<NoopRawMutex, _, _, 8, 256, 4>::new(a_node.clone(), [0u8; 256]);
    let b_iface =
        maplet::Interface::<NoopRawMutex, _, _, 8, 256, 4>::new(b_node.clone(), [0u8; 256]);

    let a_bundles = maplet::Bundles::new(&a_iface);
    let b_bundles = maplet::Bundles::new(&b_iface);

    // Send a greeting from A to B via the macro-generated instance.
    a_bundles
        .greeting
        .send_greeting_to(
            0xBB,
            &greeting_bundle::GreetingValue { name: "hello" },
            deadline(Duration::from_millis(10)),
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
