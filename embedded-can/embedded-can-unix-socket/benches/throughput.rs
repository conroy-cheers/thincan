use criterion::{BenchmarkId, Criterion, Throughput, black_box, criterion_group, criterion_main};
use embedded_can::Frame as _;
use embedded_can::{Id, StandardId};
use embedded_can_interface::{FilterConfig, IdMask, IdMaskFilter, RxFrameIo, TxFrameIo};
use embedded_can_unix_socket::{BusServer, UnixCan, UnixCanError, UnixFrame};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

const WARM_UP_TIME: Duration = Duration::from_millis(300);
const MEASUREMENT_TIME: Duration = Duration::from_millis(1200);
const SAMPLE_SIZE: usize = 10;

fn connect_with_retry(path: &PathBuf) -> UnixCan {
    for _ in 0..200 {
        if let Ok(client) = UnixCan::connect(path) {
            return client;
        }
        thread::sleep(Duration::from_millis(2));
    }
    UnixCan::connect(path).expect("connect")
}

fn spawn_drain_thread(
    mut client: UnixCan,
    stop: Arc<AtomicBool>,
    counter: Arc<AtomicU64>,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        while !stop.load(Ordering::Relaxed) {
            match client.recv_timeout(Duration::from_millis(50)) {
                Ok(_frame) => {
                    counter.fetch_add(1, Ordering::Relaxed);
                }
                Err(UnixCanError::Timeout) => {}
                Err(_) => break,
            }
        }
    })
}

fn wait_for_delta(counter: &AtomicU64, start: u64, delta: u64, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    while counter.load(Ordering::Relaxed).wrapping_sub(start) < delta {
        if Instant::now() >= deadline {
            panic!("timed out waiting for receiver to drain frames");
        }
        std::hint::spin_loop();
        thread::yield_now();
    }
}

fn bench_multi_client_end_to_end(c: &mut Criterion) {
    let mut group = c.benchmark_group("end_to_end");
    group.warm_up_time(WARM_UP_TIME);
    group.measurement_time(MEASUREMENT_TIME);
    group.sample_size(SAMPLE_SIZE);

    let clients_totals: &[usize] = &[2, 4, 8];

    for &clients_total in clients_totals {
        group.throughput(Throughput::Elements(1));
        group.bench_with_input(
            BenchmarkId::new("send_ack", clients_total),
            &clients_total,
            |b, &clients_total| {
                let tmp = tempfile::tempdir().expect("tempdir");
                let path = tmp.path().join("can-uds-bench.sock");

                let mut server = BusServer::start(&path).expect("server start");

                let mut sender = connect_with_retry(&path);

                let stop = Arc::new(AtomicBool::new(false));
                let mut handles = Vec::new();
                let mut counters = Vec::new();

                for _ in 1..clients_total {
                    let client = connect_with_retry(&path);
                    let counter = Arc::new(AtomicU64::new(0));
                    let handle = spawn_drain_thread(client, stop.clone(), Arc::clone(&counter));
                    handles.push(handle);
                    counters.push(counter);
                }

                let frame = UnixFrame::new(
                    Id::Standard(StandardId::new(0x123).unwrap()),
                    &[1, 2, 3, 4, 5, 6, 7, 8],
                )
                .unwrap();

                b.iter_custom(|iters| {
                    let iters_u64 = iters as u64;
                    let starts: Vec<u64> =
                        counters.iter().map(|c| c.load(Ordering::Relaxed)).collect();

                    let start = Instant::now();
                    for _ in 0..iters {
                        sender.send(black_box(&frame)).unwrap();
                    }

                    for (counter, &s) in counters.iter().zip(starts.iter()) {
                        wait_for_delta(counter, s, iters_u64, Duration::from_secs(2));
                    }

                    start.elapsed()
                });

                stop.store(true, Ordering::Relaxed);
                for handle in handles {
                    let _ = handle.join();
                }
                server.shutdown().expect("server shutdown");
            },
        );
    }

    group.finish();
}

fn bench_contended_send_ack_8(c: &mut Criterion) {
    let mut group = c.benchmark_group("contended");
    group.warm_up_time(WARM_UP_TIME);
    group.measurement_time(MEASUREMENT_TIME);
    group.sample_size(SAMPLE_SIZE);

    let clients_total: usize = 8;
    let senders_total: usize = clients_total - 1;

    group.throughput(Throughput::Elements(senders_total as u64));
    group.bench_function(BenchmarkId::new("contended_send_ack", clients_total), |b| {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("can-uds-arb.sock");

        let mut server = BusServer::start(&path).expect("server start");

        let mut observer = connect_with_retry(&path);

        let drop_all_filter = IdMaskFilter {
            id: embedded_can_interface::Id::Standard(StandardId::new(0x7FF).unwrap()),
            mask: IdMask::Standard(0x7FF),
        };

        let barrier = Arc::new(std::sync::Barrier::new(senders_total + 1));
        let mut txs = Vec::with_capacity(senders_total);
        let mut handles = Vec::with_capacity(senders_total);

        for idx in 0..senders_total {
            let mut client = connect_with_retry(&path);
            FilterConfig::set_filters(&mut client, &[drop_all_filter]).expect("set filters");

            let (tx, rx) = std::sync::mpsc::channel::<Option<u64>>();
            txs.push(tx);
            let barrier = barrier.clone();
            let mut sender = client;

            let id = Id::Standard(StandardId::new(0x100 + idx as u16).unwrap());
            let frame = UnixFrame::new(id, &[idx as u8; 8]).unwrap();
            let handle = thread::spawn(move || {
                while let Ok(cmd) = rx.recv() {
                    let Some(rounds) = cmd else { break };
                    for _ in 0..rounds {
                        barrier.wait();
                        sender.send(black_box(&frame)).unwrap();
                        barrier.wait();
                    }
                }
            });
            handles.push(handle);
        }

        b.iter_custom(|iters| {
            let rounds = iters as u64;
            for tx in &txs {
                tx.send(Some(rounds)).unwrap();
            }

            let start = Instant::now();
            for _ in 0..rounds {
                barrier.wait();
                for _ in 0..senders_total {
                    observer.recv_timeout(Duration::from_secs(1)).unwrap();
                }
                barrier.wait();
            }
            start.elapsed()
        });

        for tx in txs {
            let _ = tx.send(None);
        }
        for handle in handles {
            let _ = handle.join();
        }
        server.shutdown().expect("server shutdown");
    });

    group.finish();
}

criterion_group!(
    benches,
    bench_multi_client_end_to_end,
    bench_contended_send_ack_8
);
criterion_main!(benches);
