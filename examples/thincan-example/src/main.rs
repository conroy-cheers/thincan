#![cfg_attr(all(feature = "uds", feature = "socketcan"), allow(dead_code))]

#[cfg(all(feature = "uds", feature = "socketcan"))]
compile_error!(
    "Enable only one of the 'uds' or 'socketcan' features at a time. \
Note: 'uds' is enabled by default; to build SocketCAN use `--no-default-features --features socketcan`."
);

#[cfg(not(any(feature = "uds", feature = "socketcan")))]
compile_error!("Enable either the 'uds' or 'socketcan' feature.");

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use embedded_can_interface::{FilterConfig, RxFrameIo, TxFrameIo};
use std::collections::VecDeque;
use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const IO_TIMEOUT: Duration = Duration::from_secs(5);

thincan::bus_atlas! {
    pub mod atlas {
        0x0100 => Ping(len = 4);
        0x0101 => Pong(len = 4);
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
        use msgs [Ping, Pong, FileReq, FileChunk, FileAck];
        handles {}
        unhandled_by_default = true;
        ignore [];
    }
}

#[derive(Default)]
struct AppHandlers;

impl<'a> maplet::Handlers<'a> for AppHandlers {
    type Error = ();
}

struct NullStore;

impl thincan_file_transfer::FileStore for NullStore {
    type Error = core::convert::Infallible;
    type WriteHandle = ();

    fn begin_write(
        &mut self,
        _transfer_id: u32,
        _total_len: u32,
    ) -> Result<Self::WriteHandle, Self::Error> {
        Ok(())
    }

    fn write_at(
        &mut self,
        _handle: &mut Self::WriteHandle,
        _offset: u32,
        _bytes: &[u8],
    ) -> Result<(), Self::Error> {
        Ok(())
    }

    fn commit(&mut self, _handle: Self::WriteHandle) -> Result<(), Self::Error> {
        Ok(())
    }

    fn abort(&mut self, _handle: Self::WriteHandle) {}
}

struct FsStore {
    out_dir: PathBuf,
    committed: VecDeque<PathBuf>,
}

struct FileWrite {
    tmp_path: PathBuf,
    final_path: PathBuf,
    file: fs::File,
}

impl FsStore {
    fn new(out_dir: PathBuf) -> Result<Self> {
        fs::create_dir_all(&out_dir).with_context(|| format!("create {:?}", out_dir))?;
        Ok(Self {
            out_dir,
            committed: VecDeque::new(),
        })
    }

    fn drain_committed(&mut self) -> Vec<PathBuf> {
        self.committed.drain(..).collect()
    }

    fn alloc_paths(&self, transfer_id: u32) -> (PathBuf, PathBuf) {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let tmp_path = self
            .out_dir
            .join(format!("transfer_{transfer_id}_{suffix}.part"));
        let final_path = self
            .out_dir
            .join(format!("transfer_{transfer_id}_{suffix}.bin"));
        (tmp_path, final_path)
    }
}

impl thincan_file_transfer::FileStore for FsStore {
    type Error = std::io::Error;
    type WriteHandle = FileWrite;

    fn begin_write(
        &mut self,
        transfer_id: u32,
        total_len: u32,
    ) -> Result<Self::WriteHandle, Self::Error> {
        let (tmp_path, final_path) = self.alloc_paths(transfer_id);
        let file = fs::File::create(&tmp_path)?;
        file.set_len(total_len as u64)?;
        Ok(FileWrite {
            tmp_path,
            final_path,
            file,
        })
    }

    fn write_at(
        &mut self,
        handle: &mut Self::WriteHandle,
        offset: u32,
        bytes: &[u8],
    ) -> Result<(), Self::Error> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::FileExt as _;
            let mut off = offset as u64;
            let mut remaining = bytes;
            while !remaining.is_empty() {
                let wrote = handle.file.write_at(remaining, off)?;
                if wrote == 0 {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::WriteZero,
                        "write_at returned 0",
                    ));
                }
                off += wrote as u64;
                remaining = &remaining[wrote..];
            }
            Ok(())
        }
        #[cfg(not(unix))]
        {
            use std::io::{Seek, SeekFrom};
            handle.file.seek(SeekFrom::Start(offset as u64))?;
            handle.file.write_all(bytes)
        }
    }

    fn commit(&mut self, mut handle: Self::WriteHandle) -> Result<(), Self::Error> {
        handle.file.flush()?;
        drop(handle.file);
        fs::rename(&handle.tmp_path, &handle.final_path)?;
        self.committed.push_back(handle.final_path);
        Ok(())
    }

    fn abort(&mut self, handle: Self::WriteHandle) {
        let _ = fs::remove_file(handle.tmp_path);
    }
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

#[derive(Parser)]
#[command(
    author,
    version,
    about = "Thincan example CLI using ISO-TP and file transfer"
)]
struct Cli {
    /// Local UDS node address (8-bit) in hex (0x12) or decimal.
    #[arg(long, value_parser = parse_uds_addr)]
    id: u8,

    #[cfg(feature = "uds")]
    /// Path to the embedded-can-unix-socket server socket.
    #[arg(long, default_value = "/tmp/embedded-can-unix-socket.sock")]
    uds_socket: PathBuf,

    #[cfg(feature = "socketcan")]
    /// SocketCAN interface name (e.g. can0).
    #[arg(long)]
    iface: String,

    /// ISO-TP: number of consecutive frames per flow-control block (0 = unlimited).
    #[arg(long, default_value_t = 0)]
    isotp_block_size: u8,

    /// ISO-TP: CAN frame payload length (8 for classic CAN, 64 for CAN FD).
    #[arg(long, default_value_t = 8)]
    isotp_frame_len: usize,

    /// File transfer: payload size per FileChunk message.
    #[arg(long, default_value_t = 1024)]
    file_chunk_size: usize,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Listen for messages and receive files into the output directory.
    Listen {
        /// Output directory for received files.
        #[arg(long)]
        out_dir: PathBuf,
    },
    /// Send a file to a peer ID.
    Send {
        /// Destination UDS node address.
        #[arg(long, value_parser = parse_uds_addr)]
        to: u8,
        /// File path to send.
        file: PathBuf,
    },
    /// Ping a peer ID and print the result.
    Ping {
        /// Destination UDS node address.
        #[arg(long, value_parser = parse_uds_addr)]
        to: u8,
    },
}

fn parse_uds_addr(raw: &str) -> Result<u8, String> {
    let raw = raw.trim();
    let value = if let Some(hex) = raw.strip_prefix("0x") {
        u32::from_str_radix(hex, 16).map_err(|_| "invalid hex address".to_string())?
    } else {
        raw.parse::<u32>()
            .map_err(|_| "invalid address".to_string())?
    };
    if value > 0xFF {
        return Err("address must fit in 8 bits (0x00..0xFF)".to_string());
    }
    Ok(value as u8)
}

#[cfg(feature = "uds")]
fn open_can(cli: &Cli) -> Result<embedded_can_unix_socket::UnixCan> {
    embedded_can_unix_socket::UnixCan::connect(&cli.uds_socket).context("connect to uds server")
}

#[cfg(feature = "socketcan")]
fn open_can(cli: &Cli) -> Result<embedded_can_socketcan::SocketCan> {
    embedded_can_socketcan::SocketCan::open(&cli.iface).context("open socketcan interface")
}

const MAX_ISO_TP_PAYLOAD: usize = 4095;
const MAX_PEERS: usize = 16;

fn base_isotp_cfg() -> can_iso_tp::IsoTpConfig {
    let mut cfg = can_iso_tp::IsoTpConfig::default();
    cfg.max_payload_len = MAX_ISO_TP_PAYLOAD;
    cfg
}

fn isotp_cfg_for_cli(cli: &Cli) -> can_iso_tp::IsoTpConfig {
    let mut cfg = base_isotp_cfg();
    cfg.block_size = cli.isotp_block_size;
    cfg.frame_len = cli.isotp_frame_len;
    cfg
}

fn run_listen(cli: &Cli, out_dir: PathBuf) -> Result<()> {
    let mut can = open_can(cli)?;

    FilterConfig::set_filters(&mut can, &[can_uds::uds29::filter_phys_for_target(cli.id)])
        .context("set CAN acceptance filter")?;

    let (tx, rx) = split_shared(can);

    let mut rx_bufs = [[0u8; MAX_ISO_TP_PAYLOAD]; MAX_PEERS];
    let storages = can_iso_tp::rx_storages_from_buffers(&mut rx_bufs);
    let node = can_iso_tp::IsoTpDemux::new(
        tx,
        rx,
        isotp_cfg_for_cli(cli),
        can_iso_tp::StdClock,
        cli.id,
        storages,
    )
    .map_err(|_| anyhow::anyhow!("failed to build ISO-TP demux"))?;

    let mut tx_buf = vec![0u8; 4096];
    let mut iface = thincan::Interface::new(node, maplet::Router::new(), tx_buf.as_mut_slice());

    let store = FsStore::new(out_dir)?;
    let mut handlers = maplet::HandlersImpl {
        app: AppHandlers::default(),
        file_transfer: thincan_file_transfer::State::new(store),
    };

    let mut pending_pongs: VecDeque<(u8, u32)> = VecDeque::new();

    loop {
        if let Err(err) =
            iface.recv_one_dispatch_with_meta(&mut handlers, Duration::from_millis(1000), |ev| {
                if let thincan::DispatchEventMeta::Dispatched { meta, raw, .. } = ev
                    && raw.id == <atlas::Ping as thincan::Message>::ID
                    && raw.body.len() == 4
                {
                    let seq = u32::from_le_bytes(raw.body.try_into().unwrap());
                    pending_pongs.push_back((meta.reply_to, seq));
                }
                Ok(thincan::RecvControl::Continue)
            })
        {
            eprintln!("recv error: {:?}", err.kind);
            continue;
        }

        while let Some((to, seq)) = pending_pongs.pop_front() {
            iface.send_msg_to::<atlas::Pong>(to, &seq.to_le_bytes(), IO_TIMEOUT)?;
        }

        for path in handlers.file_transfer.store.drain_committed() {
            println!("received file: {}", path.display());
        }
    }
}

fn run_send(cli: &Cli, dest: u8, file: PathBuf) -> Result<()> {
    let bytes = fs::read(&file).with_context(|| format!("read {}", file.display()))?;
    let mut can = open_can(cli)?;

    FilterConfig::set_filters(&mut can, &[can_uds::uds29::filter_phys_for_target(cli.id)])
        .context("set CAN acceptance filter")?;

    let (tx, rx) = split_shared(can);

    let mut rx_bufs = [[0u8; MAX_ISO_TP_PAYLOAD]; MAX_PEERS];
    let storages = can_iso_tp::rx_storages_from_buffers(&mut rx_bufs);
    let node = can_iso_tp::IsoTpDemux::new(
        tx,
        rx,
        isotp_cfg_for_cli(cli),
        can_iso_tp::StdClock,
        cli.id,
        storages,
    )
    .map_err(|_| anyhow::anyhow!("failed to build ISO-TP demux"))?;

    let mut tx_buf = vec![0u8; 4096];
    let mut iface = thincan::Interface::new(node, maplet::Router::new(), tx_buf.as_mut_slice());

    let mut sender =
        thincan_file_transfer::Sender::<atlas::Atlas>::with_chunk_size(cli.file_chunk_size)
            .map_err(|_| anyhow::anyhow!("invalid --file-chunk-size"))?;
    let start = Instant::now();
    let result = sender.send_file_to(&mut iface, dest, &bytes, IO_TIMEOUT)?;
    let elapsed = start.elapsed();
    let mib_s = (bytes.len() as f64 / (1024.0 * 1024.0)) / elapsed.as_secs_f64();

    println!(
        "sent file: {} (transfer {}, {} bytes, {} chunks) in {:.3?} ({:.2} MiB/s)",
        file.display(),
        result.transfer_id,
        result.total_len,
        result.chunks,
        elapsed,
        mib_s,
    );
    Ok(())
}

fn run_ping(cli: &Cli, dest: u8) -> Result<()> {
    let mut can = open_can(cli)?;

    FilterConfig::set_filters(&mut can, &[can_uds::uds29::filter_phys_for_target(cli.id)])
        .context("set CAN acceptance filter")?;

    let (tx, rx) = split_shared(can);
    let mut rx_bufs = [[0u8; MAX_ISO_TP_PAYLOAD]; MAX_PEERS];
    let storages = can_iso_tp::rx_storages_from_buffers(&mut rx_bufs);
    let node = can_iso_tp::IsoTpDemux::new(
        tx,
        rx,
        isotp_cfg_for_cli(cli),
        can_iso_tp::StdClock,
        cli.id,
        storages,
    )
    .map_err(|_| anyhow::anyhow!("failed to build ISO-TP demux"))?;

    let mut tx_buf = vec![0u8; 4096];
    let mut iface = thincan::Interface::new(node, maplet::Router::new(), tx_buf.as_mut_slice());
    let mut handlers = maplet::HandlersImpl {
        app: AppHandlers::default(),
        file_transfer: thincan_file_transfer::State::new(NullStore),
    };

    let seq = (SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis()
        & 0xFFFF_FFFF) as u32;
    iface.send_msg_to::<atlas::Ping>(dest, &seq.to_le_bytes(), IO_TIMEOUT)?;

    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        if Instant::now() >= deadline {
            bail!("ping timed out");
        }
        let mut got = false;
        let _ =
            iface.recv_one_dispatch_with_meta(&mut handlers, Duration::from_millis(100), |ev| {
                if let thincan::DispatchEventMeta::Dispatched { meta, raw, .. } = ev
                    && meta.reply_to == dest
                    && raw.id == <atlas::Pong as thincan::Message>::ID
                    && raw.body.len() == 4
                {
                    let got_seq = u32::from_le_bytes(raw.body.try_into().unwrap());
                    if got_seq == seq {
                        got = true;
                    }
                }
                Ok(thincan::RecvControl::Continue)
            })?;
        if got {
            println!("pong {}", seq);
            return Ok(());
        }
    }
}

fn main() -> Result<()> {
    if cfg!(debug_assertions) {
        eprintln!("warning: running a debug build; use `cargo run --release` for performance");
    }
    let cli = Cli::parse();
    match &cli.command {
        Command::Listen { out_dir } => run_listen(&cli, out_dir.clone()),
        Command::Send { to, file } => run_send(&cli, *to, file.clone()),
        Command::Ping { to } => run_ping(&cli, *to),
    }
}
