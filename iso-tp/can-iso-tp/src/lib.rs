//! `can-iso-tp`: a lightweight ISO-TP (ISO 15765-2) transport layer for CAN.
//!
//! ISO-TP (“ISO Transport Protocol”) defines how to carry *larger* payloads over classic CAN by
//! segmenting messages into:
//! - a **Single Frame** (small payloads),
//! - a **First Frame** + multiple **Consecutive Frames** (larger payloads), and
//! - **Flow Control** frames to regulate pacing and batching.
//!
//! This crate provides:
//! - A blocking/polling node ([`IsoTpNode`]) built on split CAN Tx/Rx halves from
//!   `embedded-can-interface`.
//! - An async node ([`IsoTpAsyncNode`]) for async runtimes via [`AsyncRuntime`].
//! - Small supporting building blocks (`rx`, `tx`, `pdu`, config and addressing helpers).
//!
//! The public API is designed to be usable in `no_std` environments. Allocation is optional (see
//! feature flags). Regardless of allocator availability, receive-side reassembly requires an
//! explicit caller-provided buffer (borrowed or owned).
//!
//! # Feature flags
//! - `std` (default): enables allocation support and exports [`StdClock`].
//! - `alloc`: enables allocation support without the Rust standard library (e.g. to use
//!   [`RxStorage::Owned`]).
//! - `uds`: enables 29-bit UDS “normal fixed” addressing helpers (`can-uds`) and multi-peer
//!   demultiplexers ([`IsoTpDemux`], [`IsoTpAsyncDemux`]) that provide `reply_to` metadata.
//!
//! # Concepts
//! - **Addressing**: ISO-TP supports “normal” addressing (just CAN IDs), and extended/mixed modes
//!   where an additional *addressing byte* is inserted before the PCI. This crate models that via
//!   [`IsoTpConfig::tx_addr`] / [`IsoTpConfig::rx_addr`] and helpers in [`address`].
//! - **PCI offset**: when an addressing byte is present, the Protocol Control Information starts at
//!   byte 1 rather than byte 0. Helpers like [`IsoTpConfig::tx_pci_offset`] and
//!   [`pdu::decode_with_offset`] keep the encoding/decoding logic consistent.
//! - **Progress**: the non-blocking API uses [`Progress`] to indicate whether a step completed,
//!   needs more work, or would block.
//!
//! # Quick start
//! The `IsoTpNode` API is structured as:
//! - **polling**: repeatedly call [`IsoTpNode::poll_send`] / [`IsoTpNode::poll_recv`], or
//! - **blocking**: call [`IsoTpNode::send`] / [`IsoTpNode::recv`], which spin internally until done
//!   or timeout.
//!
//! This crate does not ship a concrete CAN driver. For tests and examples, the workspace includes
//! `embedded-can-mock` which implements `embedded-can-interface`.
//!
//! ```rust,ignore
//! use core::time::Duration;
//! use can_iso_tp::{IsoTpConfig, IsoTpNode, Progress, StdClock};
//! use embedded_can_interface::SplitTxRx;
//!
//! # fn example<D, F>(driver: D, cfg: IsoTpConfig) -> Result<(), can_iso_tp::IsoTpError<D::Error>>
//! # where
//! #   D: SplitTxRx,
//! #   D::Tx: embedded_can_interface::TxFrameIo<Frame = F>,
//! #   D::Rx: embedded_can_interface::RxFrameIo<Frame = F, Error = <D::Tx as embedded_can_interface::TxFrameIo>::Error>,
//! #   F: embedded_can::Frame,
//! # {
//! let (tx, rx) = driver.split();
//! let mut rx_buf = [0u8; 4095];
//! let mut node = IsoTpNode::with_clock(tx, rx, cfg, StdClock, &mut rx_buf).unwrap();
//!
//! // Send (blocking)
//! node.send(b"hello", Duration::from_millis(100))?;
//!
//! // Receive (polling)
//! let mut out = Vec::new();
//! loop {
//!     // In a superloop, you typically pass your current time here.
//!     let clock = StdClock;
//!     let now = clock.now();
//!     match node.poll_recv(now, &mut |data| out = data.to_vec())? {
//!         Progress::Completed => break,
//!         Progress::InFlight | Progress::WaitingForFlowControl | Progress::WouldBlock => continue,
//!     }
//! }
//! # Ok(()) }
//! ```

#![cfg_attr(not(feature = "std"), no_std)]

#[cfg(any(feature = "alloc", feature = "std"))]
extern crate alloc;

pub mod address;
#[cfg(feature = "uds")]
mod async_demux;
pub mod async_io;
pub mod async_node;
pub mod config;
#[cfg(feature = "uds")]
pub mod demux;
pub mod errors;
#[cfg(feature = "isotp-interface")]
mod interface_impl;
pub mod pdu;
pub mod rx;
pub mod timer;
pub mod tx;

pub use address::{AsymmetricAddress, IsoTpAddress, RxAddress, TargetAddressType, TxAddress};
#[cfg(feature = "uds")]
pub use async_demux::{
    AppRecvIntoError, IsoTpAsyncDemux, IsoTpAsyncDemuxApp, IsoTpAsyncDemuxDriver,
};
pub use async_io::{AsyncRuntime, TimedOut};
pub use async_node::IsoTpAsyncNode;
pub use config::IsoTpConfig;
#[cfg(feature = "uds")]
pub use demux::{IsoTpDemux, rx_storages_from_buffers};
pub use errors::{IsoTpError, TimeoutKind};
#[cfg(feature = "isotp-interface")]
pub use interface_impl::AsyncWithRt;
pub use rx::RxStorage;
pub use timer::Clock;
#[cfg(feature = "std")]
pub use timer::StdClock;
pub use tx::Progress;

/// Receive-side ISO-TP flow-control parameters (BS/STmin).
///
/// These values are advertised to the remote sender in FlowControl (FC) frames. Updating them at
/// runtime allows shaping the sender's rate based on backpressure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RxFlowControl {
    /// Block size (0 = unlimited).
    pub block_size: u8,
    /// Minimum separation time between consecutive frames.
    pub st_min: Duration,
}

impl RxFlowControl {
    /// Build flow-control parameters from a node's static configuration.
    pub fn from_config(cfg: &IsoTpConfig) -> Self {
        Self {
            block_size: cfg.block_size,
            st_min: cfg.st_min,
        }
    }
}

use core::mem;
use core::time::Duration;
use embedded_can::Frame;
use embedded_can_interface::{Id, RxFrameIo, TxFrameIo};

use pdu::{
    FlowStatus, Pdu, decode_with_offset, duration_to_st_min, encode_with_prefix_sized,
    st_min_to_duration,
};
use rx::{RxMachine, RxOutcome};
use tx::{TxSession, TxState};

/// Alias for CAN identifier.
pub type CanId = Id;
/// Re-export of the CAN frame trait.
pub use embedded_can::Frame as CanFrame;

/// ISO-TP endpoint backed by split transmit/receive halves and a clock.
pub struct IsoTpNode<'a, Tx, Rx, F, C>
where
    Tx: TxFrameIo<Frame = F>,
    Rx: RxFrameIo<Frame = F, Error = Tx::Error>,
    C: Clock,
{
    tx: Tx,
    rx: Rx,
    cfg: IsoTpConfig,
    rx_flow_control: RxFlowControl,
    clock: C,
    tx_state: TxState<C::Instant>,
    rx_machine: RxMachine<'a>,
}

impl<'a, Tx, Rx, F, C> IsoTpNode<'a, Tx, Rx, F, C>
where
    Tx: TxFrameIo<Frame = F>,
    Rx: RxFrameIo<Frame = F, Error = Tx::Error>,
    F: Frame,
    C: Clock,
{
    /// Start building an [`IsoTpNode`] (requires RX storage before `build()`).
    pub fn builder(tx: Tx, rx: Rx, cfg: IsoTpConfig, clock: C) -> IsoTpNodeBuilder<Tx, Rx, C> {
        IsoTpNodeBuilder { tx, rx, cfg, clock }
    }

    /// Construct using a provided clock and caller-provided RX buffer.
    ///
    /// ISO-TP receive-side reassembly requires a buffer for storing the in-flight payload.
    /// In `no_std`/`no alloc` environments, this is typically a static or stack-allocated slice.
    pub fn with_clock(
        tx: Tx,
        rx: Rx,
        cfg: IsoTpConfig,
        clock: C,
        rx_buffer: &'a mut [u8],
    ) -> Result<Self, IsoTpError<()>> {
        Self::with_clock_and_storage(tx, rx, cfg, clock, RxStorage::Borrowed(rx_buffer))
    }

    /// Construct using a provided clock and explicit RX storage.
    pub fn with_clock_and_storage(
        tx: Tx,
        rx: Rx,
        cfg: IsoTpConfig,
        clock: C,
        rx_storage: RxStorage<'a>,
    ) -> Result<Self, IsoTpError<()>> {
        let node = Self::builder(tx, rx, cfg, clock)
            .rx_storage(rx_storage)
            .build()?;
        Ok(node)
    }

    /// Get the current receive-side FlowControl parameters (BS/STmin).
    pub fn rx_flow_control(&self) -> RxFlowControl {
        self.rx_flow_control
    }

    /// Update receive-side FlowControl parameters (BS/STmin).
    ///
    /// This affects FlowControl frames emitted in response to segmented transfers.
    pub fn set_rx_flow_control(&mut self, fc: RxFlowControl) {
        self.rx_flow_control = fc;
    }

    /// Advance transmission once; caller supplies current time.
    ///
    /// This method is appropriate for “superloop” style designs where you poll the node until it
    /// reports [`Progress::Completed`]. The node maintains internal state between calls.
    pub fn poll_send(
        &mut self,
        payload: &[u8],
        now: C::Instant,
    ) -> Result<Progress, IsoTpError<Tx::Error>> {
        // if cfg!(debug_assertions) {
        //     match &self.tx_state {
        //         TxState::Idle => println!("DEBUG poll_send state=Idle"),
        //         TxState::WaitingForFc { .. } => println!("DEBUG poll_send state=WaitingForFc"),
        //         TxState::Sending {
        //             st_min_deadline, ..
        //         } => println!(
        //             "DEBUG poll_send state=Sending deadline_set={}",
        //             st_min_deadline.is_some()
        //         ),
        //     };
        // }
        if payload.len() > self.cfg.max_payload_len {
            return Err(IsoTpError::Overflow);
        }

        let state = mem::replace(&mut self.tx_state, TxState::Idle);
        match state {
            TxState::Idle => self.start_send(payload, now),
            TxState::WaitingForFc { session, deadline } => {
                self.continue_wait_for_fc(payload, session, deadline, now)
            }
            TxState::Sending {
                session,
                st_min_deadline,
            } => self.continue_send(payload, session, st_min_deadline, now),
        }
    }

    /// Blocking send until completion or timeout.
    ///
    /// Internally this repeatedly calls [`IsoTpNode::poll_send`] until completion or a timeout is
    /// reached.
    pub fn send(&mut self, payload: &[u8], timeout: Duration) -> Result<(), IsoTpError<Tx::Error>> {
        let start = self.clock.now();
        let deadline = self.clock.add(start, timeout);
        loop {
            let now = self.clock.now();
            if now >= deadline {
                return Err(IsoTpError::Timeout(TimeoutKind::NAs));
            }
            match self.poll_send(payload, now)? {
                Progress::Completed => return Ok(()),
                Progress::WaitingForFlowControl | Progress::InFlight | Progress::WouldBlock => {
                    continue;
                }
            }
        }
    }

    /// Non-blocking receive step; delivers bytes when complete.
    ///
    /// The provided `deliver` callback is invoked only when a full payload has been reassembled.
    /// The slice passed to `deliver` is valid until the next receive operation mutates the internal
    /// reassembly buffer.
    pub fn poll_recv(
        &mut self,
        _now: C::Instant,
        deliver: &mut dyn FnMut(&[u8]),
    ) -> Result<Progress, IsoTpError<Tx::Error>> {
        loop {
            let frame = match self.rx.try_recv() {
                Ok(frame) => frame,
                Err(_) => {
                    return Ok(Progress::WouldBlock);
                }
            };

            if !id_matches(frame.id(), &self.cfg.rx_id) {
                continue;
            }
            if let Some(expected) = self.cfg.rx_addr
                && frame.data().first().copied() != Some(expected)
            {
                continue;
            }

            let pdu = decode_with_offset(frame.data(), self.cfg.rx_pci_offset())
                .map_err(|_| IsoTpError::InvalidFrame)?;
            if matches!(pdu, Pdu::FlowControl { .. }) {
                continue;
            }
            let outcome = match self
                .rx_machine
                .on_pdu(&self.cfg, &self.rx_flow_control, pdu)
            {
                Ok(o) => o,
                Err(IsoTpError::Overflow) => {
                    let _ = self.send_overflow_fc();
                    return Err(IsoTpError::RxOverflow);
                }
                Err(IsoTpError::UnexpectedPdu) => continue,
                Err(IsoTpError::BadSequence) => return Err(IsoTpError::BadSequence),
                Err(IsoTpError::InvalidFrame) => return Err(IsoTpError::InvalidFrame),
                Err(IsoTpError::InvalidConfig) => return Err(IsoTpError::InvalidConfig),
                Err(IsoTpError::Timeout(kind)) => return Err(IsoTpError::Timeout(kind)),
                Err(IsoTpError::WouldBlock) => return Err(IsoTpError::WouldBlock),
                Err(IsoTpError::RxOverflow) => return Err(IsoTpError::RxOverflow),
                Err(IsoTpError::NotIdle) => return Err(IsoTpError::NotIdle),
                Err(IsoTpError::LinkError(_)) => return Err(IsoTpError::InvalidFrame),
            };

            match outcome {
                RxOutcome::None => return Ok(Progress::InFlight),
                RxOutcome::SendFlowControl {
                    status,
                    block_size,
                    st_min,
                } => {
                    self.send_flow_control(status, block_size, st_min)?;
                    return Ok(Progress::InFlight);
                }
                RxOutcome::Completed(_len) => {
                    let data = self.rx_machine.take_completed();
                    deliver(data);
                    return Ok(Progress::Completed);
                }
            }
        }
    }

    /// Blocking receive until a full payload arrives or timeout.
    ///
    /// Internally this repeatedly calls [`IsoTpNode::poll_recv`] until completion or a timeout is
    /// reached.
    pub fn recv(
        &mut self,
        timeout: Duration,
        deliver: &mut dyn FnMut(&[u8]),
    ) -> Result<(), IsoTpError<Tx::Error>> {
        let start = self.clock.now();
        let deadline = self.clock.add(start, timeout);
        loop {
            let now = self.clock.now();
            if now >= deadline {
                return Err(IsoTpError::Timeout(TimeoutKind::NAr));
            }
            match self.poll_recv(now, deliver)? {
                Progress::Completed => return Ok(()),
                Progress::InFlight | Progress::WaitingForFlowControl | Progress::WouldBlock => {
                    continue;
                }
            }
        }
    }

    fn start_send(
        &mut self,
        payload: &[u8],
        now: C::Instant,
    ) -> Result<Progress, IsoTpError<Tx::Error>> {
        if payload.len() <= self.cfg.max_single_frame_payload() {
            let pdu = Pdu::SingleFrame {
                len: payload.len() as u8,
                data: payload,
            };
            let frame = encode_with_prefix_sized(
                self.cfg.tx_id,
                &pdu,
                self.cfg.padding,
                self.cfg.tx_addr,
                self.cfg.frame_len,
            )
            .map_err(|_| IsoTpError::InvalidFrame)?;
            self.tx.try_send(&frame).map_err(IsoTpError::LinkError)?;
            self.tx_state = TxState::Idle;
            return Ok(Progress::Completed);
        }

        {
            let mut session = TxSession::new(payload.len(), self.cfg.block_size, self.cfg.st_min);
            let len = payload.len();
            let chunk = payload.len().min(self.cfg.max_first_frame_payload());
            let pdu = Pdu::FirstFrame {
                len: len as u16,
                data: &payload[..chunk],
            };
            let frame = encode_with_prefix_sized(
                self.cfg.tx_id,
                &pdu,
                self.cfg.padding,
                self.cfg.tx_addr,
                self.cfg.frame_len,
            )
            .map_err(|_| IsoTpError::InvalidFrame)?;
            self.tx.try_send(&frame).map_err(IsoTpError::LinkError)?;

            session.offset = chunk;

            let deadline = self.clock.add(now, self.cfg.n_bs);
            self.tx_state = TxState::WaitingForFc { session, deadline };
            Ok(Progress::WaitingForFlowControl)
        }
    }

    fn continue_wait_for_fc(
        &mut self,
        payload: &[u8],
        mut session: TxSession,
        deadline: C::Instant,
        now: C::Instant,
    ) -> Result<Progress, IsoTpError<Tx::Error>> {
        if payload.len() != session.payload_len {
            self.tx_state = TxState::Idle;
            return Err(IsoTpError::NotIdle);
        }
        if session.wait_count > self.cfg.wft_max {
            self.tx_state = TxState::Idle;
            return Err(IsoTpError::Timeout(TimeoutKind::NBs));
        }
        if now >= deadline {
            return Err(IsoTpError::Timeout(TimeoutKind::NBs));
        }
        loop {
            let frame = match self.rx.try_recv() {
                Ok(f) => f,
                Err(_) => {
                    self.tx_state = TxState::WaitingForFc { session, deadline };
                    return Ok(Progress::WaitingForFlowControl);
                }
            };
            if !id_matches(frame.id(), &self.cfg.rx_id) {
                continue;
            }
            if let Some(expected) = self.cfg.rx_addr
                && frame.data().first().copied() != Some(expected)
            {
                continue;
            }
            let pdu = decode_with_offset(frame.data(), self.cfg.rx_pci_offset())
                .map_err(|_| IsoTpError::InvalidFrame)?;
            match pdu {
                Pdu::FlowControl {
                    status,
                    block_size,
                    st_min,
                } => match status {
                    FlowStatus::ClearToSend => {
                        session.wait_count = 0;
                        let bs = if block_size == 0 {
                            self.cfg.block_size
                        } else {
                            block_size
                        };
                        session.block_size = bs;
                        session.block_remaining = bs;
                        session.st_min = st_min_to_duration(st_min).unwrap_or(self.cfg.st_min);
                        return self.continue_send(payload, session, None, now);
                    }
                    FlowStatus::Wait => {
                        #[cfg(feature = "std")]
                        if cfg!(debug_assertions) {
                            println!("DEBUG wait_count before {}", session.wait_count);
                        }
                        session.wait_count = session.wait_count.saturating_add(1);
                        if session.wait_count > self.cfg.wft_max {
                            let deadline = self.clock.add(now, self.cfg.n_bs);
                            self.tx_state = TxState::WaitingForFc { session, deadline };
                            return Err(IsoTpError::Timeout(TimeoutKind::NBs));
                        }
                        let new_deadline = self.clock.add(now, self.cfg.n_bs);
                        self.tx_state = TxState::WaitingForFc {
                            session,
                            deadline: new_deadline,
                        };
                        return Ok(Progress::WaitingForFlowControl);
                    }
                    FlowStatus::Overflow => {
                        self.tx_state = TxState::Idle;
                        return Err(IsoTpError::Overflow);
                    }
                },
                _ => continue,
            }
        }
    }

    fn continue_send(
        &mut self,
        payload: &[u8],
        mut session: TxSession,
        st_min_deadline: Option<C::Instant>,
        now: C::Instant,
    ) -> Result<Progress, IsoTpError<Tx::Error>> {
        if payload.len() != session.payload_len {
            self.tx_state = TxState::Idle;
            return Err(IsoTpError::NotIdle);
        }
        if let Some(deadline) = st_min_deadline
            && now < deadline
        {
            self.tx_state = TxState::Sending {
                session,
                st_min_deadline: Some(deadline),
            };
            return Ok(Progress::WouldBlock);
        }

        if session.offset >= session.payload_len {
            self.tx_state = TxState::Idle;
            return Ok(Progress::Completed);
        }

        let remaining = session.payload_len - session.offset;
        let chunk = remaining.min(self.cfg.max_consecutive_frame_payload());
        let data = &payload[session.offset..session.offset + chunk];
        let pdu = Pdu::ConsecutiveFrame {
            sn: session.next_sn & 0x0F,
            data,
        };
        let frame = encode_with_prefix_sized(
            self.cfg.tx_id,
            &pdu,
            self.cfg.padding,
            self.cfg.tx_addr,
            self.cfg.frame_len,
        )
        .map_err(|_| IsoTpError::InvalidFrame)?;
        self.tx.try_send(&frame).map_err(IsoTpError::LinkError)?;

        session.offset += chunk;
        session.next_sn = (session.next_sn + 1) & 0x0F;

        if session.offset >= session.payload_len {
            self.tx_state = TxState::Idle;
            return Ok(Progress::Completed);
        }

        if session.block_size > 0 {
            session.block_remaining = session.block_remaining.saturating_sub(1);
            if session.block_remaining == 0 {
                session.block_remaining = session.block_size;
                let deadline = self.clock.add(now, self.cfg.n_bs);
                self.tx_state = TxState::WaitingForFc { session, deadline };
                return Ok(Progress::WaitingForFlowControl);
            }
        }

        let next_deadline = if session.st_min > Duration::from_millis(0) {
            Some(self.clock.add(now, session.st_min))
        } else {
            None
        };
        self.tx_state = TxState::Sending {
            session,
            st_min_deadline: next_deadline,
        };
        Ok(Progress::InFlight)
    }

    fn send_flow_control(
        &mut self,
        status: FlowStatus,
        block_size: u8,
        st_min: u8,
    ) -> Result<(), IsoTpError<Tx::Error>> {
        let fc = Pdu::FlowControl {
            status,
            block_size,
            st_min,
        };
        let frame = encode_with_prefix_sized(
            self.cfg.tx_id,
            &fc,
            self.cfg.padding,
            self.cfg.tx_addr,
            self.cfg.frame_len,
        )
        .map_err(|_| IsoTpError::InvalidFrame)?;
        self.tx.try_send(&frame).map_err(IsoTpError::LinkError)?;
        Ok(())
    }

    fn send_overflow_fc(&mut self) -> Result<(), IsoTpError<Tx::Error>> {
        self.send_flow_control(FlowStatus::Overflow, 0, duration_to_st_min(self.cfg.st_min))
    }
}

fn id_matches(actual: embedded_can::Id, expected: &Id) -> bool {
    match (actual, expected) {
        (embedded_can::Id::Standard(a), Id::Standard(b)) => a == *b,
        (embedded_can::Id::Extended(a), Id::Extended(b)) => a == *b,
        _ => false,
    }
}

#[cfg(feature = "std")]
impl<'a, Tx, Rx, F> IsoTpNode<'a, Tx, Rx, F, StdClock>
where
    Tx: TxFrameIo<Frame = F>,
    Rx: RxFrameIo<Frame = F, Error = Tx::Error>,
    F: Frame,
{
    /// Convenience constructor using `StdClock`.
    pub fn with_std_clock(
        tx: Tx,
        rx: Rx,
        cfg: IsoTpConfig,
        rx_buffer: &'a mut [u8],
    ) -> Result<Self, IsoTpError<()>> {
        IsoTpNode::with_clock(tx, rx, cfg, StdClock, rx_buffer)
    }
}

/// Builder for [`IsoTpNode`] that enforces providing RX storage before construction.
pub struct IsoTpNodeBuilder<Tx, Rx, C> {
    tx: Tx,
    rx: Rx,
    cfg: IsoTpConfig,
    clock: C,
}

/// Builder state after RX storage has been provided.
pub struct IsoTpNodeBuilderWithRx<'a, Tx, Rx, C> {
    tx: Tx,
    rx: Rx,
    cfg: IsoTpConfig,
    clock: C,
    rx_storage: RxStorage<'a>,
}

impl<Tx, Rx, C> IsoTpNodeBuilder<Tx, Rx, C> {
    /// Provide the RX buffer used for receive-side reassembly.
    pub fn rx_buffer<'a>(self, buffer: &'a mut [u8]) -> IsoTpNodeBuilderWithRx<'a, Tx, Rx, C> {
        self.rx_storage(RxStorage::Borrowed(buffer))
    }

    /// Provide explicit RX storage (borrowed or owned, depending on features).
    pub fn rx_storage<'a>(
        self,
        rx_storage: RxStorage<'a>,
    ) -> IsoTpNodeBuilderWithRx<'a, Tx, Rx, C> {
        IsoTpNodeBuilderWithRx {
            tx: self.tx,
            rx: self.rx,
            cfg: self.cfg,
            clock: self.clock,
            rx_storage,
        }
    }
}

impl<'a, Tx, Rx, C> IsoTpNodeBuilderWithRx<'a, Tx, Rx, C>
where
    Tx: TxFrameIo,
    Rx: RxFrameIo<Frame = <Tx as TxFrameIo>::Frame, Error = <Tx as TxFrameIo>::Error>,
    <Tx as TxFrameIo>::Frame: Frame,
    C: Clock,
{
    /// Validate configuration and build an [`IsoTpNode`].
    pub fn build(
        self,
    ) -> Result<IsoTpNode<'a, Tx, Rx, <Tx as TxFrameIo>::Frame, C>, IsoTpError<()>> {
        self.cfg.validate().map_err(|_| IsoTpError::InvalidConfig)?;
        if self.rx_storage.capacity() < self.cfg.max_payload_len {
            return Err(IsoTpError::InvalidConfig);
        }
        let rx_flow_control = RxFlowControl::from_config(&self.cfg);
        Ok(IsoTpNode {
            tx: self.tx,
            rx: self.rx,
            cfg: self.cfg,
            rx_flow_control,
            clock: self.clock,
            tx_state: TxState::Idle,
            rx_machine: RxMachine::new(self.rx_storage),
        })
    }
}
