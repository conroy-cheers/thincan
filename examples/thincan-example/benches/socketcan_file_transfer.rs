#[cfg(feature = "socketcan")]
use anyhow::{Context, Result};
#[cfg(feature = "socketcan")]
use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
#[cfg(feature = "socketcan")]
use embedded_can_interface::{
    BlockingControl, FilterConfig, IdMask, IdMaskFilter, RxFrameIo, TxFrameIo,
};
#[cfg(feature = "socketcan")]
use embedded_can_socketcan::SocketCanFd;
#[cfg(feature = "socketcan")]
use socketcan::Error as SocketcanError;
#[cfg(feature = "socketcan")]
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
    mpsc,
};
#[cfg(feature = "socketcan")]
use std::thread;
#[cfg(feature = "socketcan")]
use std::time::Duration;

#[cfg(feature = "socketcan")]
const IO_TIMEOUT: Duration = Duration::from_secs(5);
#[cfg(feature = "socketcan")]
const MAX_ISO_TP_PAYLOAD: usize = 4095;
#[cfg(feature = "socketcan")]
const MAX_PEERS: usize = 4;
#[cfg(feature = "socketcan")]
const THINCAN_WIRE_HEADER_LEN: usize = 2;

#[cfg(feature = "socketcan")]
thincan::bus_atlas! {
    pub mod atlas {
        0x1001 => FileReq(capnp = thincan_file_transfer::schema::file_req::Owned);
        0x1002 => FileChunk(capnp = thincan_file_transfer::schema::file_chunk::Owned);
        0x1003 => FileAck(capnp = thincan_file_transfer::schema::file_ack::Owned);
    }
}

#[cfg(feature = "socketcan")]
impl thincan_file_transfer::Atlas for atlas::Atlas {
    type FileReq = atlas::FileReq;
    type FileChunk = atlas::FileChunk;
    type FileAck = atlas::FileAck;
}

#[cfg(feature = "socketcan")]
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

#[cfg(feature = "socketcan")]
#[derive(Default)]
struct NoApp;

#[cfg(feature = "socketcan")]
impl<'a> maplet::Handlers<'a> for NoApp {
    type Error = ();
}

#[cfg(feature = "socketcan")]
#[allow(dead_code)]
#[derive(Clone)]
struct SharedTx<T> {
    inner: Arc<Mutex<T>>,
}

#[cfg(feature = "socketcan")]
#[allow(dead_code)]
#[derive(Clone)]
struct SharedRx<T> {
    inner: Arc<Mutex<T>>,
}

#[cfg(feature = "socketcan")]
#[allow(dead_code)]
fn split_shared<T>(can: T) -> (SharedTx<T>, SharedRx<T>) {
    let inner = Arc::new(Mutex::new(can));
    (
        SharedTx {
            inner: inner.clone(),
        },
        SharedRx { inner },
    )
}

#[cfg(feature = "socketcan")]
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

#[cfg(feature = "socketcan")]
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

#[cfg(feature = "socketcan")]
#[derive(Debug, Clone)]
struct RecvResult {
    transfer_id: u32,
    total_len: usize,
    hash: [u8; 32],
}

#[cfg(feature = "socketcan")]
struct HashStore {
    results: mpsc::Sender<RecvResult>,
}

#[cfg(feature = "socketcan")]
struct HashWrite {
    transfer_id: u32,
    bytes: Vec<u8>,
}

#[cfg(feature = "socketcan")]
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

#[cfg(feature = "socketcan")]
fn splitmix64_next(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

#[cfg(feature = "socketcan")]
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

#[cfg(feature = "socketcan")]
fn max_chunk_data_len() -> usize {
    let max_body = MAX_ISO_TP_PAYLOAD.saturating_sub(THINCAN_WIRE_HEADER_LEN);
    max_body.saturating_sub(32) & !7usize
}

#[cfg(feature = "socketcan")]
fn iface_name() -> String {
    std::env::var("THINCAN_BENCH_IFACE").unwrap_or_else(|_| "can0".to_string())
}

#[cfg(feature = "socketcan")]
fn filter_exact_phys(target: u8, source: u8) -> IdMaskFilter {
    IdMaskFilter {
        id: can_uds::uds29::encode_phys_id(target, source),
        mask: IdMask::Extended(0x1FFF_FFFF),
    }
}

#[cfg(feature = "socketcan")]
fn drain_rx(can: &mut SocketCanFd) {
    loop {
        match can.try_recv() {
            Ok(_) => continue,
            Err(SocketcanError::Io(io)) if io.kind() == std::io::ErrorKind::WouldBlock => break,
            Err(_) => break,
        }
    }
}

#[cfg(feature = "socketcan")]
fn can_supports_frame_len(iface: &str, frame_len: usize) -> bool {
    let mut can = match SocketCanFd::open(iface) {
        Ok(c) => c,
        Err(_) => return false,
    };
    let id = embedded_can::Id::Extended(embedded_can::ExtendedId::new(0x18DA_AA_BBu32).unwrap());
    let data = vec![0xA5u8; frame_len.min(64)];
    let frame = match <<SocketCanFd as TxFrameIo>::Frame as embedded_can::Frame>::new(id, &data) {
        Some(f) => f,
        None => return false,
    };
    can.send_timeout(&frame, Duration::from_millis(50)).is_ok()
}

#[cfg(feature = "socketcan")]
fn run_case(
    iface: &str,
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

    let from = 0x10u8;
    let to = 0x20u8;

    let file = gen_bytes(seed, bytes);
    let want_hash = *blake3::hash(&file).as_bytes();

    let (tx_results, rx_results) = mpsc::channel::<RecvResult>();
    let (tx_ready, rx_ready) = mpsc::channel::<()>();
    let stop = Arc::new(AtomicBool::new(false));

    let rx_stop = stop.clone();
    let rx_iface = iface.to_string();
    let rx_thread = thread::spawn(move || -> Result<()> {
        let mut can = SocketCanFd::open(&rx_iface)
            .with_context(|| format!("open rx socketcan interface {rx_iface}"))?;
        can.set_nonblocking(true)
            .context("set rx socketcan nonblocking")?;
        FilterConfig::set_filters(&mut can, &[filter_exact_phys(to, from)])
            .context("set rx CAN acceptance filter (exact ID)")?;
        drain_rx(&mut can);

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

        let _ = tx_ready.send(());
        while !rx_stop.load(Ordering::Relaxed) {
            let _ = iface.recv_one_dispatch_meta(&mut handlers, Duration::from_millis(10));
        }
        Ok(())
    });

    rx_ready
        .recv_timeout(Duration::from_secs(5))
        .expect("receiver thread did not become ready");

    let mut can = SocketCanFd::open(iface)
        .with_context(|| format!("open tx socketcan interface {iface}"))
        .unwrap();
    can.set_nonblocking(true)
        .context("set tx socketcan nonblocking")
        .unwrap();
    FilterConfig::set_filters(&mut can, &[filter_exact_phys(from, to)])
        .context("set tx CAN acceptance filter (exact ID)")
        .unwrap();
    drain_rx(&mut can);

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

    // Preflight one transfer so we fail fast outside Criterion's inner loop.
    let send_result = sender
        .send_file_to(&mut iface, to, &file, IO_TIMEOUT)
        .unwrap();
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

#[cfg(feature = "socketcan")]
fn socketcan_file_transfer(c: &mut Criterion) {
    let iface = iface_name();

    let mut group = c.benchmark_group("socketcan_file_transfer");
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
                "skipping {frame_len}-byte frames on {iface} (does not appear to support them)"
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
                    |b, &chunk_size| run_case(&iface, b, bytes, chunk_size, frame_len, seed),
                );
            }
        }
    }

    group.finish();
}

#[cfg(feature = "socketcan")]
criterion_group!(benches, socketcan_file_transfer);
#[cfg(feature = "socketcan")]
criterion_main!(benches);

#[cfg(not(feature = "socketcan"))]
fn main() {}
