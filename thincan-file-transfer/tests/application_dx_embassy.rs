#![cfg(all(feature = "tokio", feature = "embassy"))]

mod support;
use support::{MemoryStore, PipeEnd, deadline};

use core::time::Duration;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

thincan::bus_atlas! {
    pub mod atlas {
        0x1001 => FileReq(capnp = thincan_file_transfer::schema::file_req::Owned);
        0x1002 => FileChunk(capnp = thincan_file_transfer::schema::file_chunk::Owned);
        0x1003 => FileAck(capnp = thincan_file_transfer::schema::file_ack::Owned);
    }
}

pub mod protocol_bundle {
    pub type Bundle = thincan_file_transfer::FileTransferBundle<super::atlas::Atlas>;
    pub const MESSAGE_COUNT: usize = thincan_file_transfer::FILE_TRANSFER_MESSAGE_COUNT;
}

thincan::maplet! {
    pub mod maplet: atlas {
        bundles [file_transfer = protocol_bundle];
    }
}

impl thincan_file_transfer::Atlas for atlas::Atlas {
    type FileReq = atlas::FileReq;
    type FileChunk = atlas::FileChunk;
    type FileAck = atlas::FileAck;
}

#[tokio::test(flavor = "current_thread")]
async fn application_dx_end_to_end_embassy_no_alloc_bundle_api() {
    const MAX_METADATA: usize = 16;
    const MAX_CHUNK: usize = 32;

    let (a_node, b_node) = PipeEnd::pair(0x31, 0x32);
    let a_pump = a_node.clone();
    let b_pump = b_node.clone();

    let mut a_tx = [0u8; 512];
    let mut b_tx = [0u8; 512];
    let a_iface = maplet::Interface::<thincan::NoopRawMutex, _, _, 16, 512, 4>::new(
        a_node.clone(),
        &mut a_tx,
    );
    let b_iface = maplet::Interface::<thincan::NoopRawMutex, _, _, 16, 512, 4>::new(
        b_node.clone(),
        &mut b_tx,
    );

    let a_bundles = maplet::Bundles::new(&a_iface);
    let b_bundles = maplet::Bundles::new(&b_iface);
    let a_ingest = a_iface.bus();
    let b_ingest = b_iface.bus();

    let stop = Arc::new(AtomicBool::new(false));

    let stop_a = stop.clone();
    let pump_a = async move {
        while !stop_a.load(Ordering::Relaxed) {
            for (from, payload) in a_pump.drain_incoming() {
                a_ingest.ingest(from, &payload).await.unwrap();
            }
            tokio::task::yield_now().await;
        }
        for (from, payload) in a_pump.drain_incoming() {
            a_ingest.ingest(from, &payload).await.unwrap();
        }
    };

    let stop_b = stop.clone();
    let pump_b = async move {
        while !stop_b.load(Ordering::Relaxed) {
            for (from, payload) in b_pump.drain_incoming() {
                b_ingest.ingest(from, &payload).await.unwrap();
            }
            tokio::task::yield_now().await;
        }
        for (from, payload) in b_pump.drain_incoming() {
            b_ingest.ingest(from, &payload).await.unwrap();
        }
    };

    let payload = b"embassy-no-alloc-path".to_vec();

    let recv_task = async move {
        let mut store = MemoryStore::default();
        let result = b_bundles
            .file_transfer
            .recv_file_no_alloc::<_, MAX_METADATA>(
                0x31,
                &mut store,
                deadline(Duration::from_millis(100)),
                thincan_file_transfer::ReceiverConfig {
                    max_chunk_size: MAX_CHUNK as u32,
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        (result, store.bytes)
    };

    let send_task = async move {
        a_bundles
            .file_transfer
            .send_file(
                0x32,
                &payload,
                deadline(Duration::from_millis(100)),
                thincan_file_transfer::SendConfig {
                    sender_max_chunk_size: 8,
                    window_chunks: 2,
                    max_retries: 3,
                    ack_timeout: core::time::Duration::from_secs(1),
                    ..Default::default()
                },
            )
            .await
            .unwrap()
    };

    let transfer = async {
        let (send_out, (recv_out, recv_bytes)) = tokio::join!(send_task, recv_task);
        stop.store(true, Ordering::Relaxed);
        (send_out, recv_out, recv_bytes)
    };

    let (_, _, (send_out, recv_out, recv_bytes)) = tokio::join!(pump_a, pump_b, transfer);

    assert_eq!(send_out.total_len, recv_bytes.len());
    assert_eq!(recv_out.received_len as usize, recv_bytes.len());
    assert!(recv_out.metadata.is_empty());
    assert_eq!(recv_bytes.as_slice(), b"embassy-no-alloc-path");
}
