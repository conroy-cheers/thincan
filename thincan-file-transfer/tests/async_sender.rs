#![cfg(feature = "tokio")]

use core::time::Duration;
use std::sync::{Arc, Mutex};

use can_isotp_interface::{
    IsoTpAsyncEndpoint, RecvControl, RecvError, RecvMeta, RecvStatus, SendError,
};

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

#[derive(Debug, Default)]
struct CountingNode {
    sent_ids: Arc<Mutex<Vec<u16>>>,
}

impl IsoTpAsyncEndpoint for CountingNode {
    type Error = ();

    async fn send_to(
        &mut self,
        _to: u8,
        payload: &[u8],
        _timeout: Duration,
    ) -> Result<(), SendError<Self::Error>> {
        if payload.len() >= 2 {
            let id = u16::from_le_bytes([payload[0], payload[1]]);
            self.sent_ids.lock().unwrap().push(id);
        }
        Ok(())
    }

    async fn send_functional_to(
        &mut self,
        _functional_to: u8,
        payload: &[u8],
        timeout: Duration,
    ) -> Result<(), SendError<Self::Error>> {
        self.send_to(0, payload, timeout).await
    }

    async fn recv_one<Cb>(
        &mut self,
        _timeout: Duration,
        _on_payload: Cb,
    ) -> Result<RecvStatus, RecvError<Self::Error>>
    where
        Cb: FnMut(RecvMeta, &[u8]) -> Result<RecvControl, Self::Error>,
    {
        Ok(RecvStatus::TimedOut)
    }
}

#[derive(Debug, Default)]
struct MemoryStore {
    bytes: Vec<u8>,
}

impl thincan_file_transfer::AsyncFileStore for MemoryStore {
    type Error = ();
    type WriteHandle = ();

    async fn begin_write(
        &mut self,
        _transfer_id: u32,
        total_len: u32,
    ) -> Result<Self::WriteHandle, Self::Error> {
        self.bytes.clear();
        self.bytes.resize(total_len as usize, 0);
        Ok(())
    }

    async fn write_at(
        &mut self,
        _handle: &mut Self::WriteHandle,
        offset: u32,
        bytes: &[u8],
    ) -> Result<(), Self::Error> {
        let offset = offset as usize;
        let end = offset + bytes.len();
        self.bytes[offset..end].copy_from_slice(bytes);
        Ok(())
    }

    async fn commit(&mut self, _handle: Self::WriteHandle) -> Result<(), Self::Error> {
        Ok(())
    }

    async fn abort(&mut self, _handle: Self::WriteHandle) {}
}

#[tokio::test]
async fn async_sender_completes_with_preingested_acks() {
    let sent_ids = Arc::new(Mutex::new(Vec::new()));
    let node = CountingNode {
        sent_ids: sent_ids.clone(),
    };

    let mut tx_buf = [0u8; 256];
    let iface = maplet::Interface::<thincan::NoopRawMutex, _, _, 4, 256, 2>::new(node, &mut tx_buf);
    let mut bundles = maplet::Bundles::new(&iface);
    let ingress = iface.handle();

    let transfer_id = 1u32;
    let mut enc_buf = [0u8; 256];
    let mut enc = maplet::Interface::<thincan::NoopRawMutex, _, _, 1, 256, 1>::new(
        (),
        enc_buf.as_mut_slice(),
    );

    let accept_wire = enc
        .encode_capnp_into::<atlas::FileAck, _>(&thincan_file_transfer::file_ack_accept::<
            atlas::Atlas,
        >(transfer_id, 8))
        .unwrap()
        .to_vec();
    let complete_wire = enc
        .encode_capnp_into::<atlas::FileAck, _>(
            &thincan_file_transfer::FileAckValue::<atlas::Atlas>::new(
                transfer_id,
                thincan_file_transfer::schema::FileAckKind::Complete,
                20,
                0,
                thincan_file_transfer::schema::FileAckError::None,
            ),
        )
        .unwrap()
        .to_vec();

    ingress.ingest(0, &accept_wire).await.unwrap();
    ingress.ingest(0, &complete_wire).await.unwrap();

    let config = thincan_file_transfer::SendConfig::default();
    let bytes = [0xABu8; 20];
    let out = bundles
        .file_transfer
        .send_file_with_id(0, transfer_id, &bytes, Duration::from_millis(50), config)
        .await
        .unwrap();

    assert_eq!(out.transfer_id, transfer_id);
    assert_eq!(out.total_len, 20);
    assert_eq!(out.chunk_size, 8);
    assert_eq!(out.chunks_sent, 3);
    assert_eq!(out.retries, 0);

    let sent = sent_ids.lock().unwrap();
    let reqs = sent
        .iter()
        .filter(|&&id| id == <atlas::FileReq as thincan::Message>::ID)
        .count();
    let chunks = sent
        .iter()
        .filter(|&&id| id == <atlas::FileChunk as thincan::Message>::ID)
        .count();
    assert_eq!(reqs, 1);
    assert_eq!(chunks, 3);
}

#[tokio::test]
async fn recv_file_writes_store_and_sends_acks() {
    let sent_ids = Arc::new(Mutex::new(Vec::new()));
    let node = CountingNode {
        sent_ids: sent_ids.clone(),
    };

    let mut tx_buf = [0u8; 256];
    let iface = maplet::Interface::<thincan::NoopRawMutex, _, _, 8, 256, 4>::new(node, &mut tx_buf);
    let bundles = maplet::Bundles::new(&iface);
    let ingress = iface.handle();

    let mut enc_buf = [0u8; 256];
    let mut enc =
        maplet::Interface::<thincan::NoopRawMutex, _, _, 8, 256, 4>::new((), &mut enc_buf);
    let req = enc
        .encode_capnp_into::<atlas::FileReq, _>(&thincan_file_transfer::file_offer::<atlas::Atlas>(
            9, 11, 8, b"meta",
        ))
        .unwrap()
        .to_vec();
    let chunk_a = enc
        .encode_capnp_into::<atlas::FileChunk, _>(
            &thincan_file_transfer::file_chunk::<atlas::Atlas>(9, 0, b"hello "),
        )
        .unwrap()
        .to_vec();
    let chunk_b = enc
        .encode_capnp_into::<atlas::FileChunk, _>(
            &thincan_file_transfer::file_chunk::<atlas::Atlas>(9, 6, b"world"),
        )
        .unwrap()
        .to_vec();

    ingress.ingest(0, &req).await.unwrap();
    ingress.ingest(0, &chunk_a).await.unwrap();
    ingress.ingest(0, &chunk_b).await.unwrap();

    let mut store = MemoryStore::default();
    let out = bundles
        .file_transfer
        .recv_file(
            0,
            &mut store,
            Duration::from_millis(50),
            thincan_file_transfer::ReceiverConfig { max_chunk_size: 64 },
        )
        .await
        .unwrap();

    assert_eq!(out.transfer_id, 9);
    assert_eq!(out.total_len, 11);
    assert_eq!(out.received_len, 11);
    assert_eq!(out.sender_max_chunk_size, 8);
    assert_eq!(out.negotiated_chunk_size, 8);
    assert_eq!(out.metadata.as_slice(), b"meta");
    assert_eq!(store.bytes, b"hello world");

    let sent = sent_ids.lock().unwrap();
    let acks = sent
        .iter()
        .filter(|&&id| id == <atlas::FileAck as thincan::Message>::ID)
        .count();
    assert!(acks >= 2);
}
