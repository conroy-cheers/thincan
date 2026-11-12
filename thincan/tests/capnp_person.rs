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
        0x0100 => Person(capnp = crate::person_capnp::person::Owned);
        0x0101 => PersonAck(capnp = crate::person_capnp::person::Owned);
    }
}

pub mod protocol_bundle {
    #[derive(Clone, Copy, Debug, Default)]
    pub struct Bundle;

    pub const MESSAGE_COUNT: usize = 2;

    impl thincan::BundleSpec<MESSAGE_COUNT> for Bundle {
        const MESSAGE_IDS: [u16; MESSAGE_COUNT] = [
            <super::atlas::Person as thincan::Message>::ID,
            <super::atlas::PersonAck as thincan::Message>::ID,
        ];
    }
}

thincan::maplet! {
    pub mod maplet: atlas {
        bundles [protocol_bundle];
    }
}

#[tokio::test(flavor = "current_thread")]
async fn capnp_person_roundtrip_via_bus() -> Result<(), thincan::SendMessageError<thincan::Error>> {
    let (a_node, b_node) = PipeEnd::pair(0x11, 0x22);
    let a_pump = a_node.clone();
    let b_pump = b_node.clone();

    let a_iface =
        maplet::Interface::<NoopRawMutex, _, _, 8, 256, 4>::new(a_node.clone(), [0u8; 256]);
    let b_iface =
        maplet::Interface::<NoopRawMutex, _, _, 8, 256, 4>::new(b_node.clone(), [0u8; 256]);
    let a = a_iface.bus().scope::<protocol_bundle::Bundle>();
    let b = b_iface.bus().scope::<protocol_bundle::Bundle>();

    a.__send_capnp_to::<atlas::Person, _>(
        0x22,
        &PersonValue {
            name: "alice",
            email: "alice@example.com",
        },
        deadline(Duration::from_millis(10)),
    )
    .await?;

    for (from, payload) in b_pump.drain_incoming() {
        b.ingest(from, &payload).await.unwrap();
    }

    let got = b.__recv_next_capnp_from::<atlas::Person>(0x11).await?;
    let (name, email) = got
        .with_root(ReaderOptions::default(), |root| {
            (
                root.get_name().unwrap().to_str().unwrap().to_owned(),
                root.get_email().unwrap().to_str().unwrap().to_owned(),
            )
        })
        .unwrap();
    assert_eq!(name, "alice");
    assert_eq!(email, "alice@example.com");

    b.__send_capnp_functional_to::<atlas::PersonAck, _>(
        0x7F,
        &PersonValue {
            name: "ack",
            email: "ok",
        },
        deadline(Duration::from_millis(10)),
    )
    .await?;

    for (from, payload) in a_pump.drain_incoming() {
        a.ingest(from, &payload).await.unwrap();
    }

    let ack = a.__recv_next_capnp_from::<atlas::PersonAck>(0x22).await?;
    let ack_name = ack
        .with_root(ReaderOptions::default(), |root| {
            root.get_name().unwrap().to_str().unwrap().to_owned()
        })
        .unwrap();
    assert_eq!(ack_name, "ack");

    Ok(())
}
