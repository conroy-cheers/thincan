#![cfg(all(feature = "std", feature = "capnp"))]

#[path = "support/person_capnp.rs"]
mod person_capnp;
mod support;
use support::*;

use core::time::Duration;

use capnp::message::ReaderOptions;
use embassy_sync::blocking_mutex::raw::NoopRawMutex;

thincan::bus_atlas! {
    pub mod atlas {
        0x1001 => FileReq(capnp = crate::person_capnp::person::Owned);
        0x1002 => FileChunk(capnp = crate::person_capnp::person::Owned);
    }
}

pub mod protocol_bundle {
    #[derive(Clone, Copy, Debug, Default)]
    pub struct Bundle;

    pub const MESSAGE_COUNT: usize = 2;

    impl thincan::BundleSpec<MESSAGE_COUNT> for Bundle {
        const MESSAGE_IDS: [u16; MESSAGE_COUNT] = [
            <super::atlas::FileReq as thincan::Message>::ID,
            <super::atlas::FileChunk as thincan::Message>::ID,
        ];
    }
}

thincan::maplet! {
    pub mod maplet: atlas {
        bundles [protocol_bundle];
    }
}

#[tokio::test(flavor = "current_thread")]
async fn end_to_end_file_to_logs() -> Result<(), thincan::SendMessageError<thincan::Error>> {
    let (a_node, b_node) = PipeEnd::pair(0xA0, 0xB0);
    let b_pump = b_node.clone();

    let sender_iface =
        maplet::Interface::<NoopRawMutex, _, _, 16, 256, 4>::new(a_node.clone(), [0u8; 256]);
    let receiver_iface =
        maplet::Interface::<NoopRawMutex, _, _, 16, 256, 4>::new(b_node.clone(), [0u8; 256]);
    let sender = sender_iface.bus().scope::<protocol_bundle::Bundle>();
    let receiver = receiver_iface.bus().scope::<protocol_bundle::Bundle>();

    sender
        .__send_capnp_to::<atlas::FileReq, _>(
            0xB0,
            &PersonValue::named("req"),
            deadline(Duration::from_millis(10)),
        )
        .await?;
    sender
        .__send_capnp_to::<atlas::FileChunk, _>(
            0xB0,
            &PersonValue::named("chunk-1"),
            deadline(Duration::from_millis(10)),
        )
        .await?;
    sender
        .__send_capnp_to::<atlas::FileChunk, _>(
            0xB0,
            &PersonValue::named("chunk-2"),
            deadline(Duration::from_millis(10)),
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

    let chunk_names = [chunk1, chunk2].map(|m| {
        m.with_root(ReaderOptions::default(), |root| {
            root.get_name().unwrap().to_str().unwrap().to_owned()
        })
        .unwrap()
    });

    assert_eq!(chunk_names, ["chunk-1".to_owned(), "chunk-2".to_owned()]);

    Ok(())
}
