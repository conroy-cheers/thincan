#[cfg(feature = "uds")]
use anyhow::{Context, Result};
#[cfg(feature = "uds")]
use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
#[cfg(feature = "uds")]
use embedded_can_interface::{FilterConfig, RxFrameIo, TxFrameIo};
#[cfg(feature = "uds")]
use embedded_can_unix_socket::{BusServer, UnixCan};
#[cfg(feature = "uds")]
use std::path::PathBuf;
#[cfg(feature = "uds")]
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
    mpsc,
};
#[cfg(feature = "uds")]
use std::thread;
#[cfg(feature = "uds")]
use std::time::Duration;

#[cfg(feature = "uds")]
const IO_TIMEOUT: Duration = Duration::from_secs(5);
#[cfg(feature = "uds")]
const MAX_ISO_TP_PAYLOAD: usize = 4095;
#[cfg(feature = "uds")]
const MAX_PEERS: usize = 4;
#[cfg(feature = "uds")]
const THINCAN_WIRE_HEADER_LEN: usize = 2;

#[cfg(feature = "uds")]
thincan::bus_atlas! {
    pub mod atlas {
        0x1001 => FileReq(capnp = thincan_file_transfer::schema::file_req::Owned);
        0x1002 => FileChunk(capnp = thincan_file_transfer::schema::file_chunk::Owned);
        0x1003 => FileAck(capnp = thincan_file_transfer::schema::file_ack::Owned);
    }
}

#[cfg(feature = "uds")]
impl thincan_file_transfer::Atlas for atlas::Atlas {
    type FileReq = atlas::FileReq;
    type FileChunk = atlas::FileChunk;
    type FileAck = atlas::FileAck;
}

#[cfg(feature = "uds")]
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

#[cfg(feature = "uds")]
#[derive(Default)]
struct NoApp;

#[cfg(feature = "uds")]
impl<'a> maplet::Handlers<'a> for NoApp {
    type Error = ();
}

#[cfg(feature = "uds")]
#[derive(Clone)]
struct SharedTx<T> {
    inner: Arc<Mutex<T>>,
}

#[cfg(feature = "uds")]
#[derive(Clone)]
struct SharedRx<T> {
    inner: Arc<Mutex<T>>,
}

#[cfg(feature = "uds")]
fn split_shared<T>(can: T) -> (SharedTx<T>, SharedRx<T>) {
    let inner = Arc::new(Mutex::new(can));
    (
        SharedTx {
            inner: inner.clone(),
        },
        SharedRx { inner },
    )
}

#[cfg(feature = "uds")]
impl<T> TxFrameIo for SharedTx<T>
where
    T: TxFrameIo,
{
    type Frame = T::Frame;
    type Error = T::Error;

    fn send(&mut self, frame: &Self::Frame) -> Result<(), Self::Error> {
        let mut guard = self.inner.lock().unwrap();
        guard.send(frame)
    }

    fn try_send(&mut self, frame: &Self::Frame) -> Result<(), Self::Error> {
        let mut guard = self.inner.lock().unwrap();
        guard.try_send(frame)
    }

    fn send_timeout(&mut self, frame: &Self::Frame, timeout: Duration) -> Result<(), Self::Error> {
        let mut guard = self.inner.lock().unwrap();
        guard.send_timeout(frame, timeout)
    }
}

#[cfg(feature = "uds")]
impl<T> RxFrameIo for SharedRx<T>
where
    T: RxFrameIo,
{
    type Frame = T::Frame;
    type Error = T::Error;

    fn recv(&mut self) -> Result<Self::Frame, Self::Error> {
        let mut guard = self.inner.lock().unwrap();
        guard.recv()
    }

    fn try_recv(&mut self) -> Result<Self::Frame, Self::Error> {
        let mut guard = self.inner.lock().unwrap();
        guard.try_recv()
    }

    fn recv_timeout(&mut self, timeout: Duration) -> Result<Self::Frame, Self::Error> {
        let mut guard = self.inner.lock().unwrap();
        guard.recv_timeout(timeout)
    }

    fn wait_not_empty(&mut self) -> Result<(), Self::Error> {
        let mut guard = self.inner.lock().unwrap();
        guard.wait_not_empty()
    }
}

#[cfg(feature = "uds")]
#[derive(Debug, Clone)]
struct RecvResult {
    transfer_id: u32,
    total_len: usize,
    hash: [u8; 32],
}

#[cfg(feature = "uds")]
struct HashStore {
    results: mpsc::Sender<RecvResult>,
}

#[cfg(feature = "uds")]
struct HashWrite {
    transfer_id: u32,
    bytes: Vec<u8>,
}

#[cfg(feature = "uds")]
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

#[cfg(feature = "uds")]
fn splitmix64_next(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

#[cfg(feature = "uds")]
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

#[cfg(feature = "uds")]
fn max_chunk_data_len() -> usize {
    let max_body = MAX_ISO_TP_PAYLOAD.saturating_sub(THINCAN_WIRE_HEADER_LEN);
    // Cap'n Proto body overhead for FileChunk is 32 bytes (root pointer + struct + padding),
    // plus the data payload padded to 8 bytes.
    max_body.saturating_sub(32) & !7usize
}

#[cfg(feature = "uds")]
struct SocketCleanup(Option<PathBuf>);

#[cfg(feature = "uds")]
impl Drop for SocketCleanup {
    fn drop(&mut self) {
        if let Some(path) = &self.0 {
            let _ = std::fs::remove_file(path);
        }
    }
}

#[cfg(feature = "uds")]
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

    // Use an explicitly short socket path: Unix domain sockets have a small path limit (SUN_LEN),
    // and some environments (e.g. Nix shells) have very long temp directories.
    let params = format!("{seed}:{bytes}:{chunk_size}:{frame_len}");
    let h = blake3::hash(params.as_bytes()).to_hex();
    let unique = format!("euds_{}_{}.sock", std::process::id(), &h.as_str()[..8]);
    let socket_path = PathBuf::from("/tmp").join(unique);
    let _ = std::fs::remove_file(&socket_path);
    let cleanup = SocketCleanup(Some(socket_path.clone()));

    let server = BusServer::start(&socket_path)
        .with_context(|| format!("start bus server at {}", socket_path.display()))
        .unwrap();

    let from = 0x10u8;
    let to = 0x20u8;

    let file = gen_bytes(seed, bytes);
    let want_hash = *blake3::hash(&file).as_bytes();

    let (tx_results, rx_results) = mpsc::channel::<RecvResult>();
    let stop = Arc::new(AtomicBool::new(false));

    let rx_stop = stop.clone();
    let rx_socket = socket_path.clone();
    let rx_thread = thread::spawn(move || -> Result<()> {
        let mut can = UnixCan::connect(&rx_socket)
            .with_context(|| format!("connect rx to {}", rx_socket.display()))?;
        FilterConfig::set_filters(&mut can, &[can_uds::uds29::filter_phys_for_target(to)])
            .context("set rx CAN acceptance filter")?;

        let (tx, rx) = split_shared(can);
        let storages: [can_iso_tp::RxStorage<'static>; MAX_PEERS] =
            std::array::from_fn(|_| can_iso_tp::RxStorage::Owned(vec![0u8; MAX_ISO_TP_PAYLOAD]));
        let mut cfg = can_iso_tp::IsoTpConfig::default();
        cfg.max_payload_len = MAX_ISO_TP_PAYLOAD;
        cfg.block_size = 0;
        cfg.st_min = Duration::from_micros(0);
        cfg.frame_len = frame_len;
        let node = can_iso_tp::IsoTpDemux::new(tx, rx, cfg, can_iso_tp::StdClock, to, storages)
            .map_err(|_| anyhow::anyhow!("failed to build ISO-TP demux"))?;

        let mut tx_buf = vec![0u8; 4096];
        let mut iface = thincan::Interface::new(node, maplet::Router::new(), tx_buf.as_mut_slice());
        let store = HashStore {
            results: tx_results,
        };
        let mut handlers = maplet::HandlersImpl {
            app: NoApp::default(),
            file_transfer: thincan_file_transfer::State::new(store),
        };

        while !rx_stop.load(Ordering::Relaxed) {
            let _ = iface.recv_one_dispatch_meta(&mut handlers, Duration::from_millis(10));
        }
        Ok(())
    });

    let mut can = UnixCan::connect(&socket_path)
        .with_context(|| format!("connect tx to {}", socket_path.display()))
        .unwrap();
    FilterConfig::set_filters(&mut can, &[can_uds::uds29::filter_phys_for_target(from)])
        .context("set tx CAN acceptance filter")
        .unwrap();

    let (tx, rx) = split_shared(can);
    let storages: [can_iso_tp::RxStorage<'static>; MAX_PEERS] =
        std::array::from_fn(|_| can_iso_tp::RxStorage::Owned(vec![0u8; MAX_ISO_TP_PAYLOAD]));
    let mut cfg = can_iso_tp::IsoTpConfig::default();
    cfg.max_payload_len = MAX_ISO_TP_PAYLOAD;
    cfg.block_size = 0;
    cfg.st_min = Duration::from_micros(0);
    cfg.frame_len = frame_len;
    let node = can_iso_tp::IsoTpDemux::new(tx, rx, cfg, can_iso_tp::StdClock, from, storages)
        .map_err(|_| anyhow::anyhow!("failed to build ISO-TP demux"))
        .unwrap();

    let mut tx_buf = vec![0u8; 4096];
    let mut iface = thincan::Interface::new(node, maplet::Router::new(), tx_buf.as_mut_slice());
    let mut sender =
        thincan_file_transfer::Sender::<atlas::Atlas>::with_chunk_size(chunk_size).unwrap();

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
    drop(server);
    drop(cleanup);
}

#[cfg(feature = "uds")]
fn uds_file_transfer(c: &mut Criterion) {
    let mut group = c.benchmark_group("uds_file_transfer");
    group.warm_up_time(Duration::from_millis(200));
    group.measurement_time(Duration::from_secs(4));
    group.sample_size(10);
    group.sampling_mode(criterion::SamplingMode::Flat);

    let bytes_cases_classic = [64 * 1024usize, 128 * 1024usize];
    let bytes_cases_fd = [64 * 1024usize, 1024 * 1024usize];
    let chunk_cases = [256usize, 1024usize, 2048usize];
    let frame_len_cases = [8usize, 64usize];
    let seed = 1u64;

    for &frame_len in &frame_len_cases {
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

#[cfg(feature = "uds")]
criterion_group!(benches, uds_file_transfer);
#[cfg(feature = "uds")]
criterion_main!(benches);

#[cfg(not(feature = "uds"))]
fn main() {}
