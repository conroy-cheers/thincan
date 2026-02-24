//! Receive-side reassembly and flow-control decisions.

use core::cmp::min;

use crate::RxFlowControl;
use crate::config::IsoTpConfig;
use crate::errors::IsoTpError;
use crate::pdu::{FlowStatus, Pdu, duration_to_st_min};

#[cfg(any(feature = "alloc", feature = "std"))]
use alloc::vec::Vec;

/// Storage for reassembling an incoming payload.
///
/// `can-iso-tp` supports both:
/// - caller-provided buffers (common in `no_std`), and
/// - owned buffers when allocation is available.
pub enum RxStorage<'a> {
    /// Caller-provided slice.
    Borrowed(&'a mut [u8]),
    #[cfg(any(feature = "alloc", feature = "std"))]
    /// Owned buffer when allocation is available.
    Owned(Vec<u8>),
}

impl<'a> RxStorage<'a> {
    /// Total writable capacity.
    pub fn capacity(&self) -> usize {
        match self {
            RxStorage::Borrowed(buf) => buf.len(),
            #[cfg(any(feature = "alloc", feature = "std"))]
            RxStorage::Owned(buf) => buf.len(),
        }
    }

    /// Mutable view of the buffer.
    #[allow(clippy::should_implement_trait)]
    pub fn as_mut(&mut self) -> &mut [u8] {
        match self {
            RxStorage::Borrowed(buf) => buf,
            #[cfg(any(feature = "alloc", feature = "std"))]
            RxStorage::Owned(buf) => buf.as_mut_slice(),
        }
    }
}

/// High-level receive state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RxState {
    /// No transfer active.
    Idle,
    /// In-progress segmented transfer.
    Receiving,
}

/// Outcome after processing a PDU.
pub enum RxOutcome {
    /// Nothing to send back yet.
    None,
    /// Emit a flow control frame.
    SendFlowControl {
        /// Flow status to transmit back to the sender.
        status: FlowStatus,
        /// Block size for the sender (0 = unlimited).
        block_size: u8,
        /// Encoded STmin value to send (not a `Duration`).
        st_min: u8,
    },
    /// Payload complete with length.
    Completed(usize),
}

/// Receive state machine.
pub struct RxMachine<'a> {
    /// Current receive state (idle vs receiving).
    pub state: RxState,
    buffer: RxStorage<'a>,
    written: usize,
    expected_len: usize,
    next_sn: u8,
    block_size: u8,
    block_remaining: u8,
}

impl<'a> RxMachine<'a> {
    /// Create a new machine with the provided buffer.
    pub fn new(buffer: RxStorage<'a>) -> Self {
        Self {
            state: RxState::Idle,
            buffer,
            written: 0,
            expected_len: 0,
            next_sn: 0,
            block_size: 0,
            block_remaining: 0,
        }
    }

    /// Clear state to idle.
    pub fn reset(&mut self) {
        isotp_trace!("rx reset");
        self.state = RxState::Idle;
        self.written = 0;
        self.expected_len = 0;
        self.next_sn = 0;
        self.block_size = 0;
        self.block_remaining = 0;
    }

    #[allow(dead_code)]
    /// Access completed bytes.
    pub fn buffer(&self) -> &[u8] {
        &self.buffer.as_ref()[..self.written]
    }

    #[allow(dead_code)]
    /// Writable view of the buffer.
    pub fn buffer_mut(&mut self) -> &mut [u8] {
        let len = self.buffer.capacity();
        &mut self.buffer.as_mut()[..len]
    }

    fn take_buffer_slice(&self) -> &[u8] {
        match &self.buffer {
            RxStorage::Borrowed(buf) => &buf[..self.written],
            #[cfg(any(feature = "alloc", feature = "std"))]
            RxStorage::Owned(buf) => &buf[..self.written],
        }
    }

    /// Length of the currently completed payload.
    pub fn completed_len(&self) -> usize {
        self.written
    }

    /// Replace internal storage and return the previous storage.
    pub fn replace_storage(&mut self, mut replacement: RxStorage<'a>) -> RxStorage<'a> {
        core::mem::swap(&mut self.buffer, &mut replacement);
        replacement
    }

    /// Handle an incoming PDU and return actions to take.
    ///
    /// The caller is responsible for:
    /// - feeding PDUs in-order for a given session, and
    /// - sending flow-control frames when [`RxOutcome::SendFlowControl`] is returned.
    pub fn on_pdu(
        &mut self,
        cfg: &IsoTpConfig,
        rx_fc: &RxFlowControl,
        pdu: Pdu<'_>,
    ) -> Result<RxOutcome, IsoTpError<()>> {
        match pdu {
            Pdu::SingleFrame { len, data } => self.handle_single(cfg, len, data),
            Pdu::FirstFrame { len, data } => self.handle_first(cfg, rx_fc, len, data),
            Pdu::ConsecutiveFrame { sn, data } => self.handle_consecutive(cfg, rx_fc, sn, data),
            Pdu::FlowControl { .. } => Err(IsoTpError::UnexpectedPdu),
        }
    }

    fn handle_single(
        &mut self,
        cfg: &IsoTpConfig,
        len: u8,
        data: &[u8],
    ) -> Result<RxOutcome, IsoTpError<()>> {
        if self.state != RxState::Idle {
            isotp_warn!(
                "rx single unexpected receiving={}",
                self.state == RxState::Receiving
            );
            return Err(IsoTpError::UnexpectedPdu);
        }
        let len = len as usize;
        if len > cfg.max_payload_len || len > data.len() || len > self.buffer.capacity() {
            isotp_warn!(
                "rx single overflow len={} data_len={} cap={} max={}",
                len,
                data.len(),
                self.buffer.capacity(),
                cfg.max_payload_len
            );
            return Err(IsoTpError::Overflow);
        }
        self.buffer.as_mut()[..len].copy_from_slice(&data[..len]);
        self.written = len;
        isotp_debug!("rx single complete len={}", len);
        Ok(RxOutcome::Completed(len))
    }

    fn handle_first(
        &mut self,
        cfg: &IsoTpConfig,
        rx_fc: &RxFlowControl,
        len: u16,
        data: &[u8],
    ) -> Result<RxOutcome, IsoTpError<()>> {
        if self.state == RxState::Receiving {
            // Restart reassembly on a new FF from the same stream. This avoids wedging the
            // receiver forever if the previous segmented transfer stalled mid-flight.
            isotp_warn!(
                "rx restart on first-frame written={} expected_len={}",
                self.written,
                self.expected_len
            );
            self.reset();
        } else if self.state != RxState::Idle {
            isotp_warn!("rx first unexpected state while not idle");
            return Err(IsoTpError::UnexpectedPdu);
        }
        let len = len as usize;
        if len > cfg.max_payload_len || len > self.buffer.capacity() {
            isotp_warn!(
                "rx first overflow len={} cap={} max={}",
                len,
                self.buffer.capacity(),
                cfg.max_payload_len
            );
            return Err(IsoTpError::Overflow);
        }
        let copy_len = min(data.len(), len);
        self.buffer.as_mut()[..copy_len].copy_from_slice(&data[..copy_len]);
        self.written = copy_len;
        self.expected_len = len;
        self.next_sn = 1;
        self.block_size = rx_fc.block_size;
        self.block_remaining = rx_fc.block_size;
        self.state = RxState::Receiving;
        isotp_debug!(
            "rx first start expected_len={} copied={} bs={} st_min_ms={}",
            len,
            copy_len,
            rx_fc.block_size,
            rx_fc.st_min.as_millis() as u64
        );
        Ok(RxOutcome::SendFlowControl {
            status: FlowStatus::ClearToSend,
            block_size: rx_fc.block_size,
            st_min: duration_to_st_min(rx_fc.st_min),
        })
    }

    fn handle_consecutive(
        &mut self,
        _cfg: &IsoTpConfig,
        rx_fc: &RxFlowControl,
        sn: u8,
        data: &[u8],
    ) -> Result<RxOutcome, IsoTpError<()>> {
        if self.state != RxState::Receiving {
            isotp_warn!(
                "rx cf unexpected receiving={} sn={}",
                self.state == RxState::Receiving,
                sn
            );
            return Err(IsoTpError::UnexpectedPdu);
        }
        if sn != self.next_sn {
            isotp_warn!(
                "rx cf bad-sequence expected_sn={} got_sn={} written={} expected_len={}",
                self.next_sn,
                sn,
                self.written,
                self.expected_len
            );
            return Err(IsoTpError::BadSequence);
        }
        if self.written >= self.expected_len {
            isotp_warn!(
                "rx cf overflow written={} expected_len={}",
                self.written,
                self.expected_len
            );
            return Err(IsoTpError::Overflow);
        }
        let remaining = self.expected_len - self.written;
        let chunk = min(data.len(), remaining);
        let end = self.written + chunk;
        if end > self.buffer.capacity() {
            isotp_warn!(
                "rx cf cap overflow end={} cap={} chunk={}",
                end,
                self.buffer.capacity(),
                chunk
            );
            return Err(IsoTpError::Overflow);
        }
        self.buffer.as_mut()[self.written..end].copy_from_slice(&data[..chunk]);
        self.written = end;
        self.next_sn = (self.next_sn + 1) & 0x0F;
        isotp_trace!(
            "rx cf accepted sn={} chunk={} written={} expected_len={}",
            sn,
            chunk,
            self.written,
            self.expected_len
        );

        if self.written >= self.expected_len {
            self.state = RxState::Idle;
            isotp_debug!("rx complete len={}", self.written);
            return Ok(RxOutcome::Completed(self.written));
        }

        if self.block_size > 0 {
            self.block_remaining = self.block_remaining.saturating_sub(1);
            if self.block_remaining == 0 {
                // Allow runtime backpressure updates to take effect at FC boundaries.
                self.block_size = rx_fc.block_size;
                self.block_remaining = self.block_size;
                isotp_trace!(
                    "rx cf sending-fc bs={} st_min_ms={}",
                    self.block_size,
                    rx_fc.st_min.as_millis() as u64
                );
                return Ok(RxOutcome::SendFlowControl {
                    status: FlowStatus::ClearToSend,
                    block_size: self.block_size,
                    st_min: duration_to_st_min(rx_fc.st_min),
                });
            }
        }

        Ok(RxOutcome::None)
    }

    /// View the completed message slice.
    ///
    /// The returned slice is backed by this machine’s internal buffer and remains valid until the
    /// next receive operation mutates the machine state.
    pub fn take_completed(&self) -> &[u8] {
        self.take_buffer_slice()
    }
}

impl<'a> AsRef<[u8]> for RxStorage<'a> {
    fn as_ref(&self) -> &[u8] {
        match self {
            RxStorage::Borrowed(buf) => buf,
            #[cfg(any(feature = "alloc", feature = "std"))]
            RxStorage::Owned(buf) => buf.as_slice(),
        }
    }
}

impl<'a> AsMut<[u8]> for RxStorage<'a> {
    fn as_mut(&mut self) -> &mut [u8] {
        match self {
            RxStorage::Borrowed(buf) => buf,
            #[cfg(any(feature = "alloc", feature = "std"))]
            RxStorage::Owned(buf) => buf.as_mut_slice(),
        }
    }
}
