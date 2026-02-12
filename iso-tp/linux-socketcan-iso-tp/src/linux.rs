//! Linux kernel ISO-TP socket backend.
//!
//! This crate provides a minimal wrapper around Linux `CAN_ISOTP` sockets and
//! implements the shared `can-isotp-interface` traits.

use can_isotp_interface::{
    IsoTpEndpoint, IsoTpRxFlowControlConfig, RecvControl, RecvError, RecvMeta, RecvStatus,
    RxFlowControl, SendError,
};
use can_uds::uds29;
use core::time::Duration;
use embedded_can::{ExtendedId, Id};
use socket2::{Domain, Protocol, Socket, Type};
use socketcan::CanAddr;
use std::io;
use std::mem::size_of;
use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd, OwnedFd, RawFd};
use std::thread;
use std::time::Instant;

#[cfg(feature = "tokio")]
use tokio::io::unix::AsyncFd;

#[cfg(feature = "tokio")]
use can_isotp_interface::IsoTpAsyncEndpoint;

/// ISO-TP socket error type.
#[derive(Debug)]
pub enum Error {
    /// I/O error from the OS.
    Io(io::Error),
    /// Invalid configuration passed to the constructor.
    InvalidConfig(&'static str),
}

impl From<io::Error> for Error {
    fn from(err: io::Error) -> Self {
        Self::Io(err)
    }
}

impl core::fmt::Display for Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Error::Io(err) => write!(f, "io error: {err}"),
            Error::InvalidConfig(msg) => write!(f, "invalid config: {msg}"),
        }
    }
}

impl std::error::Error for Error {}

/// Socket-level ISO-TP options.
#[derive(Debug, Clone)]
pub struct IsoTpSocketOptions {
    /// Raw kernel flag bits (see `flags` module).
    pub flags: u32,
    /// Frame transmit time (N_As/N_Ar) for the kernel in nanoseconds.
    pub frame_txtime: Option<Duration>,
    /// Extended addressing byte for TX and RX (normal extended addressing).
    pub ext_address: Option<u8>,
    /// Padding byte for transmit frames (enables TX padding flag).
    pub tx_padding: Option<u8>,
    /// Padding byte for receive frames (enables RX padding flag).
    pub rx_padding: Option<u8>,
    /// Separate RX extended address (enables RX_EXT_ADDR flag).
    pub rx_ext_address: Option<u8>,
}

impl Default for IsoTpSocketOptions {
    fn default() -> Self {
        Self {
            flags: 0,
            frame_txtime: None,
            ext_address: None,
            tx_padding: None,
            rx_padding: None,
            rx_ext_address: None,
        }
    }
}

/// Flow control options advertised by the kernel.
#[derive(Debug, Clone, Copy)]
pub struct IsoTpFlowControlOptions {
    /// Block size (0 = unlimited).
    pub block_size: u8,
    /// STmin raw encoding as defined by ISO-TP.
    pub st_min: u8,
    /// Maximum number of wait frames.
    pub wft_max: u8,
}

impl IsoTpFlowControlOptions {
    /// Build a new flow control configuration.
    pub fn new(block_size: u8, st_min: u8, wft_max: u8) -> Self {
        Self {
            block_size,
            st_min,
            wft_max,
        }
    }
}

/// Link-layer options for kernel ISO-TP sockets.
#[derive(Debug, Clone, Copy)]
pub struct IsoTpLinkLayerOptions {
    /// CAN MTU to use (e.g. CAN_MTU or CANFD_MTU).
    pub mtu: u8,
    /// TX data length in bytes for CAN FD (8/12/16/20/24/32/48/64).
    pub tx_dl: u8,
    /// CAN FD flags (e.g. BRS).
    pub tx_flags: u8,
}

impl IsoTpLinkLayerOptions {
    /// Build a new link-layer configuration.
    pub fn new(mtu: u8, tx_dl: u8, tx_flags: u8) -> Self {
        Self {
            mtu,
            tx_dl,
            tx_flags,
        }
    }
}

/// Kernel ISO-TP socket configuration.
#[derive(Debug, Clone)]
pub struct IsoTpKernelOptions {
    /// Maximum payload size to receive into the internal buffer.
    pub max_rx_payload: usize,
    /// Base ISO-TP socket options.
    pub socket: IsoTpSocketOptions,
    /// Flow control options (optional).
    pub flow_control: Option<IsoTpFlowControlOptions>,
    /// Link-layer options (optional).
    pub link_layer: Option<IsoTpLinkLayerOptions>,
    /// Force TX STmin (nanoseconds) regardless of FC frames.
    pub force_tx_stmin: Option<Duration>,
    /// Force RX STmin (nanoseconds) regardless of received CF timestamps.
    pub force_rx_stmin: Option<Duration>,
}

impl Default for IsoTpKernelOptions {
    fn default() -> Self {
        Self {
            max_rx_payload: 4095,
            socket: IsoTpSocketOptions::default(),
            flow_control: None,
            link_layer: None,
            force_tx_stmin: None,
            force_rx_stmin: None,
        }
    }
}

/// Kernel ISO-TP endpoint using a single socket.
#[derive(Debug)]
pub struct SocketCanIsoTp {
    fd: OwnedFd,
    rx_buf: Vec<u8>,
    wft_max: u8,
}

impl SocketCanIsoTp {
    /// Open a kernel ISO-TP socket on `iface` with fixed RX/TX CAN IDs.
    pub fn open(
        iface: &str,
        rx_id: Id,
        tx_id: Id,
        options: &IsoTpKernelOptions,
    ) -> Result<Self, Error> {
        if options.max_rx_payload == 0 {
            return Err(Error::InvalidConfig("max_rx_payload must be > 0"));
        }

        let socket = Socket::new(
            Domain::from(libc::AF_CAN),
            Type::DGRAM,
            Some(Protocol::from(libc::CAN_ISOTP)),
        )?;

        socket.set_nonblocking(true)?;

        // Most ISO-TP socket options must be applied before binding.
        apply_kernel_options(socket.as_raw_fd(), options)?;

        let addr = CanAddr::from_iface_isotp(iface, rx_id, tx_id).map_err(Error::Io)?;
        socket.bind(&addr.into_sock_addr())?;

        let fd = unsafe { OwnedFd::from_raw_fd(socket.into_raw_fd()) };
        let wft_max = options.flow_control.map(|fc| fc.wft_max).unwrap_or(0);
        Ok(Self {
            fd,
            rx_buf: vec![0u8; options.max_rx_payload],
            wft_max,
        })
    }

    /// Update receive-side FlowControl parameters (BS/STmin) used by the kernel.
    ///
    /// This applies `CAN_ISOTP_RECV_FC` at runtime. For best dynamic behavior, enable
    /// [`flags::CAN_ISOTP_DYN_FC_PARMS`] when opening the socket (kernel-dependent).
    pub fn set_rx_flow_control(&mut self, fc: RxFlowControl) -> Result<(), Error> {
        let c = can_isotp_fc_options {
            bs: fc.block_size,
            stmin: duration_to_isotp_stmin(fc.st_min),
            wftmax: self.wft_max,
        };
        let res = unsafe {
            libc::setsockopt(
                self.fd.as_raw_fd(),
                SOL_CAN_ISOTP,
                CAN_ISOTP_RECV_FC,
                &c as *const can_isotp_fc_options as *const libc::c_void,
                size_of::<can_isotp_fc_options>() as libc::socklen_t,
            )
        };
        if res < 0 {
            return Err(Error::Io(io::Error::last_os_error()));
        }
        Ok(())
    }

    fn recv_one_ready<Cb>(&mut self, mut on_payload: Cb) -> Result<RecvStatus, RecvError<Error>>
    where
        Cb: FnMut(&[u8]) -> Result<RecvControl, Error>,
    {
        let read = unsafe {
            libc::recv(
                self.fd.as_raw_fd(),
                self.rx_buf.as_mut_ptr().cast(),
                self.rx_buf.len(),
                libc::MSG_DONTWAIT,
            )
        };
        if read < 0 {
            let err = io::Error::last_os_error();
            if err.kind() == io::ErrorKind::WouldBlock {
                return Ok(RecvStatus::TimedOut);
            }
            return Err(RecvError::Backend(Error::Io(err)));
        }
        if read == 0 {
            return Err(RecvError::Backend(Error::Io(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "iso-tp socket returned 0 bytes",
            ))));
        }
        let payload = &self.rx_buf[..read as usize];
        let _ = on_payload(payload).map_err(RecvError::Backend)?;
        Ok(RecvStatus::DeliveredOne)
    }
}

impl AsRawFd for SocketCanIsoTp {
    fn as_raw_fd(&self) -> RawFd {
        self.fd.as_raw_fd()
    }
}

impl can_isotp_interface::IsoTpEndpoint for SocketCanIsoTp {
    type Error = Error;

    fn send(&mut self, payload: &[u8], timeout: Duration) -> Result<(), SendError<Self::Error>> {
        let deadline = Instant::now() + timeout;
        loop {
            let sent = unsafe {
                libc::send(
                    self.fd.as_raw_fd(),
                    payload.as_ptr().cast(),
                    payload.len(),
                    libc::MSG_DONTWAIT,
                )
            };
            if sent >= 0 {
                return Ok(());
            }

            let err = io::Error::last_os_error();
            if err.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            if err.kind() != io::ErrorKind::WouldBlock {
                return Err(SendError::Backend(Error::Io(err)));
            }

            let now = Instant::now();
            if now >= deadline {
                return Err(SendError::Timeout);
            }
            let remaining = deadline - now;
            let ready = poll_fd(self.fd.as_raw_fd(), libc::POLLOUT, remaining)
                .map_err(Error::Io)
                .map_err(SendError::Backend)?;
            if !ready {
                return Err(SendError::Timeout);
            }
        }
    }

    fn recv_one<Cb>(
        &mut self,
        timeout: Duration,
        mut on_payload: Cb,
    ) -> Result<RecvStatus, RecvError<Self::Error>>
    where
        Cb: FnMut(&[u8]) -> Result<RecvControl, Self::Error>,
    {
        let deadline = Instant::now() + timeout;
        loop {
            let read = unsafe {
                libc::recv(
                    self.fd.as_raw_fd(),
                    self.rx_buf.as_mut_ptr().cast(),
                    self.rx_buf.len(),
                    libc::MSG_DONTWAIT,
                )
            };
            if read >= 0 {
                if read == 0 {
                    return Err(RecvError::Backend(Error::Io(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "iso-tp socket returned 0 bytes",
                    ))));
                }

                let payload = &self.rx_buf[..read as usize];
                let _ = on_payload(payload).map_err(RecvError::Backend)?;
                return Ok(RecvStatus::DeliveredOne);
            }

            let err = io::Error::last_os_error();
            if err.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            if err.kind() != io::ErrorKind::WouldBlock {
                return Err(RecvError::Backend(Error::Io(err)));
            }

            let now = Instant::now();
            if now >= deadline {
                return Ok(RecvStatus::TimedOut);
            }
            let remaining = deadline - now;
            let ready = poll_fd(self.fd.as_raw_fd(), libc::POLLIN, remaining)
                .map_err(Error::Io)
                .map_err(RecvError::Backend)?;
            if !ready {
                return Ok(RecvStatus::TimedOut);
            }
        }
    }
}

impl IsoTpRxFlowControlConfig for SocketCanIsoTp {
    type Error = Error;

    fn set_rx_flow_control(&mut self, fc: RxFlowControl) -> Result<(), Self::Error> {
        SocketCanIsoTp::set_rx_flow_control(self, fc)
    }
}

/// Tokio-native async wrapper around [`SocketCanIsoTp`].
///
/// This uses `tokio::io::unix::AsyncFd` and non-blocking `send`/`recv` syscalls to integrate
/// kernel ISO-TP sockets into an async runtime without blocking threads.
#[cfg(feature = "tokio")]
#[derive(Debug)]
pub struct TokioSocketCanIsoTp {
    io: AsyncFd<SocketCanIsoTp>,
}

#[cfg(feature = "tokio")]
impl TokioSocketCanIsoTp {
    /// Open a kernel ISO-TP socket and wrap it for tokio async I/O.
    pub fn open(
        iface: &str,
        rx_id: Id,
        tx_id: Id,
        options: &IsoTpKernelOptions,
    ) -> Result<Self, Error> {
        let inner = SocketCanIsoTp::open(iface, rx_id, tx_id, options)?;
        let io = AsyncFd::new(inner).map_err(Error::Io)?;
        Ok(Self { io })
    }

    pub fn inner(&self) -> &SocketCanIsoTp {
        self.io.get_ref()
    }

    pub fn inner_mut(&mut self) -> &mut SocketCanIsoTp {
        self.io.get_mut()
    }

    pub fn into_inner(self) -> SocketCanIsoTp {
        self.io.into_inner()
    }
}

#[cfg(feature = "tokio")]
impl IsoTpRxFlowControlConfig for TokioSocketCanIsoTp {
    type Error = Error;

    fn set_rx_flow_control(&mut self, fc: RxFlowControl) -> Result<(), Self::Error> {
        self.io.get_mut().set_rx_flow_control(fc)
    }
}

#[cfg(feature = "tokio")]
impl IsoTpAsyncEndpoint for TokioSocketCanIsoTp {
    type Error = Error;

    async fn send(
        &mut self,
        payload: &[u8],
        timeout: Duration,
    ) -> Result<(), SendError<Self::Error>> {
        let res = tokio::time::timeout(timeout, async {
            loop {
                let fd = self.io.get_ref().as_raw_fd();
                let sent = unsafe {
                    libc::send(
                        fd,
                        payload.as_ptr().cast(),
                        payload.len(),
                        libc::MSG_DONTWAIT,
                    )
                };
                if sent >= 0 {
                    return Ok(());
                }

                let err = io::Error::last_os_error();
                if err.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                if err.kind() != io::ErrorKind::WouldBlock {
                    return Err(SendError::Backend(Error::Io(err)));
                }

                let mut guard = self
                    .io
                    .writable()
                    .await
                    .map_err(|e| SendError::Backend(Error::Io(e)))?;
                guard.clear_ready();
            }
        })
        .await;

        match res {
            Ok(v) => v,
            Err(_) => Err(SendError::Timeout),
        }
    }

    async fn recv_one<Cb>(
        &mut self,
        timeout: Duration,
        mut on_payload: Cb,
    ) -> Result<RecvStatus, RecvError<Self::Error>>
    where
        Cb: FnMut(&[u8]) -> Result<RecvControl, Self::Error>,
    {
        let res = tokio::time::timeout(timeout, async {
            loop {
                match self.io.get_mut().recv_one_ready(&mut on_payload) {
                    Ok(RecvStatus::DeliveredOne) => return Ok(RecvStatus::DeliveredOne),
                    Ok(RecvStatus::TimedOut) => {
                        let mut guard = self
                            .io
                            .readable()
                            .await
                            .map_err(|e| RecvError::Backend(Error::Io(e)))?;
                        guard.clear_ready();
                        continue;
                    }
                    Err(e) => return Err(e),
                }
            }
        })
        .await;

        match res {
            Ok(v) => v,
            Err(_) => Ok(RecvStatus::TimedOut),
        }
    }
}

/// Kernel-backed ISO-TP demux for UDS normal-fixed addressing (`0x18DA_TA_SA`).
#[derive(Debug)]
pub struct KernelUdsDemux {
    iface: String,
    local_addr: u8,
    options: IsoTpKernelOptions,
    rx_flow_control: RxFlowControl,
    peers: Vec<PeerSocket>,
}

#[derive(Debug)]
struct PeerSocket {
    addr: u8,
    socket: SocketCanIsoTp,
}

impl KernelUdsDemux {
    /// Create a demux for `local_addr` on `iface` using the provided kernel options.
    pub fn new(iface: impl Into<String>, local_addr: u8, options: IsoTpKernelOptions) -> Self {
        let rx_flow_control = match options.flow_control {
            Some(fc) => RxFlowControl {
                block_size: fc.block_size,
                st_min: isotp_stmin_to_duration(fc.st_min).unwrap_or(Duration::from_millis(0)),
            },
            None => RxFlowControl {
                block_size: 0,
                st_min: Duration::from_millis(0),
            },
        };
        Self {
            iface: iface.into(),
            local_addr,
            options,
            rx_flow_control,
            peers: Vec::new(),
        }
    }

    /// UDS local address (target address).
    pub fn local_addr(&self) -> u8 {
        self.local_addr
    }

    /// Register a peer address and open a dedicated kernel ISO-TP socket for it.
    pub fn register_peer(&mut self, peer: u8) -> Result<(), Error> {
        let _ = self.ensure_peer(peer)?;
        Ok(())
    }

    fn ensure_peer(&mut self, peer: u8) -> Result<&mut SocketCanIsoTp, Error> {
        if peer == self.local_addr {
            return Err(Error::InvalidConfig(
                "peer address must differ from local_addr",
            ));
        }

        if let Some(idx) = self.peers.iter().position(|p| p.addr == peer) {
            return Ok(&mut self.peers[idx].socket);
        }

        let rx_id = uds_phys_id(self.local_addr, peer);
        let tx_id = uds_phys_id(peer, self.local_addr);
        let mut socket = SocketCanIsoTp::open(&self.iface, rx_id, tx_id, &self.options)?;
        let _ = socket.set_rx_flow_control(self.rx_flow_control);
        self.peers.push(PeerSocket { addr: peer, socket });
        let idx = self.peers.len() - 1;
        Ok(&mut self.peers[idx].socket)
    }
}

impl IsoTpRxFlowControlConfig for KernelUdsDemux {
    type Error = Error;

    fn set_rx_flow_control(&mut self, fc: RxFlowControl) -> Result<(), Self::Error> {
        self.rx_flow_control = fc;
        for peer in self.peers.iter_mut() {
            peer.socket.set_rx_flow_control(fc)?;
        }
        Ok(())
    }
}

impl can_isotp_interface::IsoTpEndpointMeta for KernelUdsDemux {
    type Error = Error;
    type ReplyTo = u8;

    fn send_to(
        &mut self,
        to: Self::ReplyTo,
        payload: &[u8],
        timeout: Duration,
    ) -> Result<(), SendError<Self::Error>> {
        let socket = self.ensure_peer(to).map_err(SendError::Backend)?;
        socket.send(payload, timeout)
    }

    fn recv_one_meta<Cb>(
        &mut self,
        timeout: Duration,
        mut on_payload: Cb,
    ) -> Result<RecvStatus, RecvError<Self::Error>>
    where
        Cb: FnMut(RecvMeta<Self::ReplyTo>, &[u8]) -> Result<RecvControl, Self::Error>,
    {
        if self.peers.is_empty() {
            thread::sleep(timeout);
            return Ok(RecvStatus::TimedOut);
        }

        let mut fds: Vec<libc::pollfd> = self
            .peers
            .iter()
            .map(|peer| libc::pollfd {
                fd: peer.socket.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            })
            .collect();

        let ready = poll_fds(&mut fds, timeout)
            .map_err(Error::Io)
            .map_err(RecvError::Backend)?;
        if ready == 0 {
            return Ok(RecvStatus::TimedOut);
        }

        for (idx, fd) in fds.iter().enumerate() {
            if fd.revents & libc::POLLIN != 0 {
                let reply_to = self.peers[idx].addr;
                return self.peers[idx]
                    .socket
                    .recv_one_ready(|payload| on_payload(RecvMeta { reply_to }, payload));
            }
            if fd.revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0 {
                return Err(RecvError::Backend(Error::Io(io::Error::new(
                    io::ErrorKind::Other,
                    "poll error on iso-tp socket",
                ))));
            }
        }

        Ok(RecvStatus::TimedOut)
    }
}

fn poll_fd(fd: RawFd, events: i16, timeout: Duration) -> io::Result<bool> {
    let mut fds = libc::pollfd {
        fd,
        events,
        revents: 0,
    };
    let timeout_ms = duration_to_poll_timeout(timeout);
    loop {
        let res = unsafe { libc::poll(&mut fds, 1, timeout_ms) };
        if res >= 0 {
            return Ok(res > 0);
        }
        let err = io::Error::last_os_error();
        if err.kind() == io::ErrorKind::Interrupted {
            continue;
        }
        return Err(err);
    }
}

fn poll_fds(fds: &mut [libc::pollfd], timeout: Duration) -> io::Result<i32> {
    let timeout_ms = duration_to_poll_timeout(timeout);
    loop {
        let res = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as u64, timeout_ms) };
        if res >= 0 {
            return Ok(res);
        }
        let err = io::Error::last_os_error();
        if err.kind() == io::ErrorKind::Interrupted {
            continue;
        }
        return Err(err);
    }
}

fn duration_to_poll_timeout(timeout: Duration) -> i32 {
    let ms = timeout.as_millis();
    i32::try_from(ms).unwrap_or(i32::MAX)
}

fn duration_to_nanos_u32(d: Duration) -> u32 {
    d.as_nanos().min(u32::MAX as u128) as u32
}

fn isotp_stmin_to_duration(raw: u8) -> Option<Duration> {
    match raw {
        0x00..=0x7F => Some(Duration::from_millis(raw as u64)),
        0xF1..=0xF9 => Some(Duration::from_micros((raw as u64 - 0xF0) * 100)),
        _ => None,
    }
}

fn duration_to_isotp_stmin(duration: Duration) -> u8 {
    let micros = duration.as_micros();
    if micros == 0 {
        return 0;
    }
    if (100..=900).contains(&micros) && micros.is_multiple_of(100) {
        return 0xF0 + (micros / 100) as u8;
    }
    let millis = duration.as_millis();
    if millis <= 0x7F { millis as u8 } else { 0x7F }
}

fn uds_phys_id(target: u8, source: u8) -> Id {
    let raw = uds29::encode_phys_id_raw(target, source);
    let ext = ExtendedId::new(raw).expect("UDS 29-bit ID must fit in 29 bits");
    Id::Extended(ext)
}

fn apply_kernel_options(fd: RawFd, options: &IsoTpKernelOptions) -> Result<(), Error> {
    if let Some(txtime) = options.socket.frame_txtime {
        let nanos = duration_to_nanos_u32(txtime);
        let res = unsafe {
            libc::setsockopt(
                fd,
                libc::SOL_SOCKET,
                libc::SO_TXTIME,
                &nanos as *const u32 as *const libc::c_void,
                size_of::<u32>() as libc::socklen_t,
            )
        };
        if res < 0 {
            return Err(Error::Io(io::Error::last_os_error()));
        }
    }

    let base = build_can_isotp_options(&options.socket);
    let res = unsafe {
        libc::setsockopt(
            fd,
            SOL_CAN_ISOTP,
            CAN_ISOTP_OPTS,
            &base as *const can_isotp_options as *const libc::c_void,
            size_of::<can_isotp_options>() as libc::socklen_t,
        )
    };
    if res < 0 {
        return Err(Error::Io(io::Error::last_os_error()));
    }

    if let Some(fc) = options.flow_control {
        let c = can_isotp_fc_options {
            bs: fc.block_size,
            stmin: fc.st_min,
            wftmax: fc.wft_max,
        };
        let res = unsafe {
            libc::setsockopt(
                fd,
                SOL_CAN_ISOTP,
                CAN_ISOTP_RECV_FC,
                &c as *const can_isotp_fc_options as *const libc::c_void,
                size_of::<can_isotp_fc_options>() as libc::socklen_t,
            )
        };
        if res < 0 {
            return Err(Error::Io(io::Error::last_os_error()));
        }
    }

    if let Some(stmin) = options.force_tx_stmin {
        let nanos = duration_to_nanos_u32(stmin);
        let res = unsafe {
            libc::setsockopt(
                fd,
                SOL_CAN_ISOTP,
                CAN_ISOTP_TX_STMIN,
                &nanos as *const u32 as *const libc::c_void,
                size_of::<u32>() as libc::socklen_t,
            )
        };
        if res < 0 {
            return Err(Error::Io(io::Error::last_os_error()));
        }
    }

    if let Some(stmin) = options.force_rx_stmin {
        let nanos = duration_to_nanos_u32(stmin);
        let res = unsafe {
            libc::setsockopt(
                fd,
                SOL_CAN_ISOTP,
                CAN_ISOTP_RX_STMIN,
                &nanos as *const u32 as *const libc::c_void,
                size_of::<u32>() as libc::socklen_t,
            )
        };
        if res < 0 {
            return Err(Error::Io(io::Error::last_os_error()));
        }
    }

    if let Some(ll) = options.link_layer {
        let c = can_isotp_ll_options {
            mtu: ll.mtu,
            tx_dl: ll.tx_dl,
            tx_flags: ll.tx_flags,
        };
        let res = unsafe {
            libc::setsockopt(
                fd,
                SOL_CAN_ISOTP,
                CAN_ISOTP_LL_OPTS,
                &c as *const can_isotp_ll_options as *const libc::c_void,
                size_of::<can_isotp_ll_options>() as libc::socklen_t,
            )
        };
        if res < 0 {
            return Err(Error::Io(io::Error::last_os_error()));
        }
    }

    Ok(())
}

fn build_can_isotp_options(opts: &IsoTpSocketOptions) -> can_isotp_options {
    let mut flags = opts.flags;
    if opts.ext_address.is_some() {
        flags |= CAN_ISOTP_EXTEND_ADDR;
    }
    if opts.tx_padding.is_some() {
        flags |= CAN_ISOTP_TX_PADDING;
    }
    if opts.rx_padding.is_some() {
        flags |= CAN_ISOTP_RX_PADDING;
    }
    if opts.rx_ext_address.is_some() {
        flags |= CAN_ISOTP_RX_EXT_ADDR;
    }

    can_isotp_options {
        flags,
        frame_txtime: opts.frame_txtime.map(duration_to_nanos_u32).unwrap_or(0),
        ext_address: opts.ext_address.unwrap_or(0),
        txpad_content: opts.tx_padding.unwrap_or(0),
        rxpad_content: opts.rx_padding.unwrap_or(0),
        rx_ext_address: opts.rx_ext_address.unwrap_or(0),
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct can_isotp_options {
    flags: u32,
    frame_txtime: u32,
    ext_address: u8,
    txpad_content: u8,
    rxpad_content: u8,
    rx_ext_address: u8,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct can_isotp_fc_options {
    bs: u8,
    stmin: u8,
    wftmax: u8,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct can_isotp_ll_options {
    mtu: u8,
    tx_dl: u8,
    tx_flags: u8,
}

const SOL_CAN_ISOTP: i32 = libc::SOL_CAN_BASE + libc::CAN_ISOTP;
const CAN_ISOTP_OPTS: i32 = 1;
const CAN_ISOTP_RECV_FC: i32 = 2;
const CAN_ISOTP_TX_STMIN: i32 = 3;
const CAN_ISOTP_RX_STMIN: i32 = 4;
const CAN_ISOTP_LL_OPTS: i32 = 5;

/// Kernel ISO-TP flag constants.
pub mod flags {
    /// Listen-only mode (do not send FC frames).
    pub const CAN_ISOTP_LISTEN_MODE: u32 = 0x0001;
    /// Enable extended addressing.
    pub const CAN_ISOTP_EXTEND_ADDR: u32 = 0x0002;
    /// Enable CAN frame padding on the TX path.
    pub const CAN_ISOTP_TX_PADDING: u32 = 0x0004;
    /// Enable CAN frame padding on the RX path.
    pub const CAN_ISOTP_RX_PADDING: u32 = 0x0008;
    /// Check received CAN frame padding length.
    pub const CAN_ISOTP_CHK_PAD_LEN: u32 = 0x0010;
    /// Check received CAN frame padding content.
    pub const CAN_ISOTP_CHK_PAD_DATA: u32 = 0x0020;
    /// Half-duplex error state handling.
    pub const CAN_ISOTP_HALF_DUPLEX: u32 = 0x0040;
    /// Ignore STmin from received FC frames.
    pub const CAN_ISOTP_FORCE_TXSTMIN: u32 = 0x0080;
    /// Ignore CFs depending on RX STmin.
    pub const CAN_ISOTP_FORCE_RXSTMIN: u32 = 0x0100;
    /// Different RX extended addressing.
    pub const CAN_ISOTP_RX_EXT_ADDR: u32 = 0x0200;
    /// Wait for TX completion.
    pub const CAN_ISOTP_WAIT_TX_DONE: u32 = 0x0400;
    /// 1-to-N functional addressing (single frame).
    pub const CAN_ISOTP_SF_BROADCAST: u32 = 0x0800;
    /// 1-to-N transmission without FC.
    pub const CAN_ISOTP_CF_BROADCAST: u32 = 0x1000;
    /// Dynamic FC parameters.
    pub const CAN_ISOTP_DYN_FC_PARMS: u32 = 0x2000;
}

const CAN_ISOTP_EXTEND_ADDR: u32 = flags::CAN_ISOTP_EXTEND_ADDR;
const CAN_ISOTP_TX_PADDING: u32 = flags::CAN_ISOTP_TX_PADDING;
const CAN_ISOTP_RX_PADDING: u32 = flags::CAN_ISOTP_RX_PADDING;
const CAN_ISOTP_RX_EXT_ADDR: u32 = flags::CAN_ISOTP_RX_EXT_ADDR;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn can_isotp_options_layout() {
        assert_eq!(size_of::<can_isotp_options>(), 12);
        assert_eq!(size_of::<can_isotp_fc_options>(), 3);
        assert_eq!(size_of::<can_isotp_ll_options>(), 3);
    }

    #[test]
    fn duration_to_nanos_clamps() {
        let huge = Duration::from_secs(u32::MAX as u64 + 42);
        assert_eq!(super::duration_to_nanos_u32(huge), u32::MAX);
    }

    #[test]
    fn stmin_duration_round_trips_common_values() {
        assert_eq!(
            super::duration_to_isotp_stmin(Duration::from_millis(0)),
            0x00
        );
        assert_eq!(
            super::duration_to_isotp_stmin(Duration::from_millis(10)),
            0x0A
        );
        assert_eq!(
            super::duration_to_isotp_stmin(Duration::from_micros(100)),
            0xF1
        );
        assert_eq!(
            super::duration_to_isotp_stmin(Duration::from_micros(900)),
            0xF9
        );
        assert_eq!(
            super::isotp_stmin_to_duration(0xF4),
            Some(Duration::from_micros(400))
        );
    }

    #[test]
    fn options_flags_from_padding_and_ext_addr() {
        let opts = IsoTpSocketOptions {
            ext_address: Some(0x12),
            rx_ext_address: Some(0x34),
            tx_padding: Some(0xAA),
            rx_padding: Some(0xBB),
            ..IsoTpSocketOptions::default()
        };
        let c = build_can_isotp_options(&opts);
        assert_eq!(c.ext_address, 0x12);
        assert_eq!(c.rx_ext_address, 0x34);
        assert_eq!(c.txpad_content, 0xAA);
        assert_eq!(c.rxpad_content, 0xBB);
        assert!(c.flags & CAN_ISOTP_EXTEND_ADDR != 0);
        assert!(c.flags & CAN_ISOTP_RX_EXT_ADDR != 0);
        assert!(c.flags & CAN_ISOTP_TX_PADDING != 0);
        assert!(c.flags & CAN_ISOTP_RX_PADDING != 0);
    }
}
