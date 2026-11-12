#![cfg(feature = "tokio")]
//! Receiver TIME_WAIT: `serve` answering for finished transfers, and repeated
//! offers being recognised. Interfaces use `MAX_WAITERS = 2` throughout, so the
//! tests also cover `serve` sharing a small waiter budget with `recv_file`.

mod support;
use support::{Faulty, MemoryStore, PipeEnd, is_complete};

use can_isotp_interface::{Destination, IsoTpAsyncWrite, TransmitError};

use core::time::Duration;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Instant;

use embassy_futures::select::{Either, select};

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

const A: u8 = 0x11;
const B: u8 = 0x22;
const CHUNK: u16 = 0x1002;

/// Drops the first `n` messages that `select` matches, counting all matches.
fn drop_first(
    n: usize,
    select: impl Fn(u16, &[u8]) -> bool,
) -> (impl Fn(u16, &[u8]) -> bool, Arc<AtomicUsize>) {
    let seen = Arc::new(AtomicUsize::new(0));
    let counter = seen.clone();
    let drop =
        move |id: u16, body: &[u8]| select(id, body) && counter.fetch_add(1, Ordering::SeqCst) < n;
    (drop, seen)
}

/// The sender's confirmation of `Complete`: an empty chunk.
fn is_confirmation(id: u16, body: &[u8]) -> bool {
    id == CHUNK
        && thincan::CapnpTyped::<thincan_file_transfer::schema::file_chunk::Owned>::new(body)
            .with_root(Default::default(), |root| {
                root.get_data().map_or(0, |data| data.len()) == 0
            })
            .unwrap()
}

fn deadline(ms: u64) -> tokio::time::Instant {
    support::deadline(Duration::from_millis(ms))
}

fn send_config(ack_ms: u64) -> thincan_file_transfer::SendConfig {
    thincan_file_transfer::SendConfig {
        sender_max_chunk_size: 16,
        window_chunks: 2,
        max_retries: 10,
        ack_timeout: Duration::from_millis(ack_ms),
        offer_retry_interval: Some(Duration::from_millis(50)),
    }
}

fn receiver_config(idle_ms: u64) -> thincan_file_transfer::ReceiverConfig {
    thincan_file_transfer::ReceiverConfig {
        max_chunk_size: 32,
        idle_timeout: Some(Duration::from_millis(idle_ms)),
    }
}

/// Run `$body` with bundles `$a` and `$b` (and interfaces `$a_iface`, `$b_iface`)
/// connected through `$a_writer`/`$b_writer`, with both receive pumps running.
macro_rules! session {
    (
        $nodes:expr, $a_writer:expr, $b_writer:expr,
        |$a:ident, $b:ident, $a_iface:ident, $b_iface:ident| $body:block
    ) => {{
        let (a_node, b_node): (PipeEnd, PipeEnd) = $nodes;
        let mut a_tx = [0u8; 512];
        let mut b_tx = [0u8; 512];
        let $a_iface =
            maplet::Interface::<thincan::NoopRawMutex, _, _, 16, 512, 2>::new($a_writer, &mut a_tx);
        let $b_iface =
            maplet::Interface::<thincan::NoopRawMutex, _, _, 16, 512, 2>::new($b_writer, &mut b_tx);
        let $a = maplet::Bundles::new(&$a_iface);
        let $b = maplet::Bundles::new(&$b_iface);
        let (a_ingest, b_ingest) = ($a_iface.bus(), $b_iface.bus());
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
        let body = async {
            let out = $body;
            stop.store(true, Ordering::Relaxed);
            out
        };
        tokio::join!(pump_a, pump_b, body).2
    }};
}

/// Run `scenario` while `serve` answers for B's finished transfers.
async fn with_serve<T>(
    serve: impl core::future::Future<Output = Result<(), thincan_file_transfer::ServeError>>,
    scenario: impl core::future::Future<Output = T>,
) -> T {
    match select(serve, scenario).await {
        Either::First(result) => panic!("serve stopped early: {result:?}"),
        Either::Second(out) => out,
    }
}

#[tokio::test(flavor = "current_thread")]
async fn lost_confirmation_does_not_delay_a_serving_receiver() {
    let (a_node, b_node) = PipeEnd::pair(A, B);
    let (drop_confirmation, confirmations) = drop_first(1, is_confirmation);
    let a_writer = Faulty {
        pipe: a_node.clone(),
        drop: drop_confirmation,
        chunk_delay: Duration::ZERO,
    };
    session!(
        (a_node, b_node.clone()),
        a_writer,
        b_node,
        |a, b, _ai, _bi| {
            with_serve(b.file_transfer.serve(), async {
                let file = [1u8; 40];
                let started = Instant::now();
                let (sent, received) = tokio::join!(
                    a.file_transfer
                        .send_file(B, &file, deadline(2_000), send_config(50)),
                    async {
                        let mut store = MemoryStore::default();
                        let result = b
                            .file_transfer
                            .recv_file(A, &mut store, deadline(2_000), receiver_config(2_000))
                            .await;
                        (result, store.bytes, started.elapsed())
                    }
                );
                sent.unwrap();
                let (result, bytes, receive_elapsed) = received;
                result.unwrap();
                assert_eq!(bytes, file);
                assert_eq!(
                    confirmations.load(Ordering::SeqCst),
                    1,
                    "confirmation dropped"
                );
                // Without `serve`, B would wait its 2 s idle timeout for the confirmation.
                assert!(
                    receive_elapsed < Duration::from_millis(500),
                    "{receive_elapsed:?}"
                );

                // The echo succeeds within a deadline shorter than that idle timeout.
                let echo = [2u8; 40];
                let (sent, received) = tokio::join!(
                    b.file_transfer
                        .send_file(A, &echo, deadline(500), send_config(50)),
                    async {
                        let mut store = MemoryStore::default();
                        a.file_transfer
                            .recv_file(B, &mut store, deadline(500), receiver_config(2_000))
                            .await
                            .map(|_| store.bytes)
                    }
                );
                sent.unwrap();
                assert_eq!(received.unwrap(), echo);
            })
            .await
        }
    );
}

#[tokio::test(flavor = "current_thread")]
async fn serve_answers_a_lost_complete_after_recv_file_returned() {
    let (a_node, b_node) = PipeEnd::pair(A, B);
    let (drop_complete, completes) = drop_first(1, is_complete);
    let b_writer = Faulty {
        pipe: b_node.clone(),
        drop: drop_complete,
        chunk_delay: Duration::ZERO,
    };
    session!(
        (a_node.clone(), b_node),
        a_node,
        b_writer,
        |a, b, _ai, _bi| {
            with_serve(b.file_transfer.serve(), async {
                let file = [3u8; 40];
                let started = Instant::now();
                let (sent, received) = tokio::join!(
                    async {
                        let result = a
                            .file_transfer
                            .send_file(B, &file, deadline(2_000), send_config(50))
                            .await;
                        (result, started.elapsed())
                    },
                    async {
                        let mut store = MemoryStore::default();
                        let result = b
                            .file_transfer
                            .recv_file(A, &mut store, deadline(2_000), receiver_config(2_000))
                            .await;
                        (result.map(|_| store.bytes), started.elapsed())
                    }
                );
                let (received, receive_elapsed) = received;
                assert_eq!(received.unwrap(), file);
                let (sent, send_elapsed) = sent;
                let sent = sent.unwrap();
                assert!(sent.retries >= 1);
                assert_eq!(completes.load(Ordering::SeqCst), 2, "Complete sent again");
                // B returned before A recovered: the second `Complete` came from `serve`.
                assert!(
                    receive_elapsed < send_elapsed,
                    "{receive_elapsed:?} {send_elapsed:?}"
                );
            })
            .await
        }
    );
}

/// Two transfers from A under the same id, as from a sender that restarted.
async fn receive_twice_under_one_id(first: &[u8], second: &[u8], time_wait_ms: u64) -> Duration {
    let (a_node, b_node) = PipeEnd::pair(A, B);
    session!(
        (a_node.clone(), b_node.clone()),
        a_node,
        b_node,
        |a, b, _ai, _bi| {
            b.file_transfer
                .set_time_wait(Duration::from_millis(time_wait_ms));
            let receive = || async {
                let mut store = MemoryStore::default();
                b.file_transfer
                    .recv_file(A, &mut store, deadline(3_000), receiver_config(2_000))
                    .await
                    .map(|_| store.bytes)
            };
            let (sent, received) = tokio::join!(
                a.file_transfer
                    .send_file_with_id(B, 7, first, deadline(2_000), send_config(50)),
                receive()
            );
            sent.unwrap();
            assert_eq!(received.unwrap(), first);

            let started = Instant::now();
            let (sent, received) = tokio::join!(
                a.file_transfer
                    .send_file_with_id(B, 7, second, deadline(2_000), send_config(50)),
                receive()
            );
            sent.unwrap();
            assert_eq!(received.unwrap(), second);
            started.elapsed()
        }
    )
}

#[tokio::test(flavor = "current_thread")]
async fn restarted_sender_reusing_an_id_for_a_different_file_is_accepted_at_once() {
    let elapsed = receive_twice_under_one_id(&[4; 40], &[5; 40], 2_000).await;
    assert!(elapsed < Duration::from_millis(300), "{elapsed:?}");
}

#[tokio::test(flavor = "current_thread")]
async fn identical_offer_under_a_reused_id_waits_for_time_wait_to_expire() {
    // An exact repeat is indistinguishable from a retransmitted offer, so it is
    // dropped until the entry expires; the sender's offer retries then get in.
    let elapsed = receive_twice_under_one_id(&[6; 40], &[6; 40], 300).await;
    assert!(elapsed >= Duration::from_millis(200), "{elapsed:?}");
    assert!(elapsed < Duration::from_secs(1), "{elapsed:?}");
}

#[tokio::test(flavor = "current_thread")]
async fn second_serve_is_refused_and_serve_returns_when_the_interface_closes() {
    let (a_node, b_node) = PipeEnd::pair(A, B);
    session!(
        (a_node.clone(), b_node.clone()),
        a_node,
        b_node,
        |_a, b, _ai, b_iface| {
            let first = b.file_transfer.serve();
            let check = async {
                tokio::task::yield_now().await;
                // Bounded, so a second `serve` that is not refused fails instead of hanging.
                let second =
                    tokio::time::timeout(Duration::from_secs(1), b.file_transfer.serve()).await;
                assert!(matches!(
                    second,
                    Ok(Err(thincan_file_transfer::ServeError::AlreadyServing))
                ));
                b_iface.close();
            };
            let (served, ()) = tokio::join!(first, check);
            served.unwrap();
        }
    );
}

/// Rejects the first `Complete` it is asked to send.
struct RejectFirstComplete {
    pipe: PipeEnd,
    rejected: bool,
}

impl IsoTpAsyncWrite for RejectFirstComplete {
    type Error = ();
    type Deadline = tokio::time::Instant;
    async fn send(
        &mut self,
        destination: Destination,
        payload: &[u8],
        deadline: tokio::time::Instant,
    ) -> Result<(), TransmitError<()>> {
        let id = u16::from_le_bytes([payload[0], payload[1]]);
        if !self.rejected && is_complete(id, &payload[2..]) {
            self.rejected = true;
            return Err(TransmitError::Rejected(()));
        }
        self.pipe.send(destination, payload, deadline).await
    }
}

/// The file is committed before `Complete` is sent, so failing to send it does
/// not fail the receive; the sender's retransmission is answered by `serve`.
#[tokio::test(flavor = "current_thread")]
async fn failing_to_send_complete_still_completes_the_receive() {
    let (a_node, b_node) = PipeEnd::pair(A, B);
    let b_writer = RejectFirstComplete {
        pipe: b_node.clone(),
        rejected: false,
    };
    session!(
        (a_node.clone(), b_node),
        a_node,
        b_writer,
        |a, b, _ai, _bi| {
            with_serve(b.file_transfer.serve(), async {
                let file = [8u8; 40];
                let (sent, received) = tokio::join!(
                    a.file_transfer
                        .send_file(B, &file, deadline(2_000), send_config(50)),
                    async {
                        let mut store = MemoryStore::default();
                        b.file_transfer
                            .recv_file(A, &mut store, deadline(2_000), receiver_config(2_000))
                            .await
                            .map(|_| store.bytes)
                    }
                );
                assert_eq!(received.expect("receive succeeds"), file);
                assert!(sent.expect("sender recovers").retries >= 1);
            })
            .await
        }
    );
}

/// With TIME_WAIT disabled nothing is recorded for `serve` to answer from, so
/// `recv_file` must wait for the confirmation itself even while `serve` runs.
#[tokio::test(flavor = "current_thread")]
async fn without_time_wait_recv_file_answers_a_lost_complete_itself() {
    let (a_node, b_node) = PipeEnd::pair(A, B);
    let (drop_complete, completes) = drop_first(1, is_complete);
    let b_writer = Faulty {
        pipe: b_node.clone(),
        drop: drop_complete,
        chunk_delay: Duration::ZERO,
    };
    session!(
        (a_node.clone(), b_node),
        a_node,
        b_writer,
        |a, b, _ai, _bi| {
            b.file_transfer.set_time_wait(Duration::ZERO);
            with_serve(b.file_transfer.serve(), async {
                let file = [9u8; 40];
                let (sent, received) = tokio::join!(
                    a.file_transfer
                        .send_file(B, &file, deadline(2_000), send_config(50)),
                    async {
                        let mut store = MemoryStore::default();
                        b.file_transfer
                            .recv_file(A, &mut store, deadline(2_000), receiver_config(2_000))
                            .await
                            .map(|_| store.bytes)
                    }
                );
                assert_eq!(received.expect("receive"), file);
                assert!(sent.expect("sender recovers").retries >= 1);
                assert_eq!(completes.load(Ordering::SeqCst), 2, "Complete sent again");
            })
            .await
        }
    );
}
