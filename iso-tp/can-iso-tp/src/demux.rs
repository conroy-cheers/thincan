//! Multi-peer ISO-TP demultiplexing for 29-bit UDS-style addressing.
//!
//! ISO-TP itself does not define a sender identity beyond the CAN identifier/addressing context.
//! When multiple senders can target a single receiver ID (common with 29-bit "normal fixed"
//! addressing `0x18DA_TA_SA`), a single receive state machine is insufficient: segmented transfers
//! from different sources can interleave and must be reassembled independently.
//!
//! This module provides [`IsoTpDemux`], which:
//! - routes incoming frames to per-source receive machines,
//! - tracks per-destination transmit sessions (FlowControl is routed by source),
//! - exposes a receive API that includes a `reply_to` (source address) metadata field.

#![cfg(feature = "uds")]

use core::mem;
use core::time::Duration;

use can_uds::uds29::{self, Uds29Kind};
use embedded_can::Frame;
use embedded_can_interface::{RxFrameIo, TxFrameIo};

use crate::config::IsoTpConfig;
use crate::errors::{IsoTpError, TimeoutKind};
use crate::pdu::{
    FlowStatus, Pdu, decode_with_offset, duration_to_st_min, encode_with_prefix_sized,
    st_min_to_duration,
};
use crate::rx::{RxMachine, RxOutcome, RxStorage};
use crate::timer::Clock;
use crate::tx::{Progress, TxSession, TxState};

#[derive(Clone, Copy, PartialEq, Eq)]
struct PendingFc {
    status: FlowStatus,
    block_size: u8,
    st_min: u8,
}

struct Peer<'a, I> {
    remote: u8,
    rx_machine: RxMachine<'a>,
    /// A complete payload is available in `rx_machine` and must be delivered before accepting a
    /// new payload from this peer (to avoid overwriting the reassembly buffer).
    rx_ready: bool,
    tx_state: TxState<I>,
    pending_fc: Option<PendingFc>,
}

impl<'a, I> Peer<'a, I>
where
    I: Copy,
{
    fn new(remote: u8, storage: RxStorage<'a>) -> Self {
        Self {
            remote,
            rx_machine: RxMachine::new(storage),
            rx_ready: false,
            tx_state: TxState::Idle,
            pending_fc: None,
        }
    }
}

struct ReadyQueue<const N: usize> {
    buf: [usize; N],
    head: usize,
    tail: usize,
    len: usize,
}

impl<const N: usize> ReadyQueue<N> {
    fn new() -> Self {
        Self {
            buf: [0; N],
            head: 0,
            tail: 0,
            len: 0,
        }
    }

    fn push(&mut self, v: usize) -> Result<(), ()> {
        if self.len == N {
            return Err(());
        }
        self.buf[self.tail] = v;
        self.tail = (self.tail + 1) % N;
        self.len += 1;
        Ok(())
    }

    fn pop(&mut self) -> Option<usize> {
        if self.len == 0 {
            return None;
        }
        let v = self.buf[self.head];
        self.head = (self.head + 1) % N;
        self.len -= 1;
        Some(v)
    }
}

/// Build `N` borrowed ISO-TP RX storages from a fixed array of RX buffers.
///
/// Each peer session needs a dedicated reassembly buffer.
pub fn rx_storages_from_buffers<'a, const N: usize, const L: usize>(
    bufs: &'a mut [[u8; L]; N],
) -> [RxStorage<'a>; N] {
    use core::mem::MaybeUninit;

    let mut out: MaybeUninit<[RxStorage<'a>; N]> = MaybeUninit::uninit();
    let ptr = out.as_mut_ptr() as *mut RxStorage<'a>;
    for (i, b) in bufs.iter_mut().enumerate() {
        // SAFETY: `ptr` points to uninitialized array storage; each element is written once.
        unsafe {
            ptr.add(i).write(RxStorage::Borrowed(&mut b[..]));
        }
    }
    // SAFETY: every element was written exactly once.
    unsafe { out.assume_init() }
}

/// Multi-peer ISO-TP node for 29-bit UDS physical IDs `0x18DA_TA_SA`.
///
/// - `local_addr` is the receiver's UDS node address (`TA` field).
/// - Per-peer sessions are keyed by `source address` (`SA` field).
///
/// The caller must ensure the underlying CAN interface accepts all frames addressed to
/// `local_addr` (see [`IsoTpDemux::acceptance_filter`]).
pub struct IsoTpDemux<'a, Tx, Rx, F, C, const MAX_PEERS: usize>
where
    Tx: TxFrameIo<Frame = F>,
    Rx: RxFrameIo<Frame = F, Error = Tx::Error>,
    F: Frame,
    C: Clock,
{
    tx: Tx,
    rx: Rx,
    base_cfg: IsoTpConfig,
    clock: C,
    local_addr: u8,
    functional_addr: Option<u8>,
    peers: [Option<Peer<'a, C::Instant>>; MAX_PEERS],
    free: [Option<RxStorage<'a>>; MAX_PEERS],
    ready: ReadyQueue<MAX_PEERS>,
}

impl<'a, Tx, Rx, F, C, const MAX_PEERS: usize> IsoTpDemux<'a, Tx, Rx, F, C, MAX_PEERS>
where
    Tx: TxFrameIo<Frame = F>,
    Rx: RxFrameIo<Frame = F, Error = Tx::Error>,
    F: Frame,
    C: Clock,
{
    /// Create a demux with caller-provided per-peer RX storages.
    ///
    /// `base_cfg` provides ISO-TP timing/limits/padding/address-byte configuration; per-peer
    /// CAN IDs are derived from `local_addr` and the peer address on demand.
    pub fn new(
        tx: Tx,
        rx: Rx,
        base_cfg: IsoTpConfig,
        clock: C,
        local_addr: u8,
        storages: [RxStorage<'a>; MAX_PEERS],
    ) -> Result<Self, IsoTpError<()>> {
        if base_cfg.max_payload_len == 0 || base_cfg.max_payload_len > 4095 {
            return Err(IsoTpError::InvalidConfig);
        }
        for s in &storages {
            if s.capacity() < base_cfg.max_payload_len {
                return Err(IsoTpError::InvalidConfig);
            }
        }

        let mut free: [Option<RxStorage<'a>>; MAX_PEERS] = core::array::from_fn(|_| None);
        for (i, s) in storages.into_iter().enumerate() {
            free[i] = Some(s);
        }

        Ok(Self {
            tx,
            rx,
            base_cfg,
            clock,
            local_addr,
            functional_addr: None,
            peers: core::array::from_fn(|_| None),
            free,
            ready: ReadyQueue::new(),
        })
    }

    /// Enable reception of functional-addressing frames (`0x18DB_TA_SA`) for `functional_addr`.
    ///
    /// Note: ISO-TP segmentation with functional addressing is commonly avoided in practice; this
    /// demux only accepts **Single Frame** payloads on functional IDs.
    pub fn with_functional_addr(mut self, functional_addr: u8) -> Self {
        self.functional_addr = Some(functional_addr);
        self
    }

    /// UDS node address for this demux.
    pub fn local_addr(&self) -> u8 {
        self.local_addr
    }

    /// Acceptance filters that match frames addressed to this node.
    ///
    /// - physical: `0x18DA_TA_SA` where `TA == local_addr`
    /// - functional (optional): `0x18DB_TA_SA` where `TA == functional_addr`
    pub fn acceptance_filters(&self) -> uds29::AcceptanceFilters {
        uds29::filters_for_targets(self.local_addr, self.functional_addr)
    }

    fn cfg_for_peer(&self, remote: u8) -> IsoTpConfig {
        let mut cfg = self.base_cfg.clone();
        cfg.tx_id = uds29::encode_phys_id(remote, self.local_addr);
        cfg.rx_id = uds29::encode_phys_id(self.local_addr, remote);
        cfg
    }

    fn find_peer_index(&self, remote: u8) -> Option<usize> {
        self.peers
            .iter()
            .position(|p| p.as_ref().is_some_and(|pp| pp.remote == remote))
    }

    fn alloc_peer(&mut self, remote: u8) -> Result<usize, IsoTpError<Tx::Error>> {
        if remote == self.local_addr {
            return Err(IsoTpError::InvalidConfig);
        }

        let idx = self
            .peers
            .iter()
            .position(|p| p.is_none())
            .ok_or(IsoTpError::RxOverflow)?;
        let free_idx = self
            .free
            .iter()
            .position(|s| s.is_some())
            .ok_or(IsoTpError::RxOverflow)?;
        let storage = self.free[free_idx].take().ok_or(IsoTpError::RxOverflow)?;
        self.peers[idx] = Some(Peer::new(remote, storage));
        Ok(idx)
    }

    fn peer_index_or_alloc(&mut self, remote: u8) -> Result<usize, IsoTpError<Tx::Error>> {
        match self.find_peer_index(remote) {
            Some(i) => Ok(i),
            None => self.alloc_peer(remote),
        }
    }

    fn ingest_one(&mut self) -> Result<Progress, IsoTpError<Tx::Error>> {
        let frame = match self.rx.try_recv() {
            Ok(f) => f,
            Err(_) => return Ok(Progress::WouldBlock),
        };

        let uds = match uds29::decode_id(frame.id()) {
            Some(v) => v,
            None => return Ok(Progress::InFlight),
        };
        let (kind, target, source) = (uds.kind, uds.target, uds.source);

        match kind {
            Uds29Kind::Physical => {
                if target != self.local_addr {
                    return Ok(Progress::InFlight);
                }
            }
            Uds29Kind::Functional => {
                if Some(target) != self.functional_addr {
                    return Ok(Progress::InFlight);
                }
            }
        }

        let peer_idx = self.peer_index_or_alloc(source)?;
        let cfg = self.cfg_for_peer(source);
        let peer = self.peers[peer_idx].as_mut().expect("peer exists");

        // Enforce "one completed payload buffered per peer" to preserve zero-copy semantics.
        if peer.rx_ready {
            // Still allow FC frames (needed to unblock TX), but ignore all other traffic until the
            // application drains via `poll_recv`/`recv`.
            if let Some(expected) = cfg.rx_addr
                && frame.data().first().copied() != Some(expected)
            {
                return Ok(Progress::InFlight);
            }
            let pdu = decode_with_offset(frame.data(), cfg.rx_pci_offset())
                .map_err(|_| IsoTpError::InvalidFrame)?;
            if let Pdu::FlowControl {
                status,
                block_size,
                st_min,
            } = pdu
            {
                peer.pending_fc = Some(PendingFc {
                    status,
                    block_size,
                    st_min,
                });
            }
            return Ok(Progress::InFlight);
        }

        if let Some(expected) = cfg.rx_addr
            && frame.data().first().copied() != Some(expected)
        {
            return Ok(Progress::InFlight);
        }

        let pdu = decode_with_offset(frame.data(), cfg.rx_pci_offset())
            .map_err(|_| IsoTpError::InvalidFrame)?;

        // Functional addressing is treated as single-frame only.
        if kind == Uds29Kind::Functional && !matches!(pdu, Pdu::SingleFrame { .. }) {
            return Ok(Progress::InFlight);
        }

        match pdu {
            Pdu::FlowControl {
                status,
                block_size,
                st_min,
            } => {
                peer.pending_fc = Some(PendingFc {
                    status,
                    block_size,
                    st_min,
                });
                Ok(Progress::InFlight)
            }
            _ => {
                let outcome = match peer.rx_machine.on_pdu(&cfg, pdu) {
                    Ok(o) => o,
                    Err(IsoTpError::Overflow) => {
                        let _ = Self::send_overflow_fc_frame(&mut self.tx, &cfg);
                        return Err(IsoTpError::RxOverflow);
                    }
                    Err(IsoTpError::UnexpectedPdu) => return Ok(Progress::InFlight),
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
                    RxOutcome::None => Ok(Progress::InFlight),
                    RxOutcome::SendFlowControl {
                        status,
                        block_size,
                        st_min,
                    } => {
                        Self::send_flow_control_frame(
                            &mut self.tx,
                            &cfg,
                            status,
                            block_size,
                            st_min,
                        )?;
                        Ok(Progress::InFlight)
                    }
                    RxOutcome::Completed(_len) => {
                        peer.rx_ready = true;
                        self.ready
                            .push(peer_idx)
                            .map_err(|_| IsoTpError::RxOverflow)?;
                        Ok(Progress::Completed)
                    }
                }
            }
        }
    }

    /// Non-blocking receive step; delivers at most one reassembled payload.
    ///
    /// `deliver(reply_to, payload)` is called when a full payload has been reassembled; the
    /// `payload` slice remains valid until the next receive operation for that same `reply_to`
    /// overwrites the reassembly buffer.
    pub fn poll_recv(
        &mut self,
        _now: C::Instant,
        deliver: &mut dyn FnMut(u8, &[u8]),
    ) -> Result<Progress, IsoTpError<Tx::Error>> {
        if let Some(idx) = self.ready.pop() {
            let peer = self.peers[idx].as_mut().expect("ready peer exists");
            let data = peer.rx_machine.take_completed();
            let reply_to = peer.remote;
            peer.rx_ready = false;
            deliver(reply_to, data);
            return Ok(Progress::Completed);
        }

        loop {
            match self.ingest_one()? {
                Progress::WouldBlock => return Ok(Progress::WouldBlock),
                Progress::Completed => {
                    // A peer just became ready; deliver it.
                    if let Some(idx) = self.ready.pop() {
                        let peer = self.peers[idx].as_mut().expect("ready peer exists");
                        let data = peer.rx_machine.take_completed();
                        let reply_to = peer.remote;
                        peer.rx_ready = false;
                        deliver(reply_to, data);
                        return Ok(Progress::Completed);
                    }
                    return Ok(Progress::InFlight);
                }
                Progress::InFlight | Progress::WaitingForFlowControl => continue,
            }
        }
    }

    /// Blocking receive until a full payload arrives or timeout.
    pub fn recv(
        &mut self,
        timeout: Duration,
        deliver: &mut dyn FnMut(u8, &[u8]),
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

    /// Non-blocking send step to a destination address.
    pub fn poll_send_to(
        &mut self,
        remote: u8,
        payload: &[u8],
        now: C::Instant,
    ) -> Result<Progress, IsoTpError<Tx::Error>> {
        // Ingest once to pick up any FlowControl frames (and buffer any completed RX payloads).
        // When sending a continuous block (BS=0, STmin=0) we can skip the per-frame ingest to
        // avoid an extra recv syscall on every poll step.
        let skip_ingest = self
            .find_peer_index(remote)
            .and_then(|idx| self.peers[idx].as_ref())
            .map(|peer| match &peer.tx_state {
                TxState::Sending { session, .. } => {
                    session.block_size == 0 && session.st_min == Duration::from_millis(0)
                }
                _ => false,
            })
            .unwrap_or(false);
        if !skip_ingest {
            let _ = self.ingest_one()?;
        }

        let cfg = self.cfg_for_peer(remote);
        if payload.len() > cfg.max_payload_len {
            return Err(IsoTpError::Overflow);
        }

        let peer_idx = self.peer_index_or_alloc(remote)?;
        let peer = self.peers[peer_idx].as_mut().expect("peer exists");
        let tx = &mut self.tx;
        let clock = &self.clock;

        let state = mem::replace(&mut peer.tx_state, TxState::Idle);
        match state {
            TxState::Idle => Self::start_send_inner(tx, clock, peer, &cfg, payload, now),
            TxState::WaitingForFc { session, deadline } => Self::continue_wait_for_fc_inner(
                tx, clock, peer, &cfg, payload, session, deadline, now,
            ),
            TxState::Sending {
                session,
                st_min_deadline,
            } => Self::continue_send_inner(
                tx,
                clock,
                peer,
                &cfg,
                payload,
                session,
                st_min_deadline,
                now,
            ),
        }
    }

    /// Blocking send to a destination address.
    pub fn send_to(
        &mut self,
        remote: u8,
        payload: &[u8],
        timeout: Duration,
    ) -> Result<(), IsoTpError<Tx::Error>> {
        let start = self.clock.now();
        let deadline = self.clock.add(start, timeout);
        loop {
            let now = self.clock.now();
            if now >= deadline {
                return Err(IsoTpError::Timeout(TimeoutKind::NAs));
            }
            match self.poll_send_to(remote, payload, now)? {
                Progress::Completed => return Ok(()),
                Progress::WaitingForFlowControl | Progress::InFlight | Progress::WouldBlock => {
                    continue;
                }
            }
        }
    }

    /// Send a functional-addressing **single-frame** payload to `functional_target`.
    ///
    /// This is intended for small broadcast/group messages. Payloads that do not fit in a single
    /// ISO-TP frame return [`IsoTpError::Overflow`].
    pub fn send_functional_to(
        &mut self,
        functional_target: u8,
        payload: &[u8],
        timeout: Duration,
    ) -> Result<(), IsoTpError<Tx::Error>> {
        if payload.len() > self.base_cfg.max_single_frame_payload() {
            return Err(IsoTpError::Overflow);
        }
        let id = uds29::encode_func_id(functional_target, self.local_addr);
        let pdu = Pdu::SingleFrame {
            len: payload.len() as u8,
            data: payload,
        };
        let frame = encode_with_prefix_sized(
            id,
            &pdu,
            self.base_cfg.padding,
            self.base_cfg.tx_addr,
            self.base_cfg.frame_len,
        )
        .map_err(|_| IsoTpError::InvalidFrame)?;
        self.tx
            .send_timeout(&frame, timeout)
            .map_err(IsoTpError::LinkError)?;
        Ok(())
    }

    fn start_send_inner(
        tx: &mut Tx,
        clock: &C,
        peer: &mut Peer<'a, C::Instant>,
        cfg: &IsoTpConfig,
        payload: &[u8],
        now: C::Instant,
    ) -> Result<Progress, IsoTpError<Tx::Error>> {
        if payload.len() <= cfg.max_single_frame_payload() {
            let pdu = Pdu::SingleFrame {
                len: payload.len() as u8,
                data: payload,
            };
            let frame =
                encode_with_prefix_sized(cfg.tx_id, &pdu, cfg.padding, cfg.tx_addr, cfg.frame_len)
                    .map_err(|_| IsoTpError::InvalidFrame)?;
            tx.try_send(&frame).map_err(IsoTpError::LinkError)?;
            peer.tx_state = TxState::Idle;
            return Ok(Progress::Completed);
        }

        let mut session = TxSession::new(payload.len(), cfg.block_size, cfg.st_min);
        let len = payload.len();
        let chunk = payload.len().min(cfg.max_first_frame_payload());
        let pdu = Pdu::FirstFrame {
            len: len as u16,
            data: &payload[..chunk],
        };
        let frame =
            encode_with_prefix_sized(cfg.tx_id, &pdu, cfg.padding, cfg.tx_addr, cfg.frame_len)
                .map_err(|_| IsoTpError::InvalidFrame)?;
        tx.try_send(&frame).map_err(IsoTpError::LinkError)?;
        session.offset = chunk;

        let deadline = clock.add(now, cfg.n_bs);
        peer.tx_state = TxState::WaitingForFc { session, deadline };
        Ok(Progress::WaitingForFlowControl)
    }

    fn continue_wait_for_fc_inner(
        tx: &mut Tx,
        clock: &C,
        peer: &mut Peer<'a, C::Instant>,
        cfg: &IsoTpConfig,
        payload: &[u8],
        mut session: TxSession,
        deadline: C::Instant,
        now: C::Instant,
    ) -> Result<Progress, IsoTpError<Tx::Error>> {
        if payload.len() != session.payload_len {
            peer.tx_state = TxState::Idle;
            return Err(IsoTpError::NotIdle);
        }
        if session.wait_count > cfg.wft_max {
            peer.tx_state = TxState::Idle;
            return Err(IsoTpError::Timeout(TimeoutKind::NBs));
        }
        if now >= deadline {
            peer.tx_state = TxState::Idle;
            return Err(IsoTpError::Timeout(TimeoutKind::NBs));
        }

        let fc = match peer.pending_fc.take() {
            Some(fc) => fc,
            None => {
                peer.tx_state = TxState::WaitingForFc { session, deadline };
                return Ok(Progress::WaitingForFlowControl);
            }
        };

        match fc.status {
            FlowStatus::ClearToSend => {
                session.wait_count = 0;
                let bs = if fc.block_size == 0 {
                    cfg.block_size
                } else {
                    fc.block_size
                };
                session.block_size = bs;
                session.block_remaining = bs;
                session.st_min = st_min_to_duration(fc.st_min).unwrap_or(cfg.st_min);
                Self::continue_send_inner(tx, clock, peer, cfg, payload, session, None, now)
            }
            FlowStatus::Wait => {
                session.wait_count = session.wait_count.saturating_add(1);
                if session.wait_count > cfg.wft_max {
                    let deadline = clock.add(now, cfg.n_bs);
                    peer.tx_state = TxState::WaitingForFc { session, deadline };
                    return Err(IsoTpError::Timeout(TimeoutKind::NBs));
                }
                let new_deadline = clock.add(now, cfg.n_bs);
                peer.tx_state = TxState::WaitingForFc {
                    session,
                    deadline: new_deadline,
                };
                Ok(Progress::WaitingForFlowControl)
            }
            FlowStatus::Overflow => {
                peer.tx_state = TxState::Idle;
                Err(IsoTpError::Overflow)
            }
        }
    }

    fn continue_send_inner(
        tx: &mut Tx,
        clock: &C,
        peer: &mut Peer<'a, C::Instant>,
        cfg: &IsoTpConfig,
        payload: &[u8],
        mut session: TxSession,
        st_min_deadline: Option<C::Instant>,
        now: C::Instant,
    ) -> Result<Progress, IsoTpError<Tx::Error>> {
        if payload.len() != session.payload_len {
            peer.tx_state = TxState::Idle;
            return Err(IsoTpError::NotIdle);
        }
        if let Some(deadline) = st_min_deadline
            && now < deadline
        {
            peer.tx_state = TxState::Sending {
                session,
                st_min_deadline: Some(deadline),
            };
            return Ok(Progress::WouldBlock);
        }

        if session.offset >= session.payload_len {
            peer.tx_state = TxState::Idle;
            return Ok(Progress::Completed);
        }

        let remaining = session.payload_len - session.offset;
        let chunk = remaining.min(cfg.max_consecutive_frame_payload());
        let data = &payload[session.offset..session.offset + chunk];
        let pdu = Pdu::ConsecutiveFrame {
            sn: session.next_sn & 0x0F,
            data,
        };
        let frame =
            encode_with_prefix_sized(cfg.tx_id, &pdu, cfg.padding, cfg.tx_addr, cfg.frame_len)
                .map_err(|_| IsoTpError::InvalidFrame)?;
        tx.try_send(&frame).map_err(IsoTpError::LinkError)?;

        session.offset += chunk;
        session.next_sn = (session.next_sn + 1) & 0x0F;

        if session.offset >= session.payload_len {
            peer.tx_state = TxState::Idle;
            return Ok(Progress::Completed);
        }

        if session.block_size > 0 {
            session.block_remaining = session.block_remaining.saturating_sub(1);
            if session.block_remaining == 0 {
                session.block_remaining = session.block_size;
                let deadline = clock.add(now, cfg.n_bs);
                peer.tx_state = TxState::WaitingForFc { session, deadline };
                return Ok(Progress::WaitingForFlowControl);
            }
        }

        let next_deadline = if session.st_min > Duration::from_millis(0) {
            Some(clock.add(now, session.st_min))
        } else {
            None
        };
        peer.tx_state = TxState::Sending {
            session,
            st_min_deadline: next_deadline,
        };
        Ok(Progress::InFlight)
    }

    fn send_flow_control_frame(
        tx: &mut Tx,
        cfg: &IsoTpConfig,
        status: FlowStatus,
        block_size: u8,
        st_min: u8,
    ) -> Result<(), IsoTpError<Tx::Error>> {
        let fc = Pdu::FlowControl {
            status,
            block_size,
            st_min,
        };
        let frame =
            encode_with_prefix_sized::<F>(cfg.tx_id, &fc, cfg.padding, cfg.tx_addr, cfg.frame_len)
                .map_err(|_| IsoTpError::InvalidFrame)?;
        tx.try_send(&frame).map_err(IsoTpError::LinkError)?;
        Ok(())
    }

    fn send_overflow_fc_frame(tx: &mut Tx, cfg: &IsoTpConfig) -> Result<(), IsoTpError<Tx::Error>> {
        Self::send_flow_control_frame(
            tx,
            cfg,
            FlowStatus::Overflow,
            0,
            duration_to_st_min(cfg.st_min),
        )
    }
}
