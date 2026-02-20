use anyhow::{Context, Result};
use can_isotp_interface::{
    IsoTpAsyncEndpoint, RecvControl, RecvError, RecvMeta, RecvStatus, SendError,
};
use core::future::Future;
use core::pin::Pin;
use core::time::Duration;
use embedded_can::Frame as _;
use embedded_can_interface::{AsyncRxFrameIo, AsyncTxFrameIo, RxFrameIo, TxFrameIo};
use std::io::ErrorKind;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

pub const NODE_A_ADDR: u8 = 0x11;
pub const NODE_B_ADDR: u8 = 0x22;

pub const MAX_PEERS: usize = 4;
pub const MAX_ISOTP_PAYLOAD: usize = 4095;

pub const DEPTH: usize = 16;
pub const MAX_BODY: usize = 4096;
pub const MAX_WAITERS: usize = 8;
pub const TX_BUF_LEN: usize = 8192;

pub const SEND_CHUNK_SIZE: usize = 1024;
pub const SEND_WINDOW_CHUNKS: usize = 8;
pub const SEND_MAX_RETRIES: usize = 5;

thincan::bus_atlas! {
    pub mod atlas {
        0x1001 => FileReq(capnp = thincan_file_transfer::schema::file_req::Owned);
        0x1002 => FileChunk(capnp = thincan_file_transfer::schema::file_chunk::Owned);
        0x1003 => FileAck(capnp = thincan_file_transfer::schema::file_ack::Owned);
    }
}

pub mod file_transfer_bundle {
    pub type Bundle = thincan_file_transfer::FileTransferBundle<super::atlas::Atlas>;
    pub const MESSAGE_COUNT: usize = thincan_file_transfer::FILE_TRANSFER_MESSAGE_COUNT;
}

thincan::maplet! {
    pub mod maplet: atlas {
        bundles [file_transfer = file_transfer_bundle];
    }
}

impl thincan_file_transfer::Atlas for atlas::Atlas {
    type FileReq = atlas::FileReq;
    type FileChunk = atlas::FileChunk;
    type FileAck = atlas::FileAck;
}

#[derive(Clone, Copy, Default)]
pub struct TokioRuntime;

impl can_iso_tp::AsyncRuntime for TokioRuntime {
    type TimeoutError = can_iso_tp::TimedOut;

    type Sleep<'a>
        = Pin<Box<dyn Future<Output = ()> + 'a>>
    where
        Self: 'a;

    fn sleep<'a>(&'a self, duration: Duration) -> Self::Sleep<'a> {
        Box::pin(tokio::time::sleep(duration))
    }

    type Timeout<'a, F>
        = Pin<Box<dyn Future<Output = Result<F::Output, Self::TimeoutError>> + 'a>>
    where
        Self: 'a,
        F: Future + 'a;

    fn timeout<'a, F>(&'a self, duration: Duration, future: F) -> Self::Timeout<'a, F>
    where
        F: Future + 'a,
    {
        Box::pin(async move {
            tokio::time::timeout(duration, future)
                .await
                .map_err(|_| can_iso_tp::TimedOut)
        })
    }
}

#[derive(Clone)]
pub struct AsyncUnixCanTx {
    inner: Arc<Mutex<embedded_can_unix_socket::UnixCan>>,
}

pub struct AsyncUnixCanRx {
    events: tokio::sync::mpsc::UnboundedReceiver<
        Result<embedded_can_unix_socket::UnixFrame, embedded_can_unix_socket::UnixCanError>,
    >,
}

fn join_err_to_unix_can(err: tokio::task::JoinError) -> embedded_can_unix_socket::UnixCanError {
    embedded_can_unix_socket::UnixCanError::Io(std::io::Error::other(format!(
        "spawn_blocking join error: {err}"
    )))
}

const IO_SLICE: Duration = Duration::from_millis(20);

fn trace_frame(prefix: &str, frame: &embedded_can_unix_socket::UnixFrame) {
    if !bench_trace_enabled() {
        return;
    }
    let id_raw = match frame.id() {
        embedded_can::Id::Standard(v) => v.as_raw() as u32,
        embedded_can::Id::Extended(v) => v.as_raw(),
    };
    eprintln!("[{prefix}] id=0x{id_raw:08X} dlc={}", frame.dlc());
}

impl AsyncTxFrameIo for AsyncUnixCanTx {
    type Frame = embedded_can_unix_socket::UnixFrame;
    type Error = embedded_can_unix_socket::UnixCanError;

    async fn send(&mut self, frame: &Self::Frame) -> Result<(), Self::Error> {
        loop {
            match self.send_timeout(frame, IO_SLICE).await {
                Ok(()) => return Ok(()),
                Err(embedded_can_unix_socket::UnixCanError::Timeout)
                | Err(embedded_can_unix_socket::UnixCanError::WouldBlock) => {
                    tokio::task::yield_now().await;
                }
                Err(err) => return Err(err),
            }
        }
    }

    async fn send_timeout(
        &mut self,
        frame: &Self::Frame,
        timeout: Duration,
    ) -> Result<(), Self::Error> {
        let deadline = Instant::now() + timeout;
        let frame = *frame;

        loop {
            let now = Instant::now();
            if now >= deadline {
                return Err(embedded_can_unix_socket::UnixCanError::Timeout);
            }
            let slice = core::cmp::min(IO_SLICE, deadline.duration_since(now));
            let inner = self.inner.clone();
            let res = tokio::task::spawn_blocking(move || {
                let mut guard = inner.lock().map_err(|_| {
                    embedded_can_unix_socket::UnixCanError::Protocol("mutex poisoned")
                })?;
                guard.send_timeout(&frame, slice)
            })
            .await
            .map_err(join_err_to_unix_can)?;

            match res {
                Ok(()) => {
                    trace_frame("tx", &frame);
                    return Ok(());
                }
                Err(embedded_can_unix_socket::UnixCanError::Timeout)
                | Err(embedded_can_unix_socket::UnixCanError::WouldBlock) => {
                    tokio::task::yield_now().await;
                }
                Err(err) => return Err(err),
            }
        }
    }
}

impl AsyncRxFrameIo for AsyncUnixCanRx {
    type Frame = embedded_can_unix_socket::UnixFrame;
    type Error = embedded_can_unix_socket::UnixCanError;

    async fn recv(&mut self) -> Result<Self::Frame, Self::Error> {
        match self.events.recv().await {
            Some(Ok(frame)) => {
                trace_frame("rx", &frame);
                Ok(frame)
            }
            Some(Err(err)) => Err(err),
            None => Err(embedded_can_unix_socket::UnixCanError::Disconnected),
        }
    }

    async fn recv_timeout(&mut self, timeout: Duration) -> Result<Self::Frame, Self::Error> {
        match tokio::time::timeout(timeout, self.events.recv()).await {
            Ok(Some(Ok(frame))) => {
                trace_frame("rx", &frame);
                Ok(frame)
            }
            Ok(Some(Err(err))) => Err(err),
            Ok(None) => Err(embedded_can_unix_socket::UnixCanError::Disconnected),
            Err(_) => Err(embedded_can_unix_socket::UnixCanError::Timeout),
        }
    }

    async fn wait_not_empty(&mut self) -> Result<(), Self::Error> {
        tokio::task::yield_now().await;
        Ok(())
    }
}

type Demux = can_iso_tp::IsoTpAsyncDemux<
    'static,
    AsyncUnixCanTx,
    AsyncUnixCanRx,
    embedded_can_unix_socket::UnixFrame,
    can_iso_tp::StdClock,
    MAX_PEERS,
>;

#[derive(Clone)]
pub struct BenchNode {
    rt: TokioRuntime,
    demux: Arc<tokio::sync::Mutex<Demux>>,
}

impl BenchNode {
    fn new(demux: Demux) -> Self {
        Self {
            rt: TokioRuntime,
            demux: Arc::new(tokio::sync::Mutex::new(demux)),
        }
    }
}

impl IsoTpAsyncEndpoint for BenchNode {
    type Error = can_iso_tp::IsoTpError<embedded_can_unix_socket::UnixCanError>;

    async fn send_to(
        &mut self,
        to: u8,
        payload: &[u8],
        timeout: Duration,
    ) -> Result<(), SendError<Self::Error>> {
        let mut demux = self.demux.lock().await;
        let mut app = demux.app();
        app.send_to(&self.rt, to, payload, timeout)
            .await
            .map_err(|e| match e {
                can_iso_tp::IsoTpError::Timeout(_) => SendError::Timeout,
                other => SendError::Backend(other),
            })
    }

    async fn send_functional_to(
        &mut self,
        functional_to: u8,
        payload: &[u8],
        timeout: Duration,
    ) -> Result<(), SendError<Self::Error>> {
        let mut demux = self.demux.lock().await;
        let mut app = demux.app();
        app.send_functional_to(&self.rt, functional_to, payload, timeout)
            .await
            .map_err(|e| match e {
                can_iso_tp::IsoTpError::Timeout(_) => SendError::Timeout,
                other => SendError::Backend(other),
            })
    }

    async fn recv_one<Cb>(
        &mut self,
        timeout: Duration,
        mut on_payload: Cb,
    ) -> Result<RecvStatus, RecvError<Self::Error>>
    where
        Cb: FnMut(RecvMeta, &[u8]) -> Result<RecvControl, Self::Error>,
    {
        let mut demux = self.demux.lock().await;
        let mut app = demux.app();
        let mut out = [0u8; MAX_ISOTP_PAYLOAD];
        match app.recv_next_into(&self.rt, timeout, &mut out).await {
            Ok(None) => Ok(RecvStatus::TimedOut),
            Ok(Some((reply_to, len))) => {
                let _ =
                    on_payload(RecvMeta { reply_to }, &out[..len]).map_err(RecvError::Backend)?;
                Ok(RecvStatus::DeliveredOne)
            }
            Err(can_iso_tp::AppRecvIntoError::BufferTooSmall { needed, got }) => {
                Err(RecvError::BufferTooSmall { needed, got })
            }
            Err(can_iso_tp::AppRecvIntoError::IsoTp(can_iso_tp::IsoTpError::Timeout(_))) => {
                Ok(RecvStatus::TimedOut)
            }
            Err(can_iso_tp::AppRecvIntoError::IsoTp(e)) => Err(RecvError::Backend(e)),
        }
    }
}

fn connect_with_retry(
    socket_path: &Path,
    wait_timeout: Duration,
) -> Result<embedded_can_unix_socket::UnixCan> {
    let started = Instant::now();

    loop {
        match embedded_can_unix_socket::UnixCan::connect(socket_path) {
            Ok(can) => {
                if bench_trace_enabled() {
                    eprintln!("[connect] connected to {}", socket_path.display());
                }
                return Ok(can);
            }
            Err(err)
                if matches!(
                    err.kind(),
                    ErrorKind::NotFound | ErrorKind::ConnectionRefused
                ) =>
            {
                if started.elapsed() >= wait_timeout {
                    return Err(err).with_context(|| {
                        format!(
                            "connect unix CAN at {} (waited {:.3}s)",
                            socket_path.display(),
                            wait_timeout.as_secs_f64()
                        )
                    });
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(err) => {
                return Err(err).with_context(|| {
                    format!(
                        "connect unix CAN at {} (non-retryable)",
                        socket_path.display()
                    )
                });
            }
        }
    }
}

pub fn connect_bench_node(
    socket_path: &Path,
    local_addr: u8,
    wait_timeout: Duration,
) -> Result<BenchNode> {
    let tx_can = connect_with_retry(socket_path, wait_timeout)?;
    let mut rx_can = connect_with_retry(socket_path, wait_timeout)?;
    let (events_tx, events_rx) = tokio::sync::mpsc::unbounded_channel();

    std::thread::Builder::new()
        .name(format!("thincan-bench-rx-{local_addr:02x}"))
        .spawn(move || {
            loop {
                match rx_can.recv() {
                    Ok(frame) => {
                        if events_tx.send(Ok(frame)).is_err() {
                            break;
                        }
                    }
                    Err(embedded_can_unix_socket::UnixCanError::Timeout)
                    | Err(embedded_can_unix_socket::UnixCanError::WouldBlock) => continue,
                    Err(err) => {
                        let _ = events_tx.send(Err(err));
                        break;
                    }
                }
            }
        })
        .context("spawn unix CAN RX forwarder thread")?;

    let tx = AsyncUnixCanTx {
        inner: Arc::new(Mutex::new(tx_can)),
    };
    let rx = AsyncUnixCanRx { events: events_rx };

    let mut cfg = can_iso_tp::IsoTpConfig::default();
    cfg.max_payload_len = MAX_ISOTP_PAYLOAD;
    cfg.frame_len = 64;
    cfg.block_size = 0;

    let storages: [can_iso_tp::RxStorage<'static>; MAX_PEERS] =
        core::array::from_fn(|_| can_iso_tp::RxStorage::Owned(vec![0u8; MAX_ISOTP_PAYLOAD]));

    let demux = can_iso_tp::IsoTpAsyncDemux::new(
        tx,
        rx,
        cfg,
        can_iso_tp::StdClock,
        local_addr,
        None,
        storages,
    )
    .map_err(|e| anyhow::anyhow!("failed to create async ISO-TP demux: {e:?}"))?;

    Ok(BenchNode::new(demux))
}

pub type BenchInterface =
    maplet::Interface<thincan::NoopRawMutex, BenchNode, Vec<u8>, DEPTH, MAX_BODY, MAX_WAITERS>;

pub type BenchBundles<'a> =
    maplet::Bundles<'a, thincan::NoopRawMutex, BenchNode, Vec<u8>, DEPTH, MAX_BODY, MAX_WAITERS>;

pub fn build_interface(node: BenchNode) -> BenchInterface {
    maplet::Interface::<thincan::NoopRawMutex, _, _, DEPTH, MAX_BODY, MAX_WAITERS>::new(
        node,
        vec![0u8; TX_BUF_LEN],
    )
}

pub fn default_send_config() -> thincan_file_transfer::SendConfig {
    thincan_file_transfer::SendConfig {
        sender_max_chunk_size: SEND_CHUNK_SIZE,
        window_chunks: SEND_WINDOW_CHUNKS,
        max_retries: SEND_MAX_RETRIES,
    }
}

pub fn default_receiver_config() -> thincan_file_transfer::ReceiverConfig {
    thincan_file_transfer::ReceiverConfig {
        max_chunk_size: SEND_CHUNK_SIZE as u32,
    }
}

#[derive(Debug, Default)]
pub struct MemoryStore {
    pub bytes: Vec<u8>,
}

impl thincan_file_transfer::AsyncFileStore for MemoryStore {
    type Error = anyhow::Error;
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
        if end > self.bytes.len() {
            anyhow::bail!("store write out of bounds: {end} > {}", self.bytes.len());
        }
        self.bytes[offset..end].copy_from_slice(bytes);
        Ok(())
    }

    async fn commit(&mut self, _handle: Self::WriteHandle) -> Result<(), Self::Error> {
        Ok(())
    }

    async fn abort(&mut self, _handle: Self::WriteHandle) {}
}

pub async fn run_ingress_pump(
    mut node: BenchNode,
    iface: Arc<BenchInterface>,
    stop: Arc<AtomicBool>,
    recv_timeout: Duration,
) -> Result<()> {
    let trace = bench_trace_enabled();
    if trace {
        eprintln!("[ingest] pump started");
    }
    while !stop.load(Ordering::Relaxed) {
        let mut delivered: Option<(u8, Vec<u8>)> = None;
        match node
            .recv_one(recv_timeout, |meta, payload| {
                delivered = Some((meta.reply_to, payload.to_vec()));
                Ok(RecvControl::Continue)
            })
            .await
        {
            Ok(RecvStatus::TimedOut) => {}
            Ok(RecvStatus::DeliveredOne) => {
                if let Some((from, payload)) = delivered.take() {
                    if trace {
                        let id = if payload.len() >= 2 {
                            u16::from_le_bytes([payload[0], payload[1]])
                        } else {
                            0
                        };
                        eprintln!(
                            "[ingest] from=0x{from:02X} id=0x{id:04X} len={}",
                            payload.len().saturating_sub(2)
                        );
                    }
                    iface
                        .handle()
                        .ingest(from, &payload)
                        .await
                        .map_err(|e| anyhow::anyhow!("thincan ingest error: {e:?}"))?;
                }
            }
            Err(RecvError::BufferTooSmall { needed, got }) => {
                anyhow::bail!("ingress pump buffer too small: needed={needed} got={got}");
            }
            Err(RecvError::Backend(e)) => {
                anyhow::bail!("ingress pump transport error: {e:?}");
            }
        }
    }
    Ok(())
}

pub fn mib_per_sec(bytes: usize, elapsed: Duration) -> f64 {
    let secs = elapsed.as_secs_f64();
    if secs <= 0.0 {
        return f64::INFINITY;
    }
    (bytes as f64 / (1024.0 * 1024.0)) / secs
}

fn bench_trace_enabled() -> bool {
    std::env::var_os("THINCAN_BENCH_TRACE").is_some()
}
