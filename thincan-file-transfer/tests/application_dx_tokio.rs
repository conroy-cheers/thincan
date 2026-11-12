#![cfg(feature = "tokio")]

mod support;
use support::{Faulty, MemoryStore, PipeEnd, deadline, is_complete};

use core::time::Duration;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use can_isotp_interface::IsoTpAsyncWrite;

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
async fn application_dx_end_to_end_tokio_bundle_api() {
    let (a_node, b_node) = PipeEnd::pair(0x11, 0x22);
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

    let payload = b"the quick brown fox jumps over the lazy dog".to_vec();

    let recv_task = async move {
        let mut store = MemoryStore::default();
        let result = b_bundles
            .file_transfer
            .recv_file(
                0x11,
                &mut store,
                deadline(Duration::from_millis(100)),
                thincan_file_transfer::ReceiverConfig {
                    max_chunk_size: 32,
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
                0x22,
                &payload,
                deadline(Duration::from_millis(100)),
                thincan_file_transfer::SendConfig {
                    sender_max_chunk_size: 16,
                    window_chunks: 3,
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
    assert_eq!(
        recv_bytes.as_slice(),
        b"the quick brown fox jumps over the lazy dog"
    );
}

struct Outcome {
    sent: Result<thincan_file_transfer::SendFileResult, String>,
    send_elapsed: Duration,
    received: Result<Vec<u8>, String>,
    receive_elapsed: Duration,
}

/// One transfer from A to B over writers that may drop or delay messages.
async fn transfer_once<WA, WB>(
    a_writer: WA,
    b_writer: WB,
    (a_node, b_node): (PipeEnd, PipeEnd),
    payload: &[u8],
    send_config: thincan_file_transfer::SendConfig,
    receiver: thincan_file_transfer::ReceiverConfig,
) -> Outcome
where
    WA: IsoTpAsyncWrite<Error = (), Deadline = tokio::time::Instant>,
    WB: IsoTpAsyncWrite<Error = (), Deadline = tokio::time::Instant>,
{
    let mut a_tx = [0u8; 512];
    let mut b_tx = [0u8; 512];
    let a_iface =
        maplet::Interface::<thincan::NoopRawMutex, _, _, 16, 512, 4>::new(a_writer, &mut a_tx);
    let b_iface =
        maplet::Interface::<thincan::NoopRawMutex, _, _, 16, 512, 4>::new(b_writer, &mut b_tx);
    let a_bundles = maplet::Bundles::new(&a_iface);
    let b_bundles = maplet::Bundles::new(&b_iface);
    let (a_ingest, b_ingest) = (a_iface.bus(), b_iface.bus());
    let stop = AtomicBool::new(false);
    let pump_a = async {
        while !stop.load(Ordering::Relaxed) {
            for (from, payload) in a_node.drain_incoming() {
                let _ = a_ingest.ingest(from, &payload).await;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    };
    let pump_b = async {
        while !stop.load(Ordering::Relaxed) {
            for (from, payload) in b_node.drain_incoming() {
                let _ = b_ingest.ingest(from, &payload).await;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    };
    let receive = async {
        let started = std::time::Instant::now();
        let mut store = MemoryStore::default();
        let result = b_bundles
            .file_transfer
            .recv_file(0x11, &mut store, deadline(Duration::from_secs(5)), receiver)
            .await;
        (
            result.map(|_| store.bytes).map_err(|e| format!("{e:?}")),
            started.elapsed(),
        )
    };
    let send = async {
        let started = std::time::Instant::now();
        let result = a_bundles
            .file_transfer
            .send_file(0x22, payload, deadline(Duration::from_secs(5)), send_config)
            .await;
        (result.map_err(|e| format!("{e:?}")), started.elapsed())
    };
    let transfer = async {
        let result = tokio::join!(send, receive);
        stop.store(true, Ordering::Relaxed);
        result
    };
    let (_, _, ((sent, send_elapsed), (received, receive_elapsed))) =
        tokio::join!(pump_a, pump_b, transfer);
    Outcome {
        sent,
        send_elapsed,
        received,
        receive_elapsed,
    }
}

fn send_config(ack_timeout: Duration) -> thincan_file_transfer::SendConfig {
    thincan_file_transfer::SendConfig {
        sender_max_chunk_size: 16,
        window_chunks: 2,
        max_retries: 10,
        ack_timeout,
        ..Default::default()
    }
}

fn receiver(idle_timeout: Duration) -> thincan_file_transfer::ReceiverConfig {
    thincan_file_transfer::ReceiverConfig {
        max_chunk_size: 32,
        idle_timeout: Some(idle_timeout),
    }
}

#[tokio::test(flavor = "current_thread")]
async fn receiver_gives_up_after_idle_timeout_and_its_abort_stops_the_sender() {
    let (a_node, b_node) = PipeEnd::pair(0x11, 0x22);
    let drop_chunks = Faulty {
        pipe: a_node.clone(),
        drop: |id: u16, _: &[u8]| id == 0x1002,
        chunk_delay: Duration::ZERO,
    };
    let outcome = transfer_once(
        drop_chunks,
        b_node.clone(),
        (a_node, b_node),
        b"chunks never arrive",
        // Alone, the sender would retry for 10 x 200 ms.
        send_config(Duration::from_millis(200)),
        receiver(Duration::from_millis(50)),
    )
    .await;
    assert!(
        outcome.received.as_ref().unwrap_err().contains("Timeout"),
        "{:?}",
        outcome.received
    );
    assert!(outcome.receive_elapsed < Duration::from_secs(1));
    assert!(outcome.sent.is_err());
    assert!(outcome.send_elapsed < Duration::from_secs(1));
}

#[tokio::test(flavor = "current_thread")]
async fn idle_timeout_restarts_with_every_chunk() {
    let (a_node, b_node) = PipeEnd::pair(0x11, 0x22);
    let slow_chunks = Faulty {
        pipe: a_node.clone(),
        drop: |_: u16, _: &[u8]| false,
        chunk_delay: Duration::from_millis(25),
    };
    let payload = [7u8; 160]; // 10 chunks: ~250 ms in total, 25 ms apart.
    let outcome = transfer_once(
        slow_chunks,
        b_node.clone(),
        (a_node, b_node),
        &payload,
        send_config(Duration::from_millis(500)),
        receiver(Duration::from_millis(100)),
    )
    .await;
    assert_eq!(outcome.received.unwrap(), payload);
    assert!(outcome.receive_elapsed > Duration::from_millis(100));
    assert_eq!(outcome.sent.unwrap().retries, 0);
}

#[tokio::test(flavor = "current_thread")]
async fn lost_complete_is_answered_again_and_confirmation_ends_the_linger() {
    let (a_node, b_node) = PipeEnd::pair(0x11, 0x22);
    let completes = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let drop_first_complete = Faulty {
        pipe: b_node.clone(),
        drop: {
            let completes = completes.clone();
            move |id: u16, body: &[u8]| {
                is_complete(id, body) && completes.fetch_add(1, Ordering::SeqCst) == 0
            }
        },
        chunk_delay: Duration::ZERO,
    };
    let payload = [5u8; 40];
    let outcome = transfer_once(
        a_node.clone(),
        drop_first_complete,
        (a_node, b_node),
        &payload,
        send_config(Duration::from_millis(50)),
        receiver(Duration::from_secs(2)),
    )
    .await;
    assert_eq!(outcome.received.unwrap(), payload);
    assert!(outcome.sent.unwrap().retries >= 1);
    assert_eq!(completes.load(Ordering::SeqCst), 2, "Complete sent again");
    // The sender's confirmation, not the 2 s idle timeout, ended the linger.
    assert!(outcome.receive_elapsed < Duration::from_secs(1));
}

#[tokio::test(flavor = "current_thread")]
async fn confirmation_ends_the_linger_promptly_after_a_clean_transfer() {
    let (a_node, b_node) = PipeEnd::pair(0x11, 0x22);
    let payload = [6u8; 40];
    let outcome = transfer_once(
        a_node.clone(),
        b_node.clone(),
        (a_node, b_node),
        &payload,
        send_config(Duration::from_millis(500)),
        receiver(Duration::from_secs(2)),
    )
    .await;
    assert_eq!(outcome.received.unwrap(), payload);
    assert!(outcome.receive_elapsed < Duration::from_millis(500));
}

#[tokio::test(flavor = "current_thread")]
async fn lost_offer_is_resent_after_the_offer_retry_interval() {
    let (a_node, b_node) = PipeEnd::pair(0x11, 0x22);
    let offers = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let drop_first_offer = Faulty {
        pipe: a_node.clone(),
        drop: {
            let offers = offers.clone();
            move |id: u16, _: &[u8]| id == 0x1001 && offers.fetch_add(1, Ordering::SeqCst) == 0
        },
        chunk_delay: Duration::ZERO,
    };
    let mut config = send_config(Duration::from_millis(500));
    config.offer_retry_interval = Some(Duration::from_millis(50));
    let payload = [3u8; 40];
    let outcome = transfer_once(
        drop_first_offer,
        b_node.clone(),
        (a_node, b_node),
        &payload,
        config,
        receiver(Duration::from_secs(1)),
    )
    .await;
    assert_eq!(outcome.received.unwrap(), payload);
    assert!(outcome.sent.is_ok());
    assert_eq!(offers.load(Ordering::SeqCst), 2);
    assert!(outcome.send_elapsed < Duration::from_secs(1));
}
