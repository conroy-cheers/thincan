#![cfg_attr(all(feature = "uds", feature = "socketcan"), allow(dead_code))]

// If both backends are enabled (e.g. via `cargo test --all-features`), prefer UDS by default so
// the binary remains buildable. To force SocketCAN, disable default features and enable only
// `socketcan`.

#[cfg(not(any(feature = "uds", feature = "socketcan")))]
compile_error!("Enable either the 'uds' or 'socketcan' feature.");

use anyhow::{Context, Result, bail};
use can_isotp_interface::{IsoTpEndpointMeta as _, RecvControl};
use clap::{Parser, Subcommand};
use embedded_can_interface::{FilterConfig, RxFrameIo, TxFrameIo};
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const IO_TIMEOUT: Duration = Duration::from_secs(5);

thincan::bus_atlas! {
    pub mod atlas {
        0x0100 => Ping(len = 4);
        0x0101 => Pong(len = 4);
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
    about = "Thincan example CLI using ISO-TP (ping/pong)"
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

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Listen and respond to `Ping` with `Pong`.
    Listen,
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

#[cfg(all(feature = "socketcan", not(feature = "uds")))]
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

fn run_listen(cli: &Cli) -> Result<()> {
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
    let mut iface = thincan::Interface::new(node, (), tx_buf.as_mut_slice());

    let mut pending_pongs: VecDeque<(u8, u32)> = VecDeque::new();

    loop {
        let _ = iface
            .node_mut()
            .recv_one_meta(Duration::from_millis(1000), |meta, payload| {
                let raw = match thincan::decode_wire(payload) {
                    Ok(r) => r,
                    Err(_) => return Ok(RecvControl::Continue),
                };
                if raw.id == <atlas::Ping as thincan::Message>::ID && raw.body.len() == 4 {
                    let seq = u32::from_le_bytes(raw.body.try_into().unwrap());
                    pending_pongs.push_back((meta.reply_to, seq));
                }
                Ok(RecvControl::Continue)
            })
            .map_err(|e| anyhow::anyhow!("recv error: {:?}", e))?;

        while let Some((to, seq)) = pending_pongs.pop_front() {
            iface.send_msg_to::<atlas::Pong>(to, &seq.to_le_bytes(), IO_TIMEOUT)?;
        }
    }
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
    let mut iface = thincan::Interface::new(node, (), tx_buf.as_mut_slice());

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
        let _ = iface
            .node_mut()
            .recv_one_meta(Duration::from_millis(100), |meta, payload| {
                if meta.reply_to != dest {
                    return Ok(RecvControl::Continue);
                }
                let raw = match thincan::decode_wire(payload) {
                    Ok(r) => r,
                    Err(_) => return Ok(RecvControl::Continue),
                };
                if raw.id == <atlas::Pong as thincan::Message>::ID && raw.body.len() == 4 {
                    let got_seq = u32::from_le_bytes(raw.body.try_into().unwrap());
                    if got_seq == seq {
                        got = true;
                        return Ok(RecvControl::Stop);
                    }
                }
                Ok(RecvControl::Continue)
            })
            .map_err(|e| anyhow::anyhow!("recv error: {:?}", e))?;

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
        Command::Listen => run_listen(&cli),
        Command::Ping { to } => run_ping(&cli, *to),
    }
}
