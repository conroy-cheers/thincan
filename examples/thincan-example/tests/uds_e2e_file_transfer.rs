#![cfg(feature = "uds")]

use anyhow::{Context, Result};
use can_iso_tp::IsoTpConfig;
use embedded_can_interface::{FilterConfig, RxFrameIo, TxFrameIo};
use embedded_can_unix_socket::{BusServer, UnixCan};
use std::path::PathBuf;
use std::sync::{
    Arc, Mutex,
    mpsc,
};
use std::thread;
use std::time::{Duration, Instant};

thincan::bus_atlas! {
    pub mod atlas {
        0x1001 => FileReq(capnp = thincan_file_transfer::schema::file_req::Owned);
        0x1002 => FileChunk(capnp = thincan_file_transfer::schema::file_chunk::Owned);
        0x1003 => FileAck(capnp = thincan_file_transfer::schema::file_ack::Owned);
    }
}

impl thincan_file_transfer::Atlas for atlas::Atlas {
    type FileReq = atlas::FileReq;
    type FileChunk = atlas::FileChunk;
    type FileAck = atlas::FileAck;
}

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

#[derive(Default)]
struct NoApp;

impl<'a> maplet::Handlers<'a> for NoApp {
    type Error = ();
}

#[derive(Clone)]
struct SharedTx<T> {
    inner: Arc<Mutex<T>>,
}

#[derive(Clone)]
struct SharedRx<T> {
    inner: Arc<Mutex<T>>,
}

fn split_shared<T>(can: T) -> (SharedTx<T>, SharedRx<T>) {
    let inner = Arc::new(Mutex::new(can));
    (
        SharedTx {
            inner: inner.clone(),
        },
        SharedRx { inner },
    )
}

impl<T> TxFrameIo for SharedTx<T>
where
    T: TxFrameIo,
{
    type Frame = T::Frame;
    type Error = T::Error;

    fn send(&mut self, frame: &Self::Frame) -> Result<(), Self::Error> {
        self.inner.lock().unwrap().send(frame)
    }

    fn try_send(&mut self, frame: &Self::Frame) -> Result<(), Self::Error> {
        self.inner.lock().unwrap().try_send(frame)
    }

    fn send_timeout(&mut self, frame: &Self::Frame, timeout: Duration) -> Result<(), Self::Error> {
        self.inner.lock().unwrap().send_timeout(frame, timeout)
    }
}

impl<T> RxFrameIo for SharedRx<T>
where
    T: RxFrameIo,
{
    type Frame = T::Frame;
    type Error = T::Error;

    fn recv(&mut self) -> Result<Self::Frame, Self::Error> {
        self.inner.lock().unwrap().recv()
    }

    fn try_recv(&mut self) -> Result<Self::Frame, Self::Error> {
        self.inner.lock().unwrap().try_recv()
    }

    fn recv_timeout(&mut self, timeout: Duration) -> Result<Self::Frame, Self::Error> {
        self.inner.lock().unwrap().recv_timeout(timeout)
    }

    fn wait_not_empty(&mut self) -> Result<(), Self::Error> {
        self.inner.lock().unwrap().wait_not_empty()
    }
}

struct SocketCleanup(Option<PathBuf>);

impl Drop for SocketCleanup {
    fn drop(&mut self) {
        if let Some(path) = &self.0 {
            let _ = std::fs::remove_file(path);
        }
    }
}

#[derive(Default)]
struct MemoryFileStore {
    committed: Vec<Vec<u8>>,
}

impl thincan_file_transfer::FileStore for MemoryFileStore {
    type Error = core::convert::Infallible;
    type WriteHandle = Vec<u8>;

    fn begin_write(
        &mut self,
        _transfer_id: u32,
        total_len: u32,
    ) -> Result<Self::WriteHandle, Self::Error> {
        Ok(vec![0u8; total_len as usize])
    }

    fn write_at(
        &mut self,
        handle: &mut Self::WriteHandle,
        offset: u32,
        bytes: &[u8],
    ) -> Result<(), Self::Error> {
        let start = offset as usize;
        let end = start + bytes.len();
        handle[start..end].copy_from_slice(bytes);
        Ok(())
    }

    fn commit(&mut self, handle: Self::WriteHandle) -> Result<(), Self::Error> {
        self.committed.push(handle);
        Ok(())
    }

    fn abort(&mut self, _handle: Self::WriteHandle) {}
}

fn cfg(frame_len: usize) -> IsoTpConfig {
    let mut cfg = IsoTpConfig::default();
    cfg.block_size = 0;
    cfg.st_min = Duration::from_micros(0);
    cfg.max_payload_len = 4095;
    cfg.frame_len = frame_len;
    cfg
}

#[test]
fn uds_simulated_can_bus_transfers_file_end_to_end() -> Result<()> {
    // Use a short, predictable socket path: UDS path limits are small.
    let params = format!("{}:{}", std::process::id(), Instant::now().elapsed().as_nanos());
    let h = blake3::hash(params.as_bytes()).to_hex();
    let socket_path = PathBuf::from("/tmp").join(format!("thincan_e2e_{}.sock", &h.as_str()[..8]));
    let _ = std::fs::remove_file(&socket_path);
    let cleanup = SocketCleanup(Some(socket_path.clone()));

    let server = BusServer::start(&socket_path)
        .with_context(|| format!("start bus server at {}", socket_path.display()))?;

    let from = 0x10u8;
    let to = 0x20u8;

    let file = b"hello over uds simulated can bus\n".to_vec();
    let expected = file.clone();

    let (tx_done, rx_done) = mpsc::channel::<Vec<u8>>();

    let rx_socket = socket_path.clone();
    let rx_thread = thread::spawn(move || -> Result<()> {
        let mut can = UnixCan::connect(&rx_socket)
            .with_context(|| format!("connect rx to {}", rx_socket.display()))?;
        FilterConfig::set_filters(&mut can, &[can_uds::uds29::filter_phys_for_target(to)])
            .context("set rx CAN acceptance filter")?;

        let (tx, rx) = split_shared(can);
        let storages: [can_iso_tp::RxStorage<'static>; 4] =
            std::array::from_fn(|_| can_iso_tp::RxStorage::Owned(vec![0u8; 4095]));
        let node = can_iso_tp::IsoTpDemux::new(tx, rx, cfg(8), can_iso_tp::StdClock, to, storages)
            .map_err(|_| anyhow::anyhow!("failed to build ISO-TP demux"))?;

        let mut tx_buf = vec![0u8; 4096];
        let mut iface = thincan::Interface::new(node, maplet::Router::new(), tx_buf.as_mut_slice());
        let mut handlers = maplet::HandlersImpl {
            app: NoApp::default(),
            file_transfer: thincan_file_transfer::State::new(MemoryFileStore::default()),
        };

        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if Instant::now() > deadline {
                return Err(anyhow::anyhow!("timed out waiting for receiver commit"));
            }
            let _ = iface.recv_one_dispatch_meta(&mut handlers, Duration::from_millis(20));
            if let Some(bytes) = handlers.file_transfer.store.committed.pop() {
                let _ = tx_done.send(bytes);
                return Ok(());
            }
        }
    });

    let mut can = UnixCan::connect(&socket_path)
        .with_context(|| format!("connect tx to {}", socket_path.display()))?;
    FilterConfig::set_filters(&mut can, &[can_uds::uds29::filter_phys_for_target(from)])
        .context("set tx CAN acceptance filter")?;

    let (tx, rx) = split_shared(can);
    let storages: [can_iso_tp::RxStorage<'static>; 4] =
        std::array::from_fn(|_| can_iso_tp::RxStorage::Owned(vec![0u8; 4095]));
    let node =
        can_iso_tp::IsoTpDemux::new(tx, rx, cfg(8), can_iso_tp::StdClock, from, storages)
            .map_err(|_| anyhow::anyhow!("failed to build ISO-TP demux"))?;

    let mut tx_buf = vec![0u8; 4096];
    let mut iface = thincan::Interface::new(node, maplet::Router::new(), tx_buf.as_mut_slice());
    let mut sender = thincan_file_transfer::Sender::<atlas::Atlas>::with_chunk_size(16)?;
    let _ = sender.send_file_to(&mut iface, to, &file, Duration::from_secs(1))?;

    let got = rx_done
        .recv_timeout(Duration::from_secs(2))
        .context("receiver did not commit a file")?;
    assert_eq!(got, expected);

    rx_thread.join().unwrap()?;
    drop(server);
    drop(cleanup);
    Ok(())
}

