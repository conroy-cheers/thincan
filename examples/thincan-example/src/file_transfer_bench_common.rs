use anyhow::{Context, Result};
use core::time::Duration;
use embedded_can_interface::{AsyncRxFrameIo, AsyncTxFrameIo, RxFrameIo, TxFrameIo};
use std::io::ErrorKind;
use std::path::Path;
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

#[derive(Clone, Copy)]
pub struct TokioClock;
impl can_iso_tp::Clock for TokioClock {
    type Instant = tokio::time::Instant;
    fn now(&self) -> Self::Instant {
        Self::Instant::now()
    }
    fn elapsed(&self, earlier: Self::Instant) -> Duration {
        earlier.elapsed()
    }
    fn add(&self, instant: Self::Instant, duration: Duration) -> Self::Instant {
        instant
            .checked_add(duration)
            .expect("protocol timer representable")
    }
}

pub struct AsyncUnixCanTx {
    inner: Arc<Mutex<embedded_can_unix_socket::UnixCan>>,
}
pub struct AsyncUnixCanRx {
    events: tokio::sync::mpsc::Receiver<
        Result<embedded_can_unix_socket::UnixFrame, embedded_can_unix_socket::UnixCanError>,
    >,
    control: embedded_can_unix_socket::UnixCanControl,
}
impl Drop for AsyncUnixCanRx {
    fn drop(&mut self) {
        let _ = self.control.shutdown();
    }
}
impl AsyncTxFrameIo for AsyncUnixCanTx {
    type Frame = embedded_can_unix_socket::UnixFrame;
    type Error = embedded_can_unix_socket::UnixCanError;
    async fn send(&mut self, frame: &Self::Frame) -> Result<(), Self::Error> {
        self.send_timeout(frame, Duration::from_secs(1)).await
    }
    async fn send_timeout(
        &mut self,
        frame: &Self::Frame,
        timeout: Duration,
    ) -> Result<(), Self::Error> {
        let frame = *frame;
        let inner = self.inner.clone();
        // One invocation: a timed-out acknowledgement never causes a frame replay.
        tokio::task::spawn_blocking(move || {
            inner
                .lock()
                .map_err(|_| Self::Error::Protocol("mutex poisoned"))?
                .send_timeout(&frame, timeout)
        })
        .await
        .map_err(|error| Self::Error::Io(std::io::Error::other(error.to_string())))?
    }
}
impl AsyncRxFrameIo for AsyncUnixCanRx {
    type Frame = embedded_can_unix_socket::UnixFrame;
    type Error = embedded_can_unix_socket::UnixCanError;
    async fn recv(&mut self) -> Result<Self::Frame, Self::Error> {
        self.events.recv().await.ok_or(Self::Error::Disconnected)?
    }
    async fn recv_timeout(&mut self, timeout: Duration) -> Result<Self::Frame, Self::Error> {
        tokio::time::timeout(timeout, self.recv())
            .await
            .map_err(|_| Self::Error::Timeout)?
    }
    async fn wait_not_empty(&mut self) -> Result<(), Self::Error> {
        Err(Self::Error::Protocol("use cancellation-safe recv"))
    }
}
pub type BackendError = can_iso_tp::IsoTpError<embedded_can_unix_socket::UnixCanError>;
pub type BenchResources = can_isotp_queue::embassy::QueueResources<
    thincan::NoopRawMutex,
    BackendError,
    BackendError,
    tokio::time::Instant,
    MAX_ISOTP_PAYLOAD,
    4,
    4,
>;
pub type BenchNode<'a> = can_isotp_queue::embassy::QueueTx<
    'a,
    thincan::NoopRawMutex,
    BackendError,
    BackendError,
    tokio::time::Instant,
    MAX_ISOTP_PAYLOAD,
    4,
    4,
>;
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

pub fn deadline(duration: Duration) -> Result<tokio::time::Instant> {
    tokio::time::Instant::now()
        .checked_add(duration)
        .context("transfer deadline overflow")
}

pub fn default_send_config() -> thincan_file_transfer::SendConfig {
    thincan_file_transfer::SendConfig {
        sender_max_chunk_size: SEND_CHUNK_SIZE,
        window_chunks: SEND_WINDOW_CHUNKS,
        max_retries: SEND_MAX_RETRIES,
        ack_timeout: core::time::Duration::from_secs(1),
        // The responder may start listening late; it drops repeated offers.
        offer_retry_interval: Some(core::time::Duration::from_millis(200)),
    }
}

pub fn default_receiver_config() -> thincan_file_transfer::ReceiverConfig {
    thincan_file_transfer::ReceiverConfig {
        max_chunk_size: SEND_CHUNK_SIZE as u32,
        ..Default::default()
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

struct CompletionMonitor {
    failure: std::cell::RefCell<Option<BackendError>>,
    changed: tokio::sync::Notify,
}
impl can_iso_tp::driver::DriverHooks<embedded_can_unix_socket::UnixCanError>
    for &CompletionMonitor
{
    fn transmitted(&self, _: can_isotp_interface::Destination, result: Result<(), BackendError>) {
        if let Err(error) = result
            && self.failure.borrow().is_none()
        {
            *self.failure.borrow_mut() = Some(error);
            self.changed.notify_one();
        }
    }
}
/// A node's file-transfer bundles, over transmit handle `W`.
pub type Bundles<'a, W> =
    maplet::Bundles<'a, thincan::NoopRawMutex, W, Vec<u8>, DEPTH, MAX_BODY, MAX_WAITERS>;

/// How [`run_node`] runs a node.
pub struct NodeOptions {
    /// How long to retry connecting while the bus server comes up.
    pub wait_for_socket: Duration,
    /// Run the file-transfer `serve` future, so `recv_file` answers a peer that
    /// missed `Complete` without waiting for its confirmation.
    pub serve: bool,
}

/// Run a node at `local` on the unix-socket CAN bus at `socket`: the ISO-TP
/// driver, the receive pump and optionally `serve`, alongside `app`. `wrap`
/// adapts the queue's transmit handle, for example to inject faults. Returns
/// `app`'s output, or the node's first failure.
///
/// The queue storage is leaked so the transport is `'static`; run each node once.
pub async fn run_node<W, T>(
    socket: &Path,
    local: u8,
    options: NodeOptions,
    wrap: impl FnOnce(BenchNode<'static>) -> W,
    app: impl AsyncFnOnce(&Bundles<'_, W>) -> T,
) -> Result<T>
where
    W: can_isotp_interface::IsoTpAsyncWrite + 'static,
{
    let tx_can = connect_with_retry(socket, options.wait_for_socket)?;
    let mut rx_can = connect_with_retry(socket, options.wait_for_socket)?;
    let control = rx_can.control()?;
    let (events, events_rx) = tokio::sync::mpsc::channel(32);
    std::thread::Builder::new()
        .name(format!("thincan-bench-rx-{local:02x}"))
        .spawn(move || {
            loop {
                let frame = rx_can.recv();
                let failed = frame.is_err();
                if events.blocking_send(frame).is_err() || failed {
                    break;
                }
            }
        })
        .context("spawn bounded raw CAN receiver")?;

    let resources: &'static BenchResources = Box::leak(Box::new(BenchResources::new()));
    let (writer, reader, ports) = resources.split();
    // Boxed: an interface holds its mailboxes inline (hundreds of KiB).
    let iface = Box::new(maplet::Interface::<
        thincan::NoopRawMutex,
        W,
        Vec<u8>,
        DEPTH,
        MAX_BODY,
        MAX_WAITERS,
    >::new(wrap(writer), vec![0; TX_BUF_LEN]));
    let bundles = Bundles::new(&iface);

    let cfg = can_iso_tp::IsoTpConfig {
        max_payload_len: MAX_ISOTP_PAYLOAD,
        frame_len: 64,
        block_size: 0,
        ..Default::default()
    };
    let storages =
        core::array::from_fn(|_| can_iso_tp::RxStorage::Owned(vec![0; MAX_ISOTP_PAYLOAD]));
    let monitor = CompletionMonitor {
        failure: std::cell::RefCell::new(None),
        changed: tokio::sync::Notify::new(),
    };
    let driver =
        can_iso_tp::driver::run_driver::<_, _, _, _, _, _, _, MAX_PEERS, MAX_ISOTP_PAYLOAD, 4, 4>(
            AsyncUnixCanTx {
                inner: Arc::new(Mutex::new(tx_can)),
            },
            AsyncUnixCanRx {
                events: events_rx,
                control,
            },
            TokioClock,
            cfg,
            local,
            None,
            storages,
            ports,
            &monitor,
            core::future::pending(),
        );
    let ingress = async {
        let mut scratch = [0; MAX_ISOTP_PAYLOAD];
        let mut pump = thincan::ReceivePump::new(reader, &*iface, &mut scratch);
        loop {
            match pump.next().await {
                Ok(_) => {}
                Err(error) if error.is_terminal() => {
                    return anyhow::anyhow!("receive pump failed: {error:?}");
                }
                // One bad payload: drop it and keep the transport running.
                Err(error) => eprintln!("[ingress] dropped payload: {error:?}"),
            }
        }
    };
    let serve = async {
        if options.serve {
            let served = bundles.file_transfer.serve().await;
            return anyhow::anyhow!("file-transfer serve stopped: {served:?}");
        }
        core::future::pending().await
    };
    let result = tokio::select! {
        result = driver => Err(anyhow::anyhow!("ISO-TP driver stopped: {result:?}")),
        error = ingress => Err(error),
        () = monitor.changed.notified() => Err(anyhow::anyhow!(
            "ISO-TP transmission failed after acceptance: {:?}",
            monitor.failure.borrow_mut().take()
        )),
        error = serve => Err(error),
        out = app(&bundles) => Ok(out),
    };
    iface.close();
    result
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
