//! A node built from the published 0.1 release (`thincan` 0.1.1,
//! `thincan-file-transfer` 0.1.1, `can-iso-tp` 0.2.0), wired as the 0.1
//! benchmark example wired it.

use crate::{Faults, MAX_ISOTP_PAYLOAD};
use can_isotp_interface_01::{
    IsoTpAsyncEndpoint, RecvControl, RecvError, RecvMeta, RecvStatus, SendError,
};
use core::future::Future;
use core::pin::Pin;
use embedded_can_interface::{AsyncRxFrameIo, AsyncTxFrameIo, RxFrameIo, TxFrameIo};
use embedded_can_unix_socket::{UnixCan, UnixCanError, UnixFrame};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const MAX_PEERS: usize = 4;
const DEPTH: usize = 16;
const MAX_BODY: usize = 4096;
const MAX_WAITERS: usize = 8;
const TX_BUF_LEN: usize = 8192;

thincan_01::bus_atlas! {
    pub mod atlas {
        0x1001 => FileReq(capnp = thincan_file_transfer_01::schema::file_req::Owned);
        0x1002 => FileChunk(capnp = thincan_file_transfer_01::schema::file_chunk::Owned);
        0x1003 => FileAck(capnp = thincan_file_transfer_01::schema::file_ack::Owned);
    }
}

pub mod file_transfer_bundle {
    pub type Bundle = thincan_file_transfer_01::FileTransferBundle<super::atlas::Atlas>;
    pub const MESSAGE_COUNT: usize = thincan_file_transfer_01::FILE_TRANSFER_MESSAGE_COUNT;
}

thincan_01::maplet! {
    pub mod maplet: atlas {
        bundles [file_transfer = file_transfer_bundle];
    }
}

impl thincan_file_transfer_01::Atlas for atlas::Atlas {
    type FileReq = atlas::FileReq;
    type FileChunk = atlas::FileChunk;
    type FileAck = atlas::FileAck;
}

pub use thincan_file_transfer_01::{ReceiverConfig, SendConfig};

/// Sender settings like the 0.1 benchmark's.
pub fn send_config() -> SendConfig {
    SendConfig {
        sender_max_chunk_size: crate::CHUNK_SIZE,
        window_chunks: 8,
        max_retries: 5,
    }
}

pub fn receiver_config() -> ReceiverConfig {
    ReceiverConfig {
        max_chunk_size: crate::CHUNK_SIZE as u32,
    }
}

#[derive(Clone, Copy, Default)]
struct TokioRuntime;

impl can_iso_tp_02::AsyncRuntime for TokioRuntime {
    type TimeoutError = can_iso_tp_02::TimedOut;
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
                .map_err(|_| can_iso_tp_02::TimedOut)
        })
    }
}

const IO_SLICE: Duration = Duration::from_millis(20);

#[derive(Clone)]
struct CanTx {
    inner: Arc<Mutex<UnixCan>>,
}

struct CanRx {
    events: tokio::sync::mpsc::UnboundedReceiver<Result<UnixFrame, UnixCanError>>,
}

impl AsyncTxFrameIo for CanTx {
    type Frame = UnixFrame;
    type Error = UnixCanError;

    async fn send(&mut self, frame: &Self::Frame) -> Result<(), Self::Error> {
        loop {
            match self.send_timeout(frame, IO_SLICE).await {
                Ok(()) => return Ok(()),
                Err(UnixCanError::Timeout) | Err(UnixCanError::WouldBlock) => {
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
                return Err(UnixCanError::Timeout);
            }
            let slice = IO_SLICE.min(deadline - now);
            let inner = self.inner.clone();
            let result = tokio::task::spawn_blocking(move || {
                inner
                    .lock()
                    .map_err(|_| UnixCanError::Protocol("mutex poisoned"))?
                    .send_timeout(&frame, slice)
            })
            .await
            .map_err(|e| UnixCanError::Io(std::io::Error::other(e.to_string())))?;
            match result {
                Ok(()) => return Ok(()),
                Err(UnixCanError::Timeout) | Err(UnixCanError::WouldBlock) => {
                    tokio::task::yield_now().await;
                }
                Err(err) => return Err(err),
            }
        }
    }
}

impl AsyncRxFrameIo for CanRx {
    type Frame = UnixFrame;
    type Error = UnixCanError;

    async fn recv(&mut self) -> Result<Self::Frame, Self::Error> {
        self.events.recv().await.ok_or(UnixCanError::Disconnected)?
    }

    async fn recv_timeout(&mut self, timeout: Duration) -> Result<Self::Frame, Self::Error> {
        match tokio::time::timeout(timeout, self.events.recv()).await {
            Ok(Some(frame)) => frame,
            Ok(None) => Err(UnixCanError::Disconnected),
            Err(_) => Err(UnixCanError::Timeout),
        }
    }

    async fn wait_not_empty(&mut self) -> Result<(), Self::Error> {
        tokio::task::yield_now().await;
        Ok(())
    }
}

type Demux = can_iso_tp_02::IsoTpAsyncDemux<
    'static,
    CanTx,
    CanRx,
    UnixFrame,
    can_iso_tp_02::StdClock,
    MAX_PEERS,
>;

/// The 0.1 ISO-TP endpoint, dropping outgoing messages that `faults` selects.
#[derive(Clone)]
pub struct Endpoint {
    demux: Arc<tokio::sync::Mutex<Demux>>,
    faults: Faults,
}

type IsoTpError = can_iso_tp_02::IsoTpError<UnixCanError>;

fn send_error(error: IsoTpError) -> SendError<IsoTpError> {
    match error {
        can_iso_tp_02::IsoTpError::Timeout(_) => SendError::Timeout,
        other => SendError::Backend(other),
    }
}

impl IsoTpAsyncEndpoint for Endpoint {
    type Error = IsoTpError;

    async fn send_to(
        &mut self,
        to: u8,
        payload: &[u8],
        timeout: Duration,
    ) -> Result<(), SendError<Self::Error>> {
        if self.faults.drops(payload) {
            return Ok(());
        }
        let mut demux = self.demux.lock().await;
        demux
            .app()
            .send_to(&TokioRuntime, to, payload, timeout)
            .await
            .map_err(send_error)
    }

    async fn send_functional_to(
        &mut self,
        functional_to: u8,
        payload: &[u8],
        timeout: Duration,
    ) -> Result<(), SendError<Self::Error>> {
        let mut demux = self.demux.lock().await;
        demux
            .app()
            .send_functional_to(&TokioRuntime, functional_to, payload, timeout)
            .await
            .map_err(send_error)
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
        let mut out = [0u8; MAX_ISOTP_PAYLOAD];
        match demux
            .app()
            .recv_next_into(&TokioRuntime, timeout, &mut out)
            .await
        {
            Ok(None) => Ok(RecvStatus::TimedOut),
            Ok(Some((reply_to, len))) => {
                on_payload(RecvMeta { reply_to }, &out[..len]).map_err(RecvError::Backend)?;
                Ok(RecvStatus::DeliveredOne)
            }
            Err(can_iso_tp_02::AppRecvIntoError::BufferTooSmall { needed, got }) => {
                Err(RecvError::BufferTooSmall { needed, got })
            }
            Err(can_iso_tp_02::AppRecvIntoError::IsoTp(can_iso_tp_02::IsoTpError::Timeout(_))) => {
                Ok(RecvStatus::TimedOut)
            }
            Err(can_iso_tp_02::AppRecvIntoError::IsoTp(e)) => Err(RecvError::Backend(e)),
        }
    }
}

fn connect_endpoint(bus: &Path, local: u8, faults: Faults) -> Endpoint {
    let (tx_can, mut rx_can) = crate::connect(bus);
    let (events, events_rx) = tokio::sync::mpsc::unbounded_channel();
    std::thread::Builder::new()
        .name(format!("compat-0.1-rx-{local:02x}"))
        .spawn(move || {
            loop {
                match rx_can.recv() {
                    Ok(frame) => {
                        if events.send(Ok(frame)).is_err() {
                            break;
                        }
                    }
                    Err(UnixCanError::Timeout) | Err(UnixCanError::WouldBlock) => continue,
                    Err(err) => {
                        let _ = events.send(Err(err));
                        break;
                    }
                }
            }
        })
        .expect("spawn CAN receiver thread");
    let config = can_iso_tp_02::IsoTpConfig {
        max_payload_len: MAX_ISOTP_PAYLOAD,
        frame_len: 64,
        block_size: 0,
        ..Default::default()
    };
    let storages: [can_iso_tp_02::RxStorage<'static>; MAX_PEERS] =
        core::array::from_fn(|_| can_iso_tp_02::RxStorage::Owned(vec![0u8; MAX_ISOTP_PAYLOAD]));
    let demux = can_iso_tp_02::IsoTpAsyncDemux::new(
        CanTx {
            inner: Arc::new(Mutex::new(tx_can)),
        },
        CanRx { events: events_rx },
        config,
        can_iso_tp_02::StdClock,
        local,
        None,
        storages,
    )
    .expect("0.1 ISO-TP demux");
    Endpoint {
        demux: Arc::new(tokio::sync::Mutex::new(demux)),
        faults,
    }
}

pub type Interface =
    maplet::Interface<thincan_01::NoopRawMutex, Endpoint, Vec<u8>, DEPTH, MAX_BODY, MAX_WAITERS>;
pub type Bundles<'a> =
    maplet::Bundles<'a, thincan_01::NoopRawMutex, Endpoint, Vec<u8>, DEPTH, MAX_BODY, MAX_WAITERS>;

/// The 0.1 benchmark's receive pump: poll the endpoint, ingest each payload.
async fn pump(mut endpoint: Endpoint, iface: &Interface) -> String {
    loop {
        let mut delivered = None;
        let status = endpoint
            .recv_one(Duration::from_millis(10), |meta, payload| {
                delivered = Some((meta.reply_to, payload.to_vec()));
                Ok(RecvControl::Continue)
            })
            .await;
        match status {
            Ok(_) => {
                if let Some((from, payload)) = delivered
                    && let Err(error) = iface.bus().ingest(from, &payload).await
                {
                    return format!("0.1 ingest failed: {error:?}");
                }
            }
            Err(error) => return format!("0.1 receive failed: {error:?}"),
        }
    }
}

/// Run `scenario` on a 0.1 node at `local`, with its receive pump running.
pub async fn run<T>(
    bus: &Path,
    local: u8,
    faults: Faults,
    scenario: impl AsyncFnOnce(&mut Bundles<'_>) -> T,
) -> T {
    // Boxed: an interface holds its mailboxes inline (hundreds of KiB).
    Box::pin(run_node(bus, local, faults, scenario)).await
}

async fn run_node<T>(
    bus: &Path,
    local: u8,
    faults: Faults,
    scenario: impl AsyncFnOnce(&mut Bundles<'_>) -> T,
) -> T {
    let endpoint = connect_endpoint(bus, local, faults);
    let iface = Box::new(Interface::new(
        endpoint.clone(),
        endpoint.clone(),
        vec![0; TX_BUF_LEN],
    ));
    let mut bundles = Bundles::new(&iface);
    tokio::select! {
        error = pump(endpoint, &iface) => panic!("{error}"),
        out = scenario(&mut bundles) => out,
    }
}
