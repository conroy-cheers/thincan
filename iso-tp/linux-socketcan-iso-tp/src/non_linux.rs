//! Non-Linux stubs.
//!
//! This crate targets Linux kernel `CAN_ISOTP` sockets. On non-Linux targets we provide
//! compile-time stubs so downstream crates can build with `cfg` gating.

use can_isotp_interface::{
    IsoTpEndpoint, IsoTpRxFlowControlConfig, RecvControl, RecvError, RecvMeta, RecvStatus,
    RxFlowControl, SendError,
};
use core::time::Duration;
use embedded_can::Id;

/// ISO-TP socket error type (non-Linux stub).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Error;

impl core::fmt::Display for Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(
            f,
            "linux-socketcan-iso-tp is only supported on Linux targets"
        )
    }
}

impl std::error::Error for Error {}

/// Socket-level ISO-TP options.
#[derive(Debug, Clone, Default)]
pub struct IsoTpSocketOptions {
    pub flags: u32,
    pub frame_txtime: Option<Duration>,
    pub ext_address: Option<u8>,
    pub tx_padding: Option<u8>,
    pub rx_padding: Option<u8>,
    pub rx_ext_address: Option<u8>,
}

/// Flow control options advertised by the kernel.
#[derive(Debug, Clone, Copy)]
pub struct IsoTpFlowControlOptions {
    pub block_size: u8,
    pub st_min: u8,
    pub wft_max: u8,
}

impl IsoTpFlowControlOptions {
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
    pub mtu: u8,
    pub tx_dl: u8,
    pub tx_flags: u8,
}

impl IsoTpLinkLayerOptions {
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
    pub max_rx_payload: usize,
    pub socket: IsoTpSocketOptions,
    pub flow_control: Option<IsoTpFlowControlOptions>,
    pub link_layer: Option<IsoTpLinkLayerOptions>,
    pub force_tx_stmin: Option<Duration>,
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

/// Kernel ISO-TP endpoint using a single socket (non-Linux stub).
#[derive(Debug)]
pub struct SocketCanIsoTp;

impl SocketCanIsoTp {
    pub fn open(
        _iface: &str,
        _rx_id: Id,
        _tx_id: Id,
        _options: &IsoTpKernelOptions,
    ) -> Result<Self, Error> {
        Err(Error)
    }
}

impl IsoTpEndpoint for SocketCanIsoTp {
    type Error = Error;

    fn send_to(
        &mut self,
        _to: u8,
        _payload: &[u8],
        _timeout: Duration,
    ) -> Result<(), SendError<Self::Error>> {
        Err(SendError::Backend(Error))
    }

    fn send_functional_to(
        &mut self,
        _functional_to: u8,
        _payload: &[u8],
        _timeout: Duration,
    ) -> Result<(), SendError<Self::Error>> {
        Err(SendError::Backend(Error))
    }

    fn recv_one<Cb>(
        &mut self,
        _timeout: Duration,
        _on_payload: Cb,
    ) -> Result<RecvStatus, RecvError<Self::Error>>
    where
        Cb: FnMut(RecvMeta, &[u8]) -> Result<RecvControl, Self::Error>,
    {
        Err(RecvError::Backend(Error))
    }
}

#[cfg(feature = "tokio")]
mod asynchronous {
    use super::*;
    use can_isotp_interface::{
        Destination, IsoTpAsyncReadSend, IsoTpAsyncWriteSend, ReceiveError, ReceivedPayload,
        TransmitError,
    };
    use std::sync::Arc;
    #[derive(Clone)]
    pub struct SocketCloser;
    impl SocketCloser {
        pub fn new(_: usize, _: usize) -> Result<Self, Error> {
            Err(Error)
        }
        pub fn occupied(&self) -> usize {
            0
        }
    }
    #[derive(Debug, Clone, Copy)]
    pub struct SocketRoute {
        pub receive_from: u8,
        pub transmit_to: Destination,
    }
    pub struct TokioSocketCanIsoTp;
    pub struct TokioIsoTpReadHalf;
    pub struct TokioIsoTpWriteHalf;
    #[derive(Clone)]
    pub struct SocketControl;
    pub struct SocketParts {
        pub reader: TokioIsoTpReadHalf,
        pub writer: TokioIsoTpWriteHalf,
        pub control: SocketControl,
    }
    #[non_exhaustive]
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum Operation {
        Receive,
        Send,
    }
    #[derive(Debug)]
    pub struct TerminalFailure {
        pub operation: Operation,
        pub error: Arc<Error>,
    }
    impl SocketControl {
        pub async fn failed(&self) -> Option<Arc<TerminalFailure>> {
            Some(Arc::new(TerminalFailure {
                operation: Operation::Receive,
                error: Arc::new(Error),
            }))
        }
        pub async fn closed(&self) {}
        pub fn is_closed(&self) -> bool {
            true
        }
    }
    impl TokioSocketCanIsoTp {
        pub fn open_peer(
            _: &SocketCloser,
            _: &str,
            _: u8,
            _: u8,
            _: &IsoTpKernelOptions,
        ) -> Result<Self, Error> {
            Err(Error)
        }
        pub fn open_with_ids(
            _: &SocketCloser,
            _: &str,
            _: Id,
            _: Id,
            _: SocketRoute,
            _: &IsoTpKernelOptions,
        ) -> Result<Self, Error> {
            Err(Error)
        }
        pub fn into_split(self) -> SocketParts {
            SocketParts {
                reader: TokioIsoTpReadHalf,
                writer: TokioIsoTpWriteHalf,
                control: SocketControl,
            }
        }
    }
    impl IsoTpAsyncReadSend for TokioIsoTpReadHalf {
        type Error = Arc<Error>;
        async fn recv_into(
            &mut self,
            _: &mut [u8],
        ) -> Result<ReceivedPayload, ReceiveError<Self::Error>> {
            Err(ReceiveError::Fatal(Arc::new(Error)))
        }
    }
    impl IsoTpAsyncWriteSend for TokioIsoTpWriteHalf {
        type Error = Arc<Error>;
        type Deadline = tokio::time::Instant;
        async fn send(
            &mut self,
            _: Destination,
            _: &[u8],
            _: Self::Deadline,
        ) -> Result<(), TransmitError<Self::Error>> {
            Err(TransmitError::Fatal(Arc::new(Error)))
        }
    }
}
#[cfg(feature = "tokio")]
pub use asynchronous::*;

/// Kernel-backed ISO-TP demux for UDS normal-fixed addressing (`0x18DA_TA_SA`) (non-Linux stub).
#[derive(Debug)]
pub struct KernelUdsDemux {
    local_addr: u8,
    _options: IsoTpKernelOptions,
}

impl KernelUdsDemux {
    pub fn new(_iface: impl Into<String>, local_addr: u8, options: IsoTpKernelOptions) -> Self {
        Self {
            local_addr,
            _options: options,
        }
    }

    pub fn local_addr(&self) -> u8 {
        self.local_addr
    }

    pub fn register_peer(&mut self, _peer: u8) -> Result<(), Error> {
        Err(Error)
    }
}

impl IsoTpEndpoint for KernelUdsDemux {
    type Error = Error;

    fn send_to(
        &mut self,
        _to: u8,
        _payload: &[u8],
        _timeout: Duration,
    ) -> Result<(), SendError<Self::Error>> {
        Err(SendError::Backend(Error))
    }

    fn send_functional_to(
        &mut self,
        _functional_to: u8,
        _payload: &[u8],
        _timeout: Duration,
    ) -> Result<(), SendError<Self::Error>> {
        Err(SendError::Backend(Error))
    }

    fn recv_one<Cb>(
        &mut self,
        _timeout: Duration,
        _on_payload: Cb,
    ) -> Result<RecvStatus, RecvError<Self::Error>>
    where
        Cb: FnMut(RecvMeta, &[u8]) -> Result<RecvControl, Self::Error>,
    {
        Err(RecvError::Backend(Error))
    }
}

impl IsoTpRxFlowControlConfig for KernelUdsDemux {
    type Error = Error;

    fn set_rx_flow_control(&mut self, _fc: RxFlowControl) -> Result<(), Self::Error> {
        Err(Error)
    }
}

/// Kernel ISO-TP flag constants.
pub mod flags {
    pub const CAN_ISOTP_LISTEN_MODE: u32 = 0x0001;
    pub const CAN_ISOTP_EXTEND_ADDR: u32 = 0x0002;
    pub const CAN_ISOTP_TX_PADDING: u32 = 0x0004;
    pub const CAN_ISOTP_RX_PADDING: u32 = 0x0008;
    pub const CAN_ISOTP_CHK_PAD_LEN: u32 = 0x0010;
    pub const CAN_ISOTP_CHK_PAD_DATA: u32 = 0x0020;
    pub const CAN_ISOTP_HALF_DUPLEX: u32 = 0x0040;
    pub const CAN_ISOTP_FORCE_TXSTMIN: u32 = 0x0080;
    pub const CAN_ISOTP_FORCE_RXSTMIN: u32 = 0x0100;
    pub const CAN_ISOTP_RX_EXT_ADDR: u32 = 0x0200;
    pub const CAN_ISOTP_WAIT_TX_DONE: u32 = 0x0400;
    pub const CAN_ISOTP_SF_BROADCAST: u32 = 0x0800;
    pub const CAN_ISOTP_CF_BROADCAST: u32 = 0x1000;
    pub const CAN_ISOTP_DYN_FC_PARMS: u32 = 0x2000;
}
