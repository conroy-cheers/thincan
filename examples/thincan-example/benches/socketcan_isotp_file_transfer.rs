#[cfg(feature = "socketcan-isotp")]
use anyhow::{Context, Result};
#[cfg(feature = "socketcan-isotp")]
use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
#[cfg(feature = "socketcan-isotp")]
use linux_socketcan_iso_tp::{
    IsoTpFlowControlOptions, IsoTpKernelOptions, IsoTpLinkLayerOptions, KernelUdsDemux, flags,
};
#[cfg(feature = "socketcan-isotp")]
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
    mpsc,
};
#[cfg(feature = "socketcan-isotp")]
use std::thread;
#[cfg(feature = "socketcan-isotp")]
use std::time::Duration;

#[cfg(feature = "socketcan-isotp")]
const IO_TIMEOUT: Duration = Duration::from_secs(5);
#[cfg(feature = "socketcan-isotp")]
const MAX_ISO_TP_PAYLOAD: usize = 4095;
#[cfg(feature = "socketcan-isotp")]
const THINCAN_WIRE_HEADER_LEN: usize = 2;
#[cfg(feature = "socketcan-isotp")]
const CANFD_MTU: u8 = 72;

#[cfg(feature = "socketcan-isotp")]
thincan::bus_atlas! {
    pub mod atlas {
        0x1001 => FileReq(capnp = thincan_file_transfer::schema::file_req::Owned);
        0x1002 => FileChunk(capnp = thincan_file_transfer::schema::file_chunk::Owned);
        0x1003 => FileAck(capnp = thincan_file_transfer::schema::file_ack::Owned);
    }
}

#[cfg(feature = "socketcan-isotp")]
impl thincan_file_transfer::Atlas for atlas::Atlas {
    type FileReq = atlas::FileReq;
    type FileChunk = atlas::FileChunk;
    type FileAck = atlas::FileAck;
}

#[cfg(feature = "socketcan-isotp")]
thincan::bus_maplet! {
    pub mod maplet: atlas {
        bundles [file_transfer = thincan_file_transfer::Bundle];
        parser: thincan::DefaultParser;
        use msgs [FileReq, FileChunk, FileAck];
        handles {}
        unhandled_by_default = true;
        ignore [];
    }
}

#[cfg(feature = "socketcan-isotp")]
#[derive(Default)]
struct NoApp;

#[cfg(feature = "socketcan-isotp")]
impl<'a> maplet::Handlers<'a> for NoApp {
    type Error = ();
}

#[cfg(feature = "socketcan-isotp")]
#[derive(Debug, Clone)]
struct RecvResult {
    transfer_id: u32,
    total_len: usize,
    hash: [u8; 32],
}

#[cfg(feature = "socketcan-isotp")]
struct HashStore {
    results: mpsc::Sender<RecvResult>,
}

#[cfg(feature = "socketcan-isotp")]
struct HashWrite {
    transfer_id: u32,
    bytes: Vec<u8>,
}

#[cfg(feature = "socketcan-isotp")]
impl thincan_file_transfer::FileStore for HashStore {
    type Error = core::convert::Infallible;
    type WriteHandle = HashWrite;

    fn begin_write(
        &mut self,
        transfer_id: u32,
        total_len: u32,
    ) -> Result<Self::WriteHandle, Self::Error> {
        Ok(HashWrite {
            transfer_id,
            bytes: vec![0u8; total_len as usize],
        })
    }

    fn write_at(
        &mut self,
        handle: &mut Self::WriteHandle,
        offset: u32,
        bytes: &[u8],
    ) -> Result<(), Self::Error> {
        let start = offset as usize;
        let end = start + bytes.len();
        handle.bytes[start..end].copy_from_slice(bytes);
        Ok(())
    }

    fn commit(&mut self, handle: Self::WriteHandle) -> Result<(), Self::Error> {
        let hash = blake3::hash(&handle.bytes);
        let _ = self.results.send(RecvResult {
            transfer_id: handle.transfer_id,
            total_len: handle.bytes.len(),
            hash: *hash.as_bytes(),
        });
        Ok(())
    }

    fn abort(&mut self, _handle: Self::WriteHandle) {}
}

#[cfg(feature = "socketcan-isotp")]
fn splitmix64_next(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

#[cfg(feature = "socketcan-isotp")]
fn gen_bytes(seed: u64, len: usize) -> Vec<u8> {
    let mut out = vec![0u8; len];
    let mut state = seed;
    let mut i = 0usize;
    while i + 8 <= len {
        let v = splitmix64_next(&mut state).to_le_bytes();
        out[i..i + 8].copy_from_slice(&v);
        i += 8;
    }
    if i < len {
        let v = splitmix64_next(&mut state).to_le_bytes();
        out[i..].copy_from_slice(&v[..(len - i)]);
    }
    out
}

#[cfg(feature = "socketcan-isotp")]
fn max_chunk_data_len() -> usize {
    let max_body = MAX_ISO_TP_PAYLOAD.saturating_sub(THINCAN_WIRE_HEADER_LEN);
    max_body.saturating_sub(32) & !7usize
}

#[cfg(feature = "socketcan-isotp")]
fn iface_name() -> String {
    std::env::var("THINCAN_BENCH_IFACE").unwrap_or_else(|_| "can0".to_string())
}

#[cfg(feature = "socketcan-isotp")]
fn kernel_options(frame_len: usize) -> IsoTpKernelOptions {
    // Be explicit here: kernel defaults vary across distros/kernel versions and can be surprisingly
    // conservative (which absolutely tanks throughput on vcan).
    let mut options = IsoTpKernelOptions::default();
    options.max_rx_payload = MAX_ISO_TP_PAYLOAD;
    options.socket.flags = flags::CAN_ISOTP_WAIT_TX_DONE;
    options.flow_control = Some(IsoTpFlowControlOptions::new(0, 0, 0));
    options.force_tx_stmin = Some(Duration::from_nanos(0));
    options.force_rx_stmin = Some(Duration::from_nanos(0));
    if frame_len > 8 {
        options.link_layer = Some(IsoTpLinkLayerOptions::new(CANFD_MTU, frame_len as u8, 0));
    }
    options
}

#[cfg(feature = "socketcan-isotp")]
fn can_supports_frame_len(iface: &str, frame_len: usize) -> bool {
    if !(8..=64).contains(&frame_len) {
        return false;
    }

    let options = kernel_options(frame_len);
    let mut demux = KernelUdsDemux::new(iface, 0x10, options);
    demux.register_peer(0x20).is_ok()
}

#[cfg(feature = "socketcan-isotp")]
fn run_case(
    b: &mut criterion::Bencher<'_>,
    bytes: usize,
    chunk_size: usize,
    frame_len: usize,
    seed: u64,
) {
    let max_chunk = max_chunk_data_len();
    if chunk_size == 0 || chunk_size > max_chunk {
        panic!("chunk_size must be in 1..={max_chunk}");
    }
    if !(8..=64).contains(&frame_len) {
        panic!("frame_len must be in 8..=64");
    }

    // Each criterion case runs back-to-back in the same process. With kernel ISO-TP sockets,
    // in-flight frames can linger briefly after a case finishes, which can corrupt the next case
    // if it reuses the same RX/TX IDs. Use per-case addressing to avoid cross-talk.
    let case = (bytes as u32)
        .wrapping_mul(0x9E37_79B1)
        .wrapping_add(chunk_size as u32)
        .wrapping_mul(0x85EB_CA6B);
    let from = 0x10u8.wrapping_add((case & 0x3F) as u8);
    let to = 0x80u8.wrapping_add(((case >> 6) & 0x3F) as u8);
    let iface = iface_name();

    let file = gen_bytes(seed, bytes);
    let want_hash = *blake3::hash(&file).as_bytes();

    let (tx_results, rx_results) = mpsc::channel::<RecvResult>();
    let (tx_ready, rx_ready) = mpsc::channel::<()>();
    let stop = Arc::new(AtomicBool::new(false));

    let rx_stop = stop.clone();
    let rx_iface = iface.clone();
    let rx_thread = thread::spawn(move || -> Result<()> {
        let options = kernel_options(frame_len);
        let mut demux = KernelUdsDemux::new(rx_iface, to, options);
        demux
            .register_peer(from)
            .context("register kernel ISO-TP peer")?;

        let mut tx_buf = vec![0u8; 4096];
        let mut iface = thincan::Interface::new(
            thincan::IsoTpEndpointMetaTransport::new(demux),
            maplet::Router::new(),
            tx_buf.as_mut_slice(),
        );
        let store = HashStore {
            results: tx_results,
        };
        let mut handlers = maplet::HandlersImpl {
            app: NoApp::default(),
            file_transfer: thincan_file_transfer::State::new(store),
        };

        let _ = tx_ready.send(());
        while !rx_stop.load(Ordering::Relaxed) {
            let _ = iface.recv_one_dispatch_meta(&mut handlers, Duration::from_millis(10));
        }
        Ok(())
    });

    rx_ready
        .recv_timeout(Duration::from_secs(5))
        .expect("receiver thread did not become ready");

    let options = kernel_options(frame_len);
    let mut demux = KernelUdsDemux::new(iface, from, options);
    demux
        .register_peer(to)
        .expect("register kernel ISO-TP peer");

    let mut tx_buf = vec![0u8; 4096];
    let mut iface = thincan::Interface::new(
        thincan::IsoTpEndpointMetaTransport::new(demux),
        maplet::Router::new(),
        tx_buf.as_mut_slice(),
    );
    let mut sender = thincan_file_transfer::Sender::<atlas::Atlas>::with_chunk_size(chunk_size)
        .expect("failed to build file sender");

    let send_result = sender
        .send_file_to(&mut iface, to, &file, IO_TIMEOUT)
        .expect("failed to send file");
    let got = rx_results.recv_timeout(Duration::from_secs(30)).unwrap();
    if got.transfer_id != send_result.transfer_id || got.total_len != send_result.total_len {
        panic!("receiver committed unexpected transfer");
    }
    if got.hash != want_hash {
        panic!("hash mismatch (transfer_id {})", got.transfer_id);
    }

    b.iter(|| {
        let send_result = sender
            .send_file_to(&mut iface, to, &file, IO_TIMEOUT)
            .unwrap();
        let got = rx_results.recv_timeout(Duration::from_secs(30)).unwrap();
        if got.transfer_id != send_result.transfer_id {
            panic!(
                "receiver committed transfer_id {} but sender sent {}",
                got.transfer_id, send_result.transfer_id
            );
        }
        if got.total_len != send_result.total_len {
            panic!(
                "receiver committed {} bytes but sender sent {}",
                got.total_len, send_result.total_len
            );
        }
        if got.hash != want_hash {
            panic!("hash mismatch (transfer_id {})", got.transfer_id);
        }
    });

    stop.store(true, Ordering::Relaxed);
    let _ = rx_thread.join();
}

#[cfg(feature = "socketcan-isotp")]
fn socketcan_isotp_file_transfer(c: &mut Criterion) {
    let iface = iface_name();

    let mut group = c.benchmark_group("socketcan_isotp_file_transfer");
    group.warm_up_time(Duration::from_millis(200));
    group.measurement_time(Duration::from_secs(4));
    group.sample_size(10);
    group.sampling_mode(criterion::SamplingMode::Flat);

    let bytes_cases_classic = [64 * 1024usize, 128 * 1024usize];
    let bytes_cases_fd = [64 * 1024usize, 1024 * 1024usize];
    let mut chunk_cases = vec![256usize, 1024usize, 2048usize, max_chunk_data_len()];
    chunk_cases.sort_unstable();
    chunk_cases.dedup();
    let frame_len_cases = [8usize, 64usize];
    let seed = 1u64;

    for &frame_len in &frame_len_cases {
        if !can_supports_frame_len(&iface, frame_len) {
            eprintln!(
                "skipping {frame_len}-byte ISO-TP frames on {iface} (does not appear to support them)"
            );
            continue;
        }
        let bytes_cases: &[usize] = if frame_len <= 8 {
            &bytes_cases_classic
        } else {
            &bytes_cases_fd
        };

        for &bytes in bytes_cases {
            group.throughput(Throughput::Bytes(bytes as u64));
            for &chunk_size in &chunk_cases {
                if chunk_size > max_chunk_data_len() {
                    continue;
                }
                group.bench_with_input(
                    BenchmarkId::new(
                        format!("{bytes}_bytes/{frame_len}_frame"),
                        format!("{chunk_size}_chunk"),
                    ),
                    &chunk_size,
                    |b, &chunk_size| run_case(b, bytes, chunk_size, frame_len, seed),
                );
            }
        }
    }

    group.finish();
}

#[cfg(feature = "socketcan-isotp")]
criterion_group!(benches, socketcan_isotp_file_transfer);
#[cfg(feature = "socketcan-isotp")]
criterion_main!(benches);

#[cfg(not(feature = "socketcan-isotp"))]
fn main() {}
