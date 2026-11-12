#![cfg(feature = "tokio")]

mod support;
use support::deadline;

use core::time::Duration;
use std::sync::{Arc, Mutex};

use can_isotp_interface::{Destination, IsoTpAsyncWrite, TransmitError};

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

#[derive(Debug, Default, Clone)]
struct CountingNode {
    sent_ids: Arc<Mutex<Vec<u16>>>,
}

impl IsoTpAsyncWrite for CountingNode {
    type Error = ();

    type Deadline = tokio::time::Instant;
    async fn send(
        &mut self,
        _destination: Destination,
        payload: &[u8],
        _timeout: tokio::time::Instant,
    ) -> Result<(), TransmitError<Self::Error>> {
        if payload.len() >= 2 {
            let id = u16::from_le_bytes([payload[0], payload[1]]);
            self.sent_ids.lock().unwrap().push(id);
        }
        Ok(())
    }
}

#[tokio::test]
async fn async_sender_backpressures_when_ack_progress_stalls() {
    let sent_ids = Arc::new(Mutex::new(Vec::new()));
    let node = CountingNode {
        sent_ids: sent_ids.clone(),
    };

    let mut tx_buf = [0u8; 256];
    let iface =
        maplet::Interface::<thincan::NoopRawMutex, _, _, 8, 256, 4>::new(node.clone(), &mut tx_buf);
    let bundles = maplet::Bundles::new(&iface);
    let ingress = iface.bus();

    let transfer_id = 1u32;
    let chunk_size = 8usize;
    let total_len = 80usize;
    let bytes = vec![0xA5u8; total_len];

    let mut enc_buf = [0u8; 256];
    let mut enc = maplet::Interface::<thincan::NoopRawMutex, _, _, 1, 256, 1>::new(
        (),
        enc_buf.as_mut_slice(),
    );

    let accept_wire = enc
        .encode_capnp_into::<atlas::FileAck, _>(&thincan_file_transfer::file_ack_accept::<
            atlas::Atlas,
        >(transfer_id, chunk_size as u32))
        .unwrap()
        .to_vec();
    let progress_wire = enc
        .encode_capnp_into::<atlas::FileAck, _>(&thincan_file_transfer::file_ack_progress::<
            atlas::Atlas,
        >(transfer_id, (3 * chunk_size) as u32))
        .unwrap()
        .to_vec();

    ingress.ingest(0, &accept_wire).await.unwrap();
    ingress.ingest(0, &progress_wire).await.unwrap();

    let config = thincan_file_transfer::SendConfig {
        sender_max_chunk_size: chunk_size,
        window_chunks: 4,
        max_retries: 0,
        ack_timeout: core::time::Duration::from_secs(1),
        ..Default::default()
    };

    let err = bundles
        .file_transfer
        .send_file_with_id(
            0,
            transfer_id,
            &bytes,
            deadline(Duration::from_millis(5)),
            config,
        )
        .await
        .unwrap_err();
    assert!(err.is_timeout());

    let sent = sent_ids.lock().unwrap();
    let chunk_sends = sent
        .iter()
        .filter(|&&id| id == <atlas::FileChunk as thincan::Message>::ID)
        .count();

    // Last acked offset is 3 chunks; sender can send at most `window_chunks` beyond that.
    assert_eq!(chunk_sends, 3 + config.window_chunks);
}
