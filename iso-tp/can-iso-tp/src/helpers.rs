//! Ergonomic helpers for common ISO-TP demux setups.

#![cfg(feature = "uds")]

use core::time::Duration;

use can_uds::uds29;
use embedded_can::Frame;
use embedded_can_interface::{AsyncRxFrameIo, AsyncTxFrameIo};

use crate::{IsoTpAsyncDemux, IsoTpConfig, IsoTpError, Clock};

/// Builder for a 29-bit UDS async demux with sane defaults.
///
/// This hides the boilerplate of setting up base IDs and wiring borrowed RX buffers.
/// Callers can override any ISO-TP config field they care about.
pub struct Uds29AsyncDemuxBuilder<Tx, Rx, C> {
    tx: Tx,
    rx: Rx,
    clock: C,
    local_addr: u8,
    cfg: IsoTpConfig,
}

impl<Tx, Rx, C> Uds29AsyncDemuxBuilder<Tx, Rx, C>
where
    Tx: AsyncTxFrameIo,
    Rx: AsyncRxFrameIo<Frame = Tx::Frame, Error = Tx::Error>,
    Tx::Frame: Frame,
    C: Clock,
{
    /// Start a new builder for a local UDS-29 address.
    pub fn new(tx: Tx, rx: Rx, clock: C, local_addr: u8) -> Self {
        let mut cfg = IsoTpConfig::default();
        // Provide distinct, valid 29-bit IDs as a baseline; the demux rewrites IDs per peer.
        cfg.tx_id = uds29::encode_phys_id(0, local_addr);
        cfg.rx_id = uds29::encode_phys_id(local_addr, 0);
        Self {
            tx,
            rx,
            clock,
            local_addr,
            cfg,
        }
    }

    /// Replace the entire ISO-TP config.
    pub fn with_config(mut self, cfg: IsoTpConfig) -> Self {
        self.cfg = cfg;
        self
    }

    /// Set the maximum payload length.
    pub fn max_payload_len(mut self, len: usize) -> Self {
        self.cfg.max_payload_len = len;
        self
    }

    /// Set the CAN frame length (DLC).
    pub fn frame_len(mut self, len: usize) -> Self {
        self.cfg.frame_len = len;
        self
    }

    /// Set the FlowControl block size.
    pub fn block_size(mut self, bs: u8) -> Self {
        self.cfg.block_size = bs;
        self
    }

    /// Set the FlowControl minimum separation time.
    pub fn st_min(mut self, st: Duration) -> Self {
        self.cfg.st_min = st;
        self
    }

    /// Set the optional padding byte.
    pub fn padding(mut self, pad: Option<u8>) -> Self {
        self.cfg.padding = pad;
        self
    }

    /// Set all ISO-TP timeouts to the same duration.
    pub fn timeouts_all(mut self, t: Duration) -> Self {
        self.cfg.n_as = t;
        self.cfg.n_ar = t;
        self.cfg.n_bs = t;
        self.cfg.n_br = t;
        self.cfg.n_cs = t;
        self
    }

    /// Set ISO-TP timeouts individually.
    pub fn timeouts(
        mut self,
        n_as: Duration,
        n_ar: Duration,
        n_bs: Duration,
        n_br: Duration,
        n_cs: Duration,
    ) -> Self {
        self.cfg.n_as = n_as;
        self.cfg.n_ar = n_ar;
        self.cfg.n_bs = n_bs;
        self.cfg.n_br = n_br;
        self.cfg.n_cs = n_cs;
        self
    }

    /// Build an async demux using borrowed RX buffers (one per peer).
    pub fn build_with_borrowed_rx_buffers<'a, const MAX: usize, const L: usize>(
        self,
        bufs: &'a mut [[u8; L]; MAX],
    ) -> Result<IsoTpAsyncDemux<'a, Tx, Rx, Tx::Frame, C, MAX>, IsoTpError<()>> {
        IsoTpAsyncDemux::with_borrowed_rx_buffers(
            self.tx,
            self.rx,
            self.cfg,
            self.clock,
            self.local_addr,
            bufs,
        )
    }
}
