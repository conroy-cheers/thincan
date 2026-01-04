use embedded_can::Id;
use std::{error::Error as StdError, fmt};

/// A pair of CAN identifiers describing the receive/transmit direction for an ISO-TP link.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IsoTpAddress {
    pub rx_id: Id,
    pub tx_id: Id,
}

impl IsoTpAddress {
    pub fn new(rx_id: impl Into<Id>, tx_id: impl Into<Id>) -> Self {
        Self {
            rx_id: rx_id.into(),
            tx_id: tx_id.into(),
        }
    }
}

/// Generic ISO-TP error type that wraps underlying transport errors and protocol issues.
#[derive(Debug)]
pub enum IsoTpError<E> {
    Transport(E),
    PayloadTooLarge,
    InvalidFrame,
    UnsupportedFrame,
}

impl<E: fmt::Display> fmt::Display for IsoTpError<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            IsoTpError::Transport(err) => write!(f, "transport error: {err}"),
            IsoTpError::PayloadTooLarge => write!(f, "payload does not fit into a single ISO-TP frame"),
            IsoTpError::InvalidFrame => write!(f, "received malformed ISO-TP frame"),
            IsoTpError::UnsupportedFrame => write!(f, "received unsupported ISO-TP frame type"),
        }
    }
}

impl<E: StdError + 'static> StdError for IsoTpError<E> {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        match self {
            IsoTpError::Transport(err) => Some(err),
            _ => None,
        }
    }
}

/// Common ISO-TP interface that can be backed by any CAN stack.
pub trait IsoTpChannel {
    type Error;

    fn address(&self) -> IsoTpAddress;

    fn send(&mut self, payload: &[u8]) -> Result<(), IsoTpError<Self::Error>>;

    fn recv(&mut self) -> Result<Vec<u8>, IsoTpError<Self::Error>>;
}

pub mod embedded {
    use super::{IsoTpAddress, IsoTpChannel, IsoTpError};
    use embedded_can::{blocking::Can, Frame, Id};

    /// Minimal single-frame ISO-TP implementation using the blocking embedded-can traits.
    ///
    /// This currently supports payloads up to 7 bytes (single-frame PCI).
    pub struct EmbeddedIsoTp<C>
    where
        C: Can,
    {
        can: C,
        address: IsoTpAddress,
    }

    impl<C> EmbeddedIsoTp<C>
    where
        C: Can,
    {
        pub fn new(can: C, rx_id: impl Into<Id>, tx_id: impl Into<Id>) -> Self {
            Self {
                can,
                address: IsoTpAddress::new(rx_id, tx_id),
            }
        }

        pub fn into_inner(self) -> C {
            self.can
        }
    }

    impl<C> IsoTpChannel for EmbeddedIsoTp<C>
    where
        C: Can,
    {
        type Error = C::Error;

        fn address(&self) -> IsoTpAddress {
            self.address
        }

        fn send(&mut self, payload: &[u8]) -> Result<(), IsoTpError<Self::Error>> {
            const MAX_SINGLE_FRAME_LEN: usize = 7;
            if payload.len() > MAX_SINGLE_FRAME_LEN {
                return Err(IsoTpError::PayloadTooLarge);
            }

            let mut data = [0u8; 8];
            data[0] = (payload.len() as u8) & 0x0F; // Single-frame PCI
            data[1..1 + payload.len()].copy_from_slice(payload);

            let frame_len = payload.len() + 1;
            let frame = C::Frame::new(self.address.tx_id, &data[..frame_len])
                .ok_or(IsoTpError::InvalidFrame)?;

            self.can
                .transmit(&frame)
                .map_err(IsoTpError::Transport)?;

            Ok(())
        }

        fn recv(&mut self) -> Result<Vec<u8>, IsoTpError<Self::Error>> {
            loop {
                let frame = self
                    .can
                    .receive()
                    .map_err(IsoTpError::Transport)?;

                if frame.id() != self.address.rx_id {
                    continue;
                }

                let data = frame.data();
                if data.is_empty() {
                    return Err(IsoTpError::InvalidFrame);
                }

                let pci = data[0];
                let frame_type = pci >> 4;
                if frame_type != 0 {
                    return Err(IsoTpError::UnsupportedFrame);
                }

                let payload_len = (pci & 0x0F) as usize;
                if payload_len > data.len().saturating_sub(1) {
                    return Err(IsoTpError::InvalidFrame);
                }

                let mut out = vec![0u8; payload_len];
                out.copy_from_slice(&data[1..1 + payload_len]);
                return Ok(out);
            }
        }
    }
}

#[cfg(feature = "socketcan")]
pub mod socketcan {
    use super::{IsoTpAddress, IsoTpChannel, IsoTpError};
    use bitflags::bitflags;
    pub use embedded_can::{ExtendedId, Id, StandardId};
    use libc::{
        bind, c_int, c_short, c_void, close, fcntl, read, setsockopt, sockaddr, socket, write,
        F_GETFL, F_SETFL, O_NONBLOCK, SOCK_DGRAM,
    };
    use nix::net::if_::if_nametoindex;
    use std::convert::TryFrom;
    use std::convert::TryInto;
    use std::io;
    use std::mem::size_of;
    use std::num::TryFromIntError;
    use std::os::unix::io::{AsRawFd, FromRawFd, IntoRawFd, RawFd};
    use std::time::Duration;
    use thiserror::Error;

    pub const AF_CAN: c_short = 29;
    pub const PF_CAN: c_int = 29;
    pub const CAN_ISOTP: c_int = 6;
    pub const SOL_CAN_BASE: c_int = 100;
    pub const SOL_CAN_ISOTP: c_int = SOL_CAN_BASE + CAN_ISOTP;
    pub const CAN_ISOTP_OPTS: c_int = 1;
    pub const CAN_ISOTP_RECV_FC: c_int = 2;
    pub const CAN_ISOTP_TX_STMIN: c_int = 3;
    pub const CAN_ISOTP_RX_STMIN: c_int = 4;
    pub const CAN_ISOTP_LL_OPTS: c_int = 5;
    pub const CAN_MAX_DLEN: u8 = 8;
    const RECV_BUFFER_SIZE: usize = 4096;
    const SIZE_OF_CAN_FRAME: u8 = 16;
    const FLOW_CONTROL_OPTIONS_SIZE: usize = size_of::<FlowControlOptions>();
    const ISOTP_OPTIONS_SIZE: usize = size_of::<IsoTpOptions>();
    const LINK_LAYER_OPTIONS_SIZE: usize = size_of::<LinkLayerOptions>();
    const CAN_ISOTP_DEFAULT_RECV_BS: u8 = 0;
    const CAN_ISOTP_DEFAULT_RECV_STMIN: u8 = 0x00;
    const CAN_ISOTP_DEFAULT_RECV_WFTMAX: u8 = 0;

    bitflags! {
        pub struct IsoTpBehaviour: u32 {
            const CAN_ISOTP_LISTEN_MODE = 0x001;
            const CAN_ISOTP_EXTEND_ADDR	= 0x002;
            const CAN_ISOTP_TX_PADDING	= 0x004;
            const CAN_ISOTP_RX_PADDING	= 0x008;
            const CAN_ISOTP_CHK_PAD_LEN	= 0x010;
            const CAN_ISOTP_CHK_PAD_DATA = 0x020;
            const CAN_ISOTP_HALF_DUPLEX = 0x040;
            const CAN_ISOTP_FORCE_TXSTMIN = 0x080;
            const CAN_ISOTP_FORCE_RXSTMIN = 0x100;
            const CAN_ISOTP_RX_EXT_ADDR = 0x200;
        }
    }

    pub const EFF_FLAG: u32 = 0x8000_0000;
    pub const RTR_FLAG: u32 = 0x4000_0000;
    pub const ERR_FLAG: u32 = 0x2000_0000;
    pub const SFF_MASK: u32 = 0x0000_07ff;
    pub const EFF_MASK: u32 = 0x1fff_ffff;
    pub const ERR_MASK: u32 = 0x1fff_ffff;
    pub const ERR_MASK_ALL: u32 = ERR_MASK;
    pub const ERR_MASK_NONE: u32 = 0;

    #[derive(Debug)]
    #[repr(C)]
    struct CanAddr {
        _af_can: c_short,
        if_index: c_int,
        rx_id: u32,
        tx_id: u32,
        _pgn: u32,
        _addr: u8,
    }

    #[repr(C)]
    pub struct IsoTpOptions {
        flags: u32,
        frame_txtime: u32,
        ext_address: u8,
        txpad_content: u8,
        rxpad_content: u8,
        rx_ext_address: u8,
    }

    impl IsoTpOptions {
        pub fn new(
            flags: IsoTpBehaviour,
            frame_txtime: Duration,
            ext_address: u8,
            txpad_content: u8,
            rxpad_content: u8,
            rx_ext_address: u8,
        ) -> Result<Self, TryFromIntError> {
            let flags = flags.bits();
            let frame_txtime = u32::try_from(frame_txtime.as_nanos())?;

            Ok(Self {
                flags,
                frame_txtime,
                ext_address,
                txpad_content,
                rxpad_content,
                rx_ext_address,
            })
        }

        pub fn get_flags(&self) -> Option<IsoTpBehaviour> {
            IsoTpBehaviour::from_bits(self.flags)
        }

        pub fn set_flags(&mut self, flags: IsoTpBehaviour) {
            self.flags = flags.bits();
        }

        pub fn get_frame_txtime(&self) -> Duration {
            Duration::from_nanos(self.frame_txtime.into())
        }

        pub fn set_frame_txtime(&mut self, frame_txtime: Duration) -> Result<(), TryFromIntError> {
            self.frame_txtime = u32::try_from(frame_txtime.as_nanos())?;
            Ok(())
        }

        pub fn get_ext_address(&self) -> u8 {
            self.ext_address
        }

        pub fn set_ext_address(&mut self, ext_address: u8) {
            self.ext_address = ext_address;
        }

        pub fn get_txpad_content(&self) -> u8 {
            self.txpad_content
        }

        pub fn set_txpad_content(&mut self, txpad_content: u8) {
            self.txpad_content = txpad_content;
        }

        pub fn get_rxpad_content(&self) -> u8 {
            self.rxpad_content
        }

        pub fn set_rxpad_content(&mut self, rxpad_content: u8) {
            self.rxpad_content = rxpad_content;
        }

        pub fn get_rx_ext_address(&self) -> u8 {
            self.rx_ext_address
        }

        pub fn set_rx_ext_address(&mut self, rx_ext_address: u8) {
            self.rx_ext_address = rx_ext_address;
        }
    }

    impl Default for IsoTpOptions {
        fn default() -> Self {
            Self {
                flags: 0x00,
                frame_txtime: 0x00,
                ext_address: 0x00,
                txpad_content: 0xCC,
                rxpad_content: 0xCC,
                rx_ext_address: 0x00,
            }
        }
    }

    #[repr(C)]
    pub struct FlowControlOptions {
        bs: u8,
        stmin: u8,
        wftmax: u8,
    }

    impl Default for FlowControlOptions {
        fn default() -> Self {
            Self {
                bs: CAN_ISOTP_DEFAULT_RECV_BS,
                stmin: CAN_ISOTP_DEFAULT_RECV_STMIN,
                wftmax: CAN_ISOTP_DEFAULT_RECV_WFTMAX,
            }
        }
    }

    impl FlowControlOptions {
        pub fn new(bs: u8, stmin: u8, wftmax: u8) -> Self {
            Self { bs, stmin, wftmax }
        }
    }

    bitflags! {
        pub struct TxFlags: u8 {
            const CANFD_BRS = 0x01;
            const CANFD_ESI	= 0x02;
        }
    }

    #[repr(C)]
    pub struct LinkLayerOptions {
        mtu: u8,
        tx_dl: u8,
        tx_flags: u8,
    }

    impl LinkLayerOptions {
        pub fn new(mtu: u8, tx_dl: u8, tx_flags: TxFlags) -> Self {
            let tx_flags = tx_flags.bits();
            Self {
                mtu,
                tx_dl,
                tx_flags,
            }
        }
    }

    impl Default for LinkLayerOptions {
        fn default() -> Self {
            Self {
                mtu: SIZE_OF_CAN_FRAME,
                tx_dl: CAN_MAX_DLEN,
                tx_flags: 0x00,
            }
        }
    }

    #[derive(Error, Debug)]
    pub enum Error {
        #[error("Failed to find can device: {source:?}")]
        Lookup {
            #[from]
            source: nix::Error,
        },
        #[error("IO error: {source:?}")]
        Io {
            #[from]
            source: io::Error,
        },
    }

    pub struct IsoTpSocket {
        fd: c_int,
        recv_buffer: [u8; RECV_BUFFER_SIZE],
    }

    impl IsoTpSocket {
        pub fn open(ifname: &str, rx_id: impl Into<Id>, tx_id: impl Into<Id>) -> Result<Self, Error> {
            Self::open_with_opts(
                ifname,
                rx_id,
                tx_id,
                Some(IsoTpOptions::default()),
                Some(FlowControlOptions::default()),
                Some(LinkLayerOptions::default()),
            )
        }

        pub fn open_with_opts(
            ifname: &str,
            rx_id: impl Into<Id>,
            tx_id: impl Into<Id>,
            isotp_options: Option<IsoTpOptions>,
            rx_flow_control_options: Option<FlowControlOptions>,
            link_layer_options: Option<LinkLayerOptions>,
        ) -> Result<Self, Error> {
            let if_index = if_nametoindex(ifname)?;
            Self::open_if_with_opts(
                if_index.try_into().unwrap(),
                rx_id,
                tx_id,
                isotp_options,
                rx_flow_control_options,
                link_layer_options,
            )
        }

        pub fn open_if(
            if_index: c_int,
            rx_id: impl Into<Id>,
            tx_id: impl Into<Id>,
        ) -> Result<Self, Error> {
            Self::open_if_with_opts(
                if_index,
                rx_id,
                tx_id,
                Some(IsoTpOptions::default()),
                Some(FlowControlOptions::default()),
                Some(LinkLayerOptions::default()),
            )
        }

        pub fn open_if_with_opts(
            if_index: c_int,
            rx_id: impl Into<Id>,
            tx_id: impl Into<Id>,
            isotp_options: Option<IsoTpOptions>,
            rx_flow_control_options: Option<FlowControlOptions>,
            link_layer_options: Option<LinkLayerOptions>,
        ) -> Result<Self, Error> {
            let rx_id = match rx_id.into() {
                Id::Standard(standard_id) => standard_id.as_raw() as u32,
                Id::Extended(extended_id) => extended_id.as_raw() | EFF_FLAG,
            };
            let tx_id = match tx_id.into() {
                Id::Standard(standard_id) => standard_id.as_raw() as u32,
                Id::Extended(extended_id) => extended_id.as_raw() | EFF_FLAG,
            };
            let addr = CanAddr {
                _af_can: AF_CAN,
                if_index,
                rx_id,
                tx_id,
                _pgn: 0,
                _addr: 0,
            };

            let sock_fd;
            unsafe {
                sock_fd = socket(PF_CAN, SOCK_DGRAM, CAN_ISOTP);
            }

            if sock_fd == -1 {
                return Err(Error::from(io::Error::last_os_error()));
            }

            if let Some(isotp_options) = isotp_options {
                let isotp_options_ptr: *const c_void = &isotp_options as *const _ as *const c_void;
                let err = unsafe {
                    setsockopt(
                        sock_fd,
                        SOL_CAN_ISOTP,
                        CAN_ISOTP_OPTS,
                        isotp_options_ptr,
                        ISOTP_OPTIONS_SIZE.try_into().unwrap(),
                    )
                };
                if err == -1 {
                    return Err(Error::from(io::Error::last_os_error()));
                }
            }

            if let Some(rx_flow_control_options) = rx_flow_control_options {
                let rx_flow_control_options_ptr: *const c_void =
                    &rx_flow_control_options as *const _ as *const c_void;
                let err = unsafe {
                    setsockopt(
                        sock_fd,
                        SOL_CAN_ISOTP,
                        CAN_ISOTP_RECV_FC,
                        rx_flow_control_options_ptr,
                        FLOW_CONTROL_OPTIONS_SIZE.try_into().unwrap(),
                    )
                };
                if err == -1 {
                    return Err(Error::from(io::Error::last_os_error()));
                }
            }

            if let Some(link_layer_options) = link_layer_options {
                let link_layer_options_ptr: *const c_void =
                    &link_layer_options as *const _ as *const c_void;
                let err = unsafe {
                    setsockopt(
                        sock_fd,
                        SOL_CAN_ISOTP,
                        CAN_ISOTP_LL_OPTS,
                        link_layer_options_ptr,
                        LINK_LAYER_OPTIONS_SIZE.try_into().unwrap(),
                    )
                };
                if err == -1 {
                    return Err(Error::from(io::Error::last_os_error()));
                }
            }

            let bind_rv;
            unsafe {
                let sockaddr_ptr = &addr as *const CanAddr;
                bind_rv = bind(
                    sock_fd,
                    sockaddr_ptr as *const sockaddr,
                    size_of::<CanAddr>().try_into().unwrap(),
                );
            }

            if bind_rv == -1 {
                let e = io::Error::last_os_error();
                unsafe {
                    close(sock_fd);
                }
                return Err(Error::from(e));
            }

            Ok(Self {
                fd: sock_fd,
                recv_buffer: [0x00; RECV_BUFFER_SIZE],
            })
        }

        fn close(&mut self) -> io::Result<()> {
            unsafe {
                let rv = close(self.fd);
                if rv == -1 {
                    return Err(io::Error::last_os_error());
                }
            }
            Ok(())
        }

        pub fn set_nonblocking(&self, nonblocking: bool) -> io::Result<()> {
            let oldfl = unsafe { fcntl(self.fd, F_GETFL) };

            if oldfl == -1 {
                return Err(io::Error::last_os_error());
            }

            let newfl = if nonblocking {
                oldfl | O_NONBLOCK
            } else {
                oldfl & !O_NONBLOCK
            };

            let rv = unsafe { fcntl(self.fd, F_SETFL, newfl) };

            if rv != 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        }

        pub fn read(&mut self) -> io::Result<&[u8]> {
            let buffer_ptr = &mut self.recv_buffer as *mut _ as *mut c_void;

            let read_rv = unsafe { read(self.fd, buffer_ptr, RECV_BUFFER_SIZE) };

            if read_rv < 0 {
                return Err(io::Error::last_os_error());
            }

            Ok(&self.recv_buffer[0..read_rv.try_into().unwrap()])
        }

        pub fn write(&self, buffer: &[u8]) -> io::Result<()> {
            let write_rv = unsafe {
                let buffer_ptr = buffer as *const _ as *const c_void;
                write(self.fd, buffer_ptr, buffer.len())
            };

            if write_rv != buffer.len().try_into().unwrap() {
                return Err(io::Error::last_os_error());
            }

            Ok(())
        }
    }

    impl AsRawFd for IsoTpSocket {
        fn as_raw_fd(&self) -> RawFd {
            self.fd
        }
    }

    impl FromRawFd for IsoTpSocket {
        unsafe fn from_raw_fd(fd: RawFd) -> Self {
            Self {
                fd,
                recv_buffer: [0x00; RECV_BUFFER_SIZE],
            }
        }
    }

    impl IntoRawFd for IsoTpSocket {
        fn into_raw_fd(self) -> RawFd {
            self.fd
        }
    }

    impl Drop for IsoTpSocket {
        fn drop(&mut self) {
            self.close().ok();
        }
    }

    /// Simple adapter that implements the generic ISO-TP trait using the Linux socketcan backend.
    pub struct SocketCanIsoTp {
        socket: IsoTpSocket,
        address: IsoTpAddress,
    }

    impl SocketCanIsoTp {
        pub fn open(ifname: &str, rx_id: impl Into<Id>, tx_id: impl Into<Id>) -> Result<Self, Error> {
            let address = IsoTpAddress::new(rx_id.into(), tx_id.into());
            let socket = IsoTpSocket::open(ifname, address.rx_id, address.tx_id)?;
            Ok(Self { socket, address })
        }

        pub fn open_with_opts(
            ifname: &str,
            rx_id: impl Into<Id>,
            tx_id: impl Into<Id>,
            isotp_options: Option<IsoTpOptions>,
            rx_flow_control_options: Option<FlowControlOptions>,
            link_layer_options: Option<LinkLayerOptions>,
        ) -> Result<Self, Error> {
            let address = IsoTpAddress::new(rx_id.into(), tx_id.into());
            let socket = IsoTpSocket::open_with_opts(
                ifname,
                address.rx_id,
                address.tx_id,
                isotp_options,
                rx_flow_control_options,
                link_layer_options,
            )?;
            Ok(Self { socket, address })
        }
    }

    impl IsoTpChannel for SocketCanIsoTp {
        type Error = Error;

        fn address(&self) -> IsoTpAddress {
            self.address
        }

        fn send(&mut self, payload: &[u8]) -> Result<(), IsoTpError<Self::Error>> {
            self.socket.write(payload).map_err(IsoTpError::Transport)
        }

        fn recv(&mut self) -> Result<Vec<u8>, IsoTpError<Self::Error>> {
            let data = self.socket.read().map_err(IsoTpError::Transport)?;
            Ok(data.to_vec())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{embedded::EmbeddedIsoTp, IsoTpAddress, IsoTpChannel, IsoTpError};
    use embedded_can::{blocking::Can, Frame, Id, StandardId};
    use embedded_can_mock::{CanFrame, MockBus, MockCanInterface};

    #[test]
    fn sends_and_receives_single_frame() {
        let bus = MockBus::<CanFrame>::new();
        let mut node_a = EmbeddedIsoTp::new(
            MockCanInterface::new(bus.clone()),
            Id::Standard(StandardId::new(0x123).unwrap()),
            Id::Standard(StandardId::new(0x321).unwrap()),
        );
        let mut node_b = EmbeddedIsoTp::new(
            MockCanInterface::new(bus),
            Id::Standard(StandardId::new(0x321).unwrap()),
            Id::Standard(StandardId::new(0x123).unwrap()),
        );

        let message = [0x11, 0x22, 0x33];
        node_a.send(&message).unwrap();

        let received = node_b.recv().unwrap();
        assert_eq!(received, &message);
    }

    #[test]
    fn rejects_payloads_too_large() {
        let bus = MockBus::<CanFrame>::new();
        let mut node = EmbeddedIsoTp::new(
            MockCanInterface::new(bus),
            Id::Standard(StandardId::new(0x100).unwrap()),
            Id::Standard(StandardId::new(0x200).unwrap()),
        );

        let too_big = [0xAA; 9];
        assert!(matches!(
            node.send(&too_big),
            Err(IsoTpError::PayloadTooLarge)
        ));
    }

    #[ignore = "multi-frame support not yet implemented"]
    #[test]
    fn multi_frame_send_segments_payload_into_iso_tp_frames() {
        let bus = MockBus::<CanFrame>::new();
        let mut tx_node = EmbeddedIsoTp::new(
            MockCanInterface::new(bus.clone()),
            Id::Standard(StandardId::new(0x555).unwrap()),
            Id::Standard(StandardId::new(0x556).unwrap()),
        );
        let mut sniffer = MockCanInterface::new(bus);

        let payload: Vec<u8> = (0u8..20).collect();
        tx_node.send(&payload).expect("multi-frame send should succeed");

        let first = sniffer.receive().expect("first frame missing");
        let cf1 = sniffer.receive().expect("consecutive frame #1 missing");
        let cf2 = sniffer.receive().expect("consecutive frame #2 missing");

        assert_eq!(first.id(), Id::Standard(StandardId::new(0x556).unwrap()));
        assert_eq!(cf1.id(), first.id());
        assert_eq!(cf2.id(), first.id());

        let first_data = first.data();
        assert_eq!(first_data.len(), 8);
        assert_eq!(first_data[0], 0x10 | ((payload.len() >> 8) as u8 & 0x0F));
        assert_eq!(first_data[1], payload.len() as u8);
        assert_eq!(&first_data[2..], &payload[..6]);

        let cf1_data = cf1.data();
        assert_eq!(cf1_data[0], 0x21);
        assert_eq!(&cf1_data[1..], &payload[6..13]);

        let cf2_data = cf2.data();
        assert_eq!(cf2_data[0], 0x22);
        assert_eq!(&cf2_data[1..], &payload[13..]);
    }

    #[ignore = "multi-frame support not yet implemented"]
    #[test]
    fn multi_frame_send_handles_short_final_frame() {
        let bus = MockBus::<CanFrame>::new();
        let mut tx_node = EmbeddedIsoTp::new(
            MockCanInterface::new(bus.clone()),
            Id::Standard(StandardId::new(0x600).unwrap()),
            Id::Standard(StandardId::new(0x601).unwrap()),
        );
        let mut sniffer = MockCanInterface::new(bus);

        let payload: Vec<u8> = (0u8..9).collect();
        tx_node.send(&payload).expect("multi-frame send should succeed");

        let first = sniffer.receive().unwrap();
        let cf1 = sniffer.receive().unwrap();

        let ff_data = first.data();
        assert_eq!(ff_data[0], 0x10);
        assert_eq!(ff_data[1], payload.len() as u8);
        assert_eq!(&ff_data[2..], &payload[..6]);

        let cf_data = cf1.data();
        assert_eq!(cf_data[0], 0x21);
        assert_eq!(&cf_data[1..4], &payload[6..]);
    }

    #[ignore = "multi-frame support not yet implemented"]
    #[test]
    fn multi_frame_receive_reassembles_payload() {
        let bus = MockBus::<CanFrame>::new();
        let mut rx_node = EmbeddedIsoTp::new(
            MockCanInterface::new(bus.clone()),
            Id::Standard(StandardId::new(0x700).unwrap()),
            Id::Standard(StandardId::new(0x701).unwrap()),
        );
        let mut remote = MockCanInterface::new(bus);

        let payload: Vec<u8> = (0u8..15).collect();
        let ff = first_frame(Id::Standard(StandardId::new(0x700).unwrap()), payload.len(), &payload[..6]);
        let cf1 = consecutive_frame(Id::Standard(StandardId::new(0x700).unwrap()), 1, &payload[6..13]);
        let cf2 = consecutive_frame(Id::Standard(StandardId::new(0x700).unwrap()), 2, &payload[13..]);

        remote.transmit(&ff).unwrap();
        remote.transmit(&cf1).unwrap();
        remote.transmit(&cf2).unwrap();

        let out = rx_node.recv().expect("should reassemble multi-frame payload");
        assert_eq!(out, payload);
    }

    #[test]
    fn exposes_address() {
        let addr = IsoTpAddress::new(
            Id::Standard(StandardId::new(0x10).unwrap()),
            Id::Standard(StandardId::new(0x20).unwrap()),
        );
        assert_eq!(addr.rx_id, Id::Standard(StandardId::new(0x10).unwrap()));
        assert_eq!(addr.tx_id, Id::Standard(StandardId::new(0x20).unwrap()));
    }

    fn first_frame(id: Id, payload_len: usize, first_chunk: &[u8]) -> CanFrame {
        let mut data = [0u8; 8];
        data[0] = 0x10 | ((payload_len >> 8) as u8 & 0x0F);
        data[1] = (payload_len & 0xFF) as u8;
        let copy_len = first_chunk.len().min(6);
        data[2..2 + copy_len].copy_from_slice(&first_chunk[..copy_len]);
        CanFrame::new(id, &data).expect("valid first frame")
    }

    fn consecutive_frame(id: Id, seq: u8, chunk: &[u8]) -> CanFrame {
        let mut data = [0u8; 8];
        data[0] = 0x20 | (seq & 0x0F);
        let copy_len = chunk.len().min(7);
        data[1..1 + copy_len].copy_from_slice(&chunk[..copy_len]);
        CanFrame::new(id, &data[..1 + copy_len]).expect("valid consecutive frame")
    }
}
