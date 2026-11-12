#![cfg(feature = "tokio")]

mod support;
use support::{MemoryStore, deadline};

use core::time::Duration;
use std::sync::{Arc, Mutex};

use can_isotp_interface::{Destination, IsoTpAsyncWrite, TransmitError};
use sha2::{Digest, Sha256};

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

struct FailingNode;

impl IsoTpAsyncWrite for FailingNode {
    type Error = std::io::Error;
    type Deadline = tokio::time::Instant;

    async fn send(
        &mut self,
        _destination: Destination,
        _payload: &[u8],
        _deadline: Self::Deadline,
    ) -> Result<(), TransmitError<Self::Error>> {
        Err(TransmitError::Fatal(std::io::Error::new(
            std::io::ErrorKind::ConnectionReset,
            "original transport failure",
        )))
    }
}

#[tokio::test]
async fn async_sender_preserves_noncloneable_transport_cause() {
    let mut tx_buf = [0u8; 256];
    let iface =
        maplet::Interface::<thincan::NoopRawMutex, _, _, 4, 256, 2>::new(FailingNode, &mut tx_buf);
    let bundles = maplet::Bundles::new(&iface);

    let error = bundles
        .file_transfer
        .send_file_with_id(
            0,
            1,
            b"payload",
            deadline(Duration::from_secs(1)),
            thincan_file_transfer::SendConfig::default(),
        )
        .await
        .unwrap_err();

    assert!(iface.is_closed());
    assert!(!error.is_timeout());
    let thincan_file_transfer::SendFileError::Send(thincan::SendMessageError::Transport(
        TransmitError::Fatal(cause),
    )) = &error
    else {
        panic!("unexpected error: {error:?}");
    };
    assert_eq!(cause.kind(), std::io::ErrorKind::ConnectionReset);
    assert_eq!(cause.to_string(), "original transport failure");

    let mut source = std::error::Error::source(&error).unwrap();
    while let Some(next) = source.source() {
        source = next;
    }
    assert!(core::ptr::eq(
        source.downcast_ref::<std::io::Error>().unwrap(),
        cause,
    ));
}

#[tokio::test]
async fn recv_file_writes_store_and_sends_acks() {
    let sent_ids = Arc::new(Mutex::new(Vec::new()));
    let node = CountingNode {
        sent_ids: sent_ids.clone(),
    };

    let mut tx_buf = [0u8; 256];
    let iface =
        maplet::Interface::<thincan::NoopRawMutex, _, _, 8, 256, 4>::new(node.clone(), &mut tx_buf);
    let bundles = maplet::Bundles::new(&iface);
    let ingress = iface.bus();

    let mut enc_buf = [0u8; 256];
    let mut enc =
        maplet::Interface::<thincan::NoopRawMutex, _, _, 8, 256, 4>::new((), &mut enc_buf);
    let req = enc
        .encode_capnp_into::<atlas::FileReq, _>(&thincan_file_transfer::file_offer::<atlas::Atlas>(
            9,
            11,
            8,
            b"meta",
            thincan_file_transfer::schema::FileHashAlgo::Sha256,
            Sha256::digest(b"hello world").as_slice(),
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
            deadline(Duration::from_millis(50)),
            thincan_file_transfer::ReceiverConfig {
                max_chunk_size: 64,
                ..Default::default()
            },
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

/// Receive "hello world" from a sender that promises to confirm `Complete`,
/// using pre-ingested messages with nothing arriving after the last chunk (so
/// the confirmation is lost), and time the call.
async fn receive_without_confirmation() -> Duration {
    let node = CountingNode::default();
    let mut tx_buf = [0u8; 256];
    let iface = maplet::Interface::<thincan::NoopRawMutex, _, _, 8, 256, 4>::new(node, &mut tx_buf);
    let bundles = maplet::Bundles::new(&iface);
    let ingress = iface.bus();

    let mut enc_buf = [0u8; 256];
    let mut enc =
        maplet::Interface::<thincan::NoopRawMutex, _, _, 8, 256, 4>::new((), &mut enc_buf);
    let hash = Sha256::digest(b"hello world");
    let offer = thincan_file_transfer::file_offer::<atlas::Atlas>(
        9,
        11,
        8,
        b"",
        thincan_file_transfer::schema::FileHashAlgo::Sha256,
        hash.as_slice(),
    )
    .confirming_complete();
    let req = enc
        .encode_capnp_into::<atlas::FileReq, _>(&offer)
        .unwrap()
        .to_vec();
    ingress.ingest(0, &req).await.unwrap();
    for (offset, data) in [(0, b"hello ".as_slice()), (6, b"world")] {
        let chunk = enc
            .encode_capnp_into::<atlas::FileChunk, _>(&thincan_file_transfer::file_chunk::<
                atlas::Atlas,
            >(9, offset, data))
            .unwrap()
            .to_vec();
        ingress.ingest(0, &chunk).await.unwrap();
    }

    let started = std::time::Instant::now();
    let mut store = MemoryStore::default();
    bundles
        .file_transfer
        .recv_file(
            0,
            &mut store,
            deadline(Duration::from_secs(5)),
            thincan_file_transfer::ReceiverConfig {
                max_chunk_size: 64,
                idle_timeout: Some(Duration::from_millis(200)),
            },
        )
        .await
        .unwrap();
    assert_eq!(store.bytes, b"hello world");
    started.elapsed()
}

#[tokio::test]
async fn receiver_lingers_until_idle_timeout_when_the_confirmation_is_lost() {
    let elapsed = receive_without_confirmation().await;
    assert!(elapsed >= Duration::from_millis(200), "{elapsed:?}");
    assert!(elapsed < Duration::from_secs(1), "{elapsed:?}");
}

#[test]
fn complete_confirmation_flag_does_not_change_the_offer_size() {
    let mut enc_buf = [0u8; 256];
    let mut enc =
        maplet::Interface::<thincan::NoopRawMutex, _, _, 8, 256, 4>::new((), &mut enc_buf);
    let hash = Sha256::digest(b"x");
    let offer = thincan_file_transfer::file_offer::<atlas::Atlas>(
        1,
        1,
        8,
        b"meta",
        thincan_file_transfer::schema::FileHashAlgo::Sha256,
        hash.as_slice(),
    );
    let plain = enc
        .encode_capnp_into::<atlas::FileReq, _>(&offer)
        .unwrap()
        .to_vec();
    let flagged = enc
        .encode_capnp_into::<atlas::FileReq, _>(&offer.confirming_complete())
        .unwrap()
        .to_vec();
    // The flag uses spare bits in an existing data word, so peers with tight
    // body limits see the same length; only the flag's byte differs.
    assert_eq!(plain.len(), flagged.len());
    let differing = plain.iter().zip(&flagged).filter(|(a, b)| a != b).count();
    assert_eq!(differing, 1);
}
