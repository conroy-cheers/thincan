#![cfg(all(feature = "std", feature = "capnp"))]

use core::time::Duration;

#[path = "support/person_capnp.rs"]
mod person_capnp;
mod support;
use support::*;

use capnp::message::ReaderOptions;
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

#[tokio::test(flavor = "current_thread")]
async fn async_iso_tp_recv_one_dispatch_roundtrips()
-> Result<(), thincan::SendMessageError<thincan::Error>> {
    let (a_node, b_node) = PipeEnd::pair(0xAA, 0xBB);
    let b_pump = b_node.clone();
    let a_iface =
        maplet::Interface::<NoopRawMutex, _, _, 8, 256, 4>::new(a_node.clone(), [0u8; 256]);
    let b_iface =
        maplet::Interface::<NoopRawMutex, _, _, 8, 256, 4>::new(b_node.clone(), [0u8; 256]);
    let a = a_iface.bus().scope::<protocol_bundle::Bundle>();
    let b = b_iface.bus().scope::<protocol_bundle::Bundle>();

    // Send B first, then A. Receiver asks for A first; B should be buffered.
    a.__send_capnp_to::<atlas::B, _>(
        0xBB,
        &PersonValue::named("B"),
        deadline(Duration::from_millis(10)),
    )
    .await?;
    a.__send_capnp_to::<atlas::A, _>(
        0xBB,
        &PersonValue::named("A"),
        deadline(Duration::from_millis(10)),
    )
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
async fn filtered_recv_allows_concurrent_waiters()
-> Result<(), thincan::SendMessageError<thincan::Error>> {
    let (a_node, b_node) = PipeEnd::pair(0xAA, 0xBB);
    let b_pump = b_node.clone();
    let a_iface =
        maplet::Interface::<NoopRawMutex, _, _, 8, 256, 4>::new(a_node.clone(), [0u8; 256]);
    let b_iface =
        maplet::Interface::<NoopRawMutex, _, _, 8, 256, 4>::new(b_node.clone(), [0u8; 256]);
    let a = a_iface.bus().scope::<protocol_bundle::Bundle>();
    let b = b_iface.bus().scope::<protocol_bundle::Bundle>();

    a.__send_capnp_to::<atlas::A, _>(
        0xBB,
        &PersonValue::named("one"),
        deadline(Duration::from_millis(10)),
    )
    .await?;
    a.__send_capnp_to::<atlas::A, _>(
        0xBB,
        &PersonValue::named("two"),
        deadline(Duration::from_millis(10)),
    )
    .await?;

    for (from, payload) in b_pump.drain_incoming() {
        b.ingest(from, &payload).await.unwrap();
    }

    let b_one = b;
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

    let b_two = b;
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
