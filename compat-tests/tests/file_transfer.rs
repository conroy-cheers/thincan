//! File transfer between a node from the published 0.1 release and a node from
//! this workspace, end to end over a CAN bus (see the crate docs).
//!
//! Tests named `pins_...` assert a known compatibility limitation, so that a
//! change in that behaviour is noticed and the docs updated.
//!
//! Both scenarios in a test wait at a start barrier, so neither node sends
//! before the other is connected to the bus and pumping, and at a done barrier,
//! so neither node shuts down while its peer still needs it.

use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};
use thincan_compat_tests::{
    Bus, Faults, NODE_A, NODE_B, Store, e2e, file, is_complete, is_data_chunk, is_progress, new,
    old,
};
use tokio::sync::Barrier;

/// Sizes covering an empty file, a single chunk, and several windows' worth.
const SIZES: [usize; 4] = [0, 1, 5000, 9000];

/// Generous: the tests bound each failure mode with tighter, explicit timeouts.
const RECEIVE_DEADLINE: Duration = Duration::from_secs(5);

async fn new_recv(
    b: &new::Bundles<'_>,
    store: &mut Store,
    config: new::ReceiverConfig,
) -> Result<(), String> {
    b.file_transfer
        .recv_file(NODE_A, store, new::deadline(RECEIVE_DEADLINE), config)
        .await
        .map(drop)
        .map_err(|e| format!("{e:?}"))
}

async fn old_recv(
    b: &old::Bundles<'_>,
    store: &mut Store,
    timeout: Duration,
) -> Result<(), String> {
    b.file_transfer
        .recv_file(NODE_A, store, timeout, old::receiver_config())
        .await
        .map(drop)
        .map_err(|e| format!("{e:?}"))
}

// --- Plain transfers -------------------------------------------------------

#[test]
fn old_sender_to_new_receiver_back_to_back() {
    e2e(|| async {
        let bus = Bus::start();
        let (start, done) = (Barrier::new(2), Barrier::new(2));
        tokio::join!(
            old::run(bus.path(), NODE_A, Faults::none(), async |a| {
                start.wait().await;
                for (i, len) in SIZES.into_iter().enumerate() {
                    a.file_transfer
                        .send_file(
                            NODE_B,
                            &file(len, i as u8),
                            Duration::from_secs(2),
                            old::send_config(),
                        )
                        .await
                        .unwrap_or_else(|e| panic!("0.1 send of {len} bytes: {e:?}"));
                }
                done.wait().await;
            }),
            new::run(bus.path(), NODE_B, new::Options::default(), async |b| {
                start.wait().await;
                for (i, len) in SIZES.into_iter().enumerate() {
                    let mut store = Store::default();
                    new_recv(b, &mut store, new::receiver_config())
                        .await
                        .unwrap_or_else(|e| panic!("current receive of {len} bytes: {e}"));
                    assert_eq!(store.bytes, file(len, i as u8));
                }
                done.wait().await;
            }),
        );
    });
}

/// Also shows that a 0.1 receiver ignores `confirmsComplete` and discards the
/// confirmation chunk that follows each transfer.
#[test]
fn new_sender_to_old_receiver_back_to_back() {
    e2e(|| async {
        let bus = Bus::start();
        let (start, done) = (Barrier::new(2), Barrier::new(2));
        tokio::join!(
            new::run(bus.path(), NODE_A, new::Options::default(), async |a| {
                start.wait().await;
                for (i, len) in SIZES.into_iter().enumerate() {
                    a.file_transfer
                        .send_file(
                            NODE_B,
                            &file(len, i as u8),
                            new::deadline(Duration::from_secs(5)),
                            new::send_config(),
                        )
                        .await
                        .unwrap_or_else(|e| panic!("current send of {len} bytes: {e:?}"));
                }
                done.wait().await;
            }),
            old::run(bus.path(), NODE_B, Faults::none(), async |b| {
                start.wait().await;
                for (i, len) in SIZES.into_iter().enumerate() {
                    let mut store = Store::default();
                    old_recv(b, &mut store, Duration::from_secs(2))
                        .await
                        .unwrap_or_else(|e| panic!("0.1 receive of {len} bytes: {e}"));
                    assert_eq!(store.bytes, file(len, i as u8));
                }
                done.wait().await;
            }),
        );
    });
}

#[test]
fn ping_pong_between_versions() {
    e2e(|| async {
        let bus = Bus::start();
        let (start, done) = (Barrier::new(2), Barrier::new(2));
        tokio::join!(
            old::run(bus.path(), NODE_A, Faults::none(), async |a| {
                start.wait().await;
                for round in 0..3u8 {
                    let sent = file(3000, round);
                    a.file_transfer
                        .send_file(NODE_B, &sent, Duration::from_secs(2), old::send_config())
                        .await
                        .expect("0.1 send");
                    let mut store = Store::default();
                    a.file_transfer
                        .recv_file(
                            NODE_B,
                            &mut store,
                            Duration::from_secs(2),
                            old::receiver_config(),
                        )
                        .await
                        .expect("0.1 receive of the echo");
                    assert_eq!(store.bytes, sent);
                }
                done.wait().await;
            }),
            new::run(bus.path(), NODE_B, new::Options::default(), async |b| {
                start.wait().await;
                for _ in 0..3 {
                    let mut store = Store::default();
                    new_recv(b, &mut store, new::receiver_config())
                        .await
                        .expect("current receive");
                    b.file_transfer
                        .send_file(
                            NODE_A,
                            &store.bytes,
                            new::deadline(Duration::from_secs(5)),
                            new::send_config(),
                        )
                        .await
                        .expect("current echo");
                }
                done.wait().await;
            }),
        );
    });
}

// --- A lost `Complete` -----------------------------------------------------

/// The current receiver's `serve` answers a 0.1 sender that missed `Complete`.
#[test]
fn serve_answers_old_sender_that_missed_complete() {
    e2e(|| async {
        let bus = Bus::start();
        let (start, done) = (Barrier::new(2), Barrier::new(2));
        let (drop_complete, completes) = Faults::drop_first(1, is_complete);
        let sent = file(5000, 7);
        let started = Instant::now();
        let (send_elapsed, receive_elapsed) = tokio::join!(
            old::run(bus.path(), NODE_A, Faults::none(), async |a| {
                start.wait().await;
                let result = a
                    .file_transfer
                    .send_file(
                        NODE_B,
                        &sent,
                        Duration::from_millis(300),
                        old::send_config(),
                    )
                    .await;
                let elapsed = started.elapsed();
                result.expect("0.1 send recovers after its timeout");
                done.wait().await;
                elapsed
            }),
            new::run(
                bus.path(),
                NODE_B,
                new::Options {
                    faults: drop_complete,
                    serve: true
                },
                async |b| {
                    start.wait().await;
                    let mut store = Store::default();
                    new_recv(b, &mut store, new::receiver_config())
                        .await
                        .expect("current receive");
                    let elapsed = started.elapsed();
                    assert_eq!(store.bytes, sent);
                    done.wait().await;
                    elapsed
                }
            ),
        );
        assert_eq!(completes.load(Ordering::SeqCst), 2, "Complete sent again");
        // `recv_file` returned at once; `serve` sent the second `Complete` later.
        assert!(
            receive_elapsed < send_elapsed,
            "{receive_elapsed:?} {send_elapsed:?}"
        );
    });
}

/// Without `serve`, nothing answers a 0.1 sender that missed `Complete`: 0.1
/// senders never confirm, so the current receiver does not wait for them.
#[test]
fn pins_old_sender_missing_complete_fails_without_serve() {
    e2e(|| async {
        let bus = Bus::start();
        let (start, done) = (Barrier::new(2), Barrier::new(2));
        let (drop_complete, _) = Faults::drop_first(1, is_complete);
        tokio::join!(
            old::run(bus.path(), NODE_A, Faults::none(), async |a| {
                start.wait().await;
                let result = a
                    .file_transfer
                    .send_file(
                        NODE_B,
                        &file(5000, 8),
                        Duration::from_millis(200),
                        old::send_config(),
                    )
                    .await;
                assert!(result.is_err(), "0.1 sender recovered: {result:?}");
                done.wait().await;
            }),
            new::run(
                bus.path(),
                NODE_B,
                new::Options {
                    faults: drop_complete,
                    serve: false
                },
                async |b| {
                    start.wait().await;
                    let mut store = Store::default();
                    new_recv(b, &mut store, new::receiver_config())
                        .await
                        .expect("the file itself arrives");
                    assert_eq!(store.bytes, file(5000, 8));
                    done.wait().await;
                }
            ),
        );
    });
}

/// A 0.1 receiver returns as soon as it sends `Complete`, so a current sender
/// that misses it cannot recover; the file has nevertheless arrived.
#[test]
fn pins_new_sender_missing_complete_from_old_receiver_fails() {
    e2e(|| async {
        let bus = Bus::start();
        let (start, done) = (Barrier::new(2), Barrier::new(2));
        let (drop_complete, _) = Faults::drop_first(1, is_complete);
        tokio::join!(
            new::run(bus.path(), NODE_A, new::Options::default(), async |a| {
                start.wait().await;
                let config = new::SendConfig {
                    ack_timeout: Duration::from_millis(100),
                    ..new::send_config()
                };
                let result = a
                    .file_transfer
                    .send_file(
                        NODE_B,
                        &file(5000, 9),
                        new::deadline(Duration::from_secs(3)),
                        config,
                    )
                    .await;
                assert!(result.is_err(), "current sender recovered: {result:?}");
                done.wait().await;
            }),
            old::run(bus.path(), NODE_B, drop_complete, async |b| {
                start.wait().await;
                let mut store = Store::default();
                old_recv(b, &mut store, Duration::from_secs(2))
                    .await
                    .expect("the file itself arrives");
                assert_eq!(store.bytes, file(5000, 9));
                done.wait().await;
            }),
        );
    });
}

// --- Repeated offers -------------------------------------------------------

/// A 0.1 sender re-offers after each timeout while the current receiver
/// prepares storage; the repeats must not start a phantom next transfer. The
/// first file is empty, where Accept is the last message of the transfer.
#[test]
fn new_receiver_ignores_repeated_offers_from_old_sender() {
    e2e(|| async {
        let bus = Bus::start();
        let (start, done) = (Barrier::new(2), Barrier::new(2));
        tokio::join!(
            old::run(bus.path(), NODE_A, Faults::none(), async |a| {
                start.wait().await;
                for (i, len) in [0, 3000, 3000].into_iter().enumerate() {
                    a.file_transfer
                        .send_file(
                            NODE_B,
                            &file(len, i as u8),
                            Duration::from_millis(250),
                            old::send_config(),
                        )
                        .await
                        .unwrap_or_else(|e| panic!("0.1 send {i}: {e:?}"));
                }
                done.wait().await;
            }),
            new::run(bus.path(), NODE_B, new::Options::default(), async |b| {
                start.wait().await;
                for (i, len) in [0, 3000, 3000].into_iter().enumerate() {
                    // Slower than the sender's 250 ms timeout: it re-offers twice.
                    let mut store = Store::slow(Duration::from_millis(600));
                    new_recv(b, &mut store, new::receiver_config())
                        .await
                        .unwrap_or_else(|e| panic!("current receive {i}: {e}"));
                    assert_eq!(store.bytes, file(len, i as u8));
                }
                done.wait().await;
            }),
        );
    });
}

/// With the default 10 s offer retry, a 0.1 receiver that takes a while to
/// prepare storage never sees a repeated offer.
#[test]
fn new_sender_default_offer_retry_suits_slow_old_receiver() {
    e2e(|| async {
        let bus = Bus::start();
        let (start, done) = (Barrier::new(2), Barrier::new(2));
        tokio::join!(
            new::run(bus.path(), NODE_A, new::Options::default(), async |a| {
                start.wait().await;
                for i in 0..2u8 {
                    a.file_transfer
                        .send_file(
                            NODE_B,
                            &file(3000, i),
                            new::deadline(Duration::from_secs(5)),
                            new::send_config(),
                        )
                        .await
                        .unwrap_or_else(|e| panic!("current send {i}: {e:?}"));
                }
                done.wait().await;
            }),
            old::run(bus.path(), NODE_B, Faults::none(), async |b| {
                start.wait().await;
                for i in 0..2u8 {
                    let mut store = Store::slow(Duration::from_millis(600));
                    old_recv(b, &mut store, Duration::from_secs(2))
                        .await
                        .unwrap_or_else(|e| panic!("0.1 receive {i}: {e}"));
                    assert_eq!(store.bytes, file(3000, i));
                }
                done.wait().await;
            }),
        );
    });
}

/// An offer retry shorter than a 0.1 receiver's storage preparation leaves a
/// repeated offer queued there, which its next `recv_file` takes as a new
/// transfer: the documented reason to keep the interval long with 0.1 peers.
#[test]
fn pins_short_offer_retry_breaks_slow_old_receiver() {
    e2e(|| async {
        let bus = Bus::start();
        let (start, done) = (Barrier::new(2), Barrier::new(2));
        tokio::join!(
            new::run(bus.path(), NODE_A, new::Options::default(), async |a| {
                start.wait().await;
                let config = new::SendConfig {
                    offer_retry_interval: Some(Duration::from_millis(150)),
                    ..new::send_config()
                };
                a.file_transfer
                    .send_file(
                        NODE_B,
                        &file(3000, 0),
                        new::deadline(Duration::from_secs(5)),
                        config,
                    )
                    .await
                    .expect("first transfer completes");
                let second = a
                    .file_transfer
                    .send_file(
                        NODE_B,
                        &file(3000, 1),
                        new::deadline(Duration::from_secs(2)),
                        config,
                    )
                    .await;
                assert!(second.is_err(), "second transfer was accepted: {second:?}");
                done.wait().await;
            }),
            old::run(bus.path(), NODE_B, Faults::none(), async |b| {
                start.wait().await;
                let mut store = Store::slow(Duration::from_millis(600));
                old_recv(b, &mut store, Duration::from_secs(2))
                    .await
                    .expect("first transfer completes");
                // The leftover repeat of the first offer starts a phantom transfer.
                let mut store = Store::default();
                let second = old_recv(b, &mut store, Duration::from_millis(500)).await;
                assert!(
                    second.is_err(),
                    "0.1 receiver got the second file: {second:?}"
                );
                done.wait().await;
            }),
        );
    });
}

// --- Liveness and lost acks ------------------------------------------------

/// When a 0.1 sender stops delivering chunks, the current receiver's idle
/// timeout aborts the transfer, and the `Abort` stops the sender promptly
/// instead of after all of its retries.
#[test]
fn idle_abort_stops_old_sender() {
    e2e(|| async {
        let bus = Bus::start();
        let (start, done) = (Barrier::new(2), Barrier::new(2));
        tokio::join!(
            old::run(
                bus.path(),
                NODE_A,
                Faults::drop_all(is_data_chunk),
                async |a| {
                    start.wait().await;
                    let started = Instant::now();
                    // Alone it would retry 5 times at 1 s each.
                    let result = a
                        .file_transfer
                        .send_file(
                            NODE_B,
                            &file(3000, 3),
                            Duration::from_secs(1),
                            old::send_config(),
                        )
                        .await;
                    let elapsed = started.elapsed();
                    assert!(result.is_err(), "{result:?}");
                    assert!(elapsed < Duration::from_millis(900), "{elapsed:?}");
                    done.wait().await;
                }
            ),
            new::run(bus.path(), NODE_B, new::Options::default(), async |b| {
                start.wait().await;
                let config = new::ReceiverConfig {
                    idle_timeout: Some(Duration::from_millis(300)),
                    ..new::receiver_config()
                };
                let mut store = Store::default();
                let result = new_recv(b, &mut store, config).await;
                assert!(result.is_err(), "{result:?}");
                done.wait().await;
            }),
        );
    });
}

/// A progress ack lost on its way from the current receiver: the 0.1 sender
/// retransmits after its timeout and the transfer completes.
#[test]
fn old_sender_recovers_from_lost_progress_ack() {
    e2e(|| async {
        let bus = Bus::start();
        let (start, done) = (Barrier::new(2), Barrier::new(2));
        let (drop_progress, _) = Faults::drop_first(1, is_progress);
        let sent = file(3000, 4);
        tokio::join!(
            old::run(bus.path(), NODE_A, Faults::none(), async |a| {
                start.wait().await;
                // A one-chunk window, so a later cumulative ack cannot cover the loss.
                let config = old::SendConfig {
                    window_chunks: 1,
                    ..old::send_config()
                };
                let result = a
                    .file_transfer
                    .send_file(NODE_B, &sent, Duration::from_millis(300), config)
                    .await
                    .expect("0.1 send");
                assert!(result.retries >= 1);
                done.wait().await;
            }),
            new::run(
                bus.path(),
                NODE_B,
                new::Options {
                    faults: drop_progress,
                    serve: true
                },
                async |b| {
                    start.wait().await;
                    let mut store = Store::default();
                    new_recv(b, &mut store, new::receiver_config())
                        .await
                        .expect("current receive");
                    assert_eq!(store.bytes, sent);
                    done.wait().await;
                }
            ),
        );
    });
}

/// A progress ack lost on its way from a 0.1 receiver: the current sender
/// retransmits after `ack_timeout` and the transfer completes.
#[test]
fn new_sender_recovers_from_lost_progress_ack() {
    e2e(|| async {
        let bus = Bus::start();
        let (start, done) = (Barrier::new(2), Barrier::new(2));
        let (drop_progress, _) = Faults::drop_first(1, is_progress);
        let sent = file(3000, 5);
        tokio::join!(
            new::run(bus.path(), NODE_A, new::Options::default(), async |a| {
                start.wait().await;
                let config = new::SendConfig {
                    window_chunks: 1,
                    ack_timeout: Duration::from_millis(200),
                    ..new::send_config()
                };
                let result = a
                    .file_transfer
                    .send_file(NODE_B, &sent, new::deadline(Duration::from_secs(5)), config)
                    .await
                    .expect("current send");
                assert!(result.retries >= 1);
                done.wait().await;
            }),
            old::run(bus.path(), NODE_B, drop_progress, async |b| {
                start.wait().await;
                let mut store = Store::default();
                old_recv(b, &mut store, Duration::from_secs(2))
                    .await
                    .expect("0.1 receive");
                assert_eq!(store.bytes, sent);
                done.wait().await;
            }),
        );
    });
}

/// Offer repeats are capped at `max_retries`, as in 0.1. A 0.1 receiver that is
/// not reading offers yet would otherwise fill its 16-slot offer mailbox, and
/// its receive pump stops on the resulting ingest error.
#[test]
fn new_sender_offer_repeats_are_bounded_for_busy_old_receiver() {
    e2e(|| async {
        let bus = Bus::start();
        let (start, done) = (Barrier::new(2), Barrier::new(2));
        let sent = file(3000, 6);
        tokio::join!(
            new::run(bus.path(), NODE_A, new::Options::default(), async |a| {
                start.wait().await;
                let config = new::SendConfig {
                    offer_retry_interval: Some(Duration::from_millis(50)),
                    ..new::send_config()
                };
                a.file_transfer
                    .send_file(NODE_B, &sent, new::deadline(Duration::from_secs(5)), config)
                    .await
                    .expect("current send");
                done.wait().await;
            }),
            old::run(bus.path(), NODE_B, Faults::none(), async |b| {
                start.wait().await;
                // Busy elsewhere: unbounded repeats every 50 ms would be ~30 here.
                tokio::time::sleep(Duration::from_millis(1500)).await;
                let mut store = Store::default();
                old_recv(b, &mut store, Duration::from_secs(2))
                    .await
                    .expect("0.1 receive");
                assert_eq!(store.bytes, sent);
                done.wait().await;
            }),
        );
    });
}

/// 4000-byte chunks make ISO-TP messages of about 4 KiB, near the 4095-byte
/// maximum, so both ISO-TP implementations segment and reassemble long
/// multi-frame messages from each other.
#[test]
fn large_chunks_near_isotp_maximum_in_both_directions() {
    const CHUNK: usize = 4000;
    e2e(|| async {
        let bus = Bus::start();
        let (start, done) = (Barrier::new(2), Barrier::new(2));
        let to_new = file(3 * CHUNK + 123, 10);
        let to_old = file(3 * CHUNK + 321, 11);
        tokio::join!(
            old::run(bus.path(), NODE_A, Faults::none(), async |a| {
                start.wait().await;
                let config = old::SendConfig {
                    sender_max_chunk_size: CHUNK,
                    ..old::send_config()
                };
                a.file_transfer
                    .send_file(NODE_B, &to_new, Duration::from_secs(2), config)
                    .await
                    .expect("0.1 send");
                let mut store = Store::default();
                let receiver = old::ReceiverConfig {
                    max_chunk_size: CHUNK as u32,
                };
                a.file_transfer
                    .recv_file(NODE_B, &mut store, Duration::from_secs(2), receiver)
                    .await
                    .expect("0.1 receive");
                assert_eq!(store.bytes, to_old);
                done.wait().await;
            }),
            new::run(bus.path(), NODE_B, new::Options::default(), async |b| {
                start.wait().await;
                let receiver = new::ReceiverConfig {
                    max_chunk_size: CHUNK as u32,
                    ..new::receiver_config()
                };
                let mut store = Store::default();
                new_recv(b, &mut store, receiver)
                    .await
                    .expect("current receive");
                assert_eq!(store.bytes, to_new);
                let config = new::SendConfig {
                    sender_max_chunk_size: CHUNK,
                    ..new::send_config()
                };
                b.file_transfer
                    .send_file(
                        NODE_A,
                        &to_old,
                        new::deadline(Duration::from_secs(5)),
                        config,
                    )
                    .await
                    .expect("current send");
                done.wait().await;
            }),
        );
    });
}

/// Repeats of a 0.1 sender's offer that queued while the current receiver
/// prepared storage are dropped when the transfer finishes, so even with
/// TIME_WAIT disabled the next `recv_file` gets the real next offer.
#[test]
fn new_receiver_drops_queued_repeats_without_time_wait() {
    e2e(|| async {
        let bus = Bus::start();
        let (start, done) = (Barrier::new(2), Barrier::new(2));
        tokio::join!(
            old::run(bus.path(), NODE_A, Faults::none(), async |a| {
                start.wait().await;
                for i in 0..2u8 {
                    a.file_transfer
                        .send_file(
                            NODE_B,
                            &file(3000, i),
                            Duration::from_millis(250),
                            old::send_config(),
                        )
                        .await
                        .unwrap_or_else(|e| panic!("0.1 send {i}: {e:?}"));
                }
                done.wait().await;
            }),
            new::run(bus.path(), NODE_B, new::Options::default(), async |b| {
                start.wait().await;
                b.file_transfer.set_time_wait(Duration::ZERO);
                for i in 0..2u8 {
                    // Slower than the sender's 250 ms timeout: it re-offers twice.
                    let mut store = Store::slow(Duration::from_millis(600));
                    new_recv(b, &mut store, new::receiver_config())
                        .await
                        .unwrap_or_else(|e| panic!("current receive {i}: {e}"));
                    assert_eq!(store.bytes, file(3000, i));
                }
                done.wait().await;
            }),
        );
    });
}

/// Repeats of an offer are dropped even when the transfer fails: here the
/// current receiver aborts the 0.1 sender's first transfer on its idle timeout
/// after the sender re-offered, and the next transfer must still be the real one.
#[test]
fn new_receiver_drops_queued_repeats_after_a_failed_transfer() {
    e2e(|| async {
        let bus = Bus::start();
        let (start, done) = (Barrier::new(2), Barrier::new(2));
        // Lose the first transfer's chunks (its whole first window).
        let (drop_chunks, _) = Faults::drop_first(3, is_data_chunk);
        tokio::join!(
            old::run(bus.path(), NODE_A, drop_chunks, async |a| {
                start.wait().await;
                let first = a
                    .file_transfer
                    .send_file(
                        NODE_B,
                        &file(3000, 1),
                        Duration::from_millis(250),
                        old::send_config(),
                    )
                    .await;
                assert!(
                    first.is_err(),
                    "first transfer was meant to fail: {first:?}"
                );
                a.file_transfer
                    .send_file(
                        NODE_B,
                        &file(3000, 2),
                        Duration::from_secs(2),
                        old::send_config(),
                    )
                    .await
                    .expect("second 0.1 send");
                done.wait().await;
            }),
            new::run(bus.path(), NODE_B, new::Options::default(), async |b| {
                start.wait().await;
                // Shorter than the sender's 250 ms retransmission timeout, so the
                // first transfer aborts instead of recovering.
                let idle = new::ReceiverConfig {
                    idle_timeout: Some(Duration::from_millis(100)),
                    ..new::receiver_config()
                };
                // Slower than the sender's 250 ms timeout: it re-offers twice.
                let mut store = Store::slow(Duration::from_millis(600));
                let first = new_recv(b, &mut store, idle).await;
                assert!(
                    first.is_err(),
                    "first transfer was meant to fail: {first:?}"
                );
                let mut store = Store::default();
                new_recv(b, &mut store, idle)
                    .await
                    .expect("second transfer is received, not a stale repeat");
                assert_eq!(store.bytes, file(3000, 2));
                done.wait().await;
            }),
        );
    });
}
