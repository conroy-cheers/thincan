//! Async multi-peer ISO-TP demultiplexing for 29-bit UDS-style addressing.
//!
//! This is the async counterpart to [`crate::demux::IsoTpDemux`]. It supports receiving from
//! multiple sources addressed to a single local target by keeping per-source reassembly state, and
//! it exposes a `reply_to` metadata field (the sender's UDS source address).

#![cfg(feature = "uds")]

use core::time::Duration;

use can_uds::uds29::{self, Uds29Kind};
use embedded_can::Frame;
use embedded_can_interface::{AsyncRxFrameIo, AsyncTxFrameIo};

use crate::RxFlowControl;
use crate::async_io::AsyncRuntime;
use crate::config::IsoTpConfig;
use crate::demux::rx_storages_from_buffers;
use crate::errors::{IsoTpError, TimeoutKind};
use crate::pdu::{
    FlowStatus, Pdu, decode_with_offset, duration_to_st_min, encode_with_prefix_sized,
    st_min_to_duration,
};
use crate::rx::{RxMachine, RxOutcome, RxStorage};
use crate::timer::Clock;

#[cfg(feature = "defmt")]
use defmt::{debug, warn};

#[cfg(not(feature = "defmt"))]
macro_rules! debug {
    ($($t:tt)*) => {};
}
#[cfg(not(feature = "defmt"))]
macro_rules! warn {
    ($($t:tt)*) => {};
}

fn flow_status_str(status: FlowStatus) -> &'static str {
    match status {
        FlowStatus::ClearToSend => "cts",
        FlowStatus::Wait => "wait",
        FlowStatus::Overflow => "overflow",
    }
}

fn timeout_kind_str(kind: TimeoutKind) -> &'static str {
    match kind {
        TimeoutKind::NAs => "N_As",
        TimeoutKind::NAr => "N_Ar",
        TimeoutKind::NBs => "N_Bs",
        TimeoutKind::NBr => "N_Br",
        TimeoutKind::NCs => "N_Cs",
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct PendingFc {
    status: FlowStatus,
    block_size: u8,
    st_min: u8,
}

struct Peer<'a> {
    remote: u8,
    rx_machine: RxMachine<'a>,
    rx_ready: bool,
    pending_fc: Option<PendingFc>,
}

impl<'a> Peer<'a> {
    fn new(remote: u8, storage: RxStorage<'a>) -> Self {
        Self {
            remote,
            rx_machine: RxMachine::new(storage),
            rx_ready: false,
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

/// Async multi-peer ISO-TP node for 29-bit UDS physical IDs `0x18DA_TA_SA`.
///
/// The demux may additionally accept functional addressing (`0x18DB_TA_SA`) if configured via
/// [`IsoTpAsyncDemux::with_functional_addr`]. Functional IDs are treated as **Single Frame only**.
pub struct IsoTpAsyncDemux<'a, Tx, Rx, F, C, const MAX_PEERS: usize>
where
    Tx: AsyncTxFrameIo<Frame = F>,
    Rx: AsyncRxFrameIo<Frame = F, Error = Tx::Error>,
    F: Frame,
    C: Clock,
{
    tx: Tx,
    rx: Rx,
    base_cfg: IsoTpConfig,
    rx_flow_control: RxFlowControl,
    clock: C,
    local_addr: u8,
    functional_addr: Option<u8>,
    peers: [Option<Peer<'a>>; MAX_PEERS],
    free: [Option<RxStorage<'a>>; MAX_PEERS],
    ready: ReadyQueue<MAX_PEERS>,
}

impl<'a, Tx, Rx, F, C, const MAX_PEERS: usize> IsoTpAsyncDemux<'a, Tx, Rx, F, C, MAX_PEERS>
where
    Tx: AsyncTxFrameIo<Frame = F>,
    Rx: AsyncRxFrameIo<Frame = F, Error = Tx::Error>,
    F: Frame,
    C: Clock,
{
    /// Create a demux with caller-provided per-peer RX storages.
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

        let rx_flow_control = RxFlowControl::from_config(&base_cfg);
        Ok(Self {
            tx,
            rx,
            base_cfg,
            rx_flow_control,
            clock,
            local_addr,
            functional_addr: None,
            peers: core::array::from_fn(|_| None),
            free,
            ready: ReadyQueue::new(),
        })
    }

    /// Create a demux with borrowed RX buffers (one per peer).
    pub fn with_borrowed_rx_buffers<const L: usize>(
        tx: Tx,
        rx: Rx,
        base_cfg: IsoTpConfig,
        clock: C,
        local_addr: u8,
        bufs: &'a mut [[u8; L]; MAX_PEERS],
    ) -> Result<Self, IsoTpError<()>> {
        let storages = rx_storages_from_buffers(bufs);
        Self::new(tx, rx, base_cfg, clock, local_addr, storages)
    }

    /// Enable reception of functional-addressing frames (`0x18DB_TA_SA`) for `functional_addr`.
    pub fn with_functional_addr(mut self, functional_addr: u8) -> Self {
        self.functional_addr = Some(functional_addr);
        self
    }

    /// Acceptance filters matching frames addressed to this node.
    pub fn acceptance_filters(&self) -> uds29::AcceptanceFilters {
        uds29::filters_for_targets(self.local_addr, self.functional_addr)
    }

    /// Get the current receive-side FlowControl parameters (BS/STmin).
    pub fn rx_flow_control(&self) -> RxFlowControl {
        self.rx_flow_control
    }

    /// Update receive-side FlowControl parameters (BS/STmin).
    pub fn set_rx_flow_control(&mut self, fc: RxFlowControl) {
        self.rx_flow_control = fc;
    }

    fn cfg_for_peer(&self, remote: u8) -> IsoTpConfig {
        let mut cfg = self.base_cfg.clone();
        cfg.tx_id = uds29::encode_phys_id(remote, self.local_addr);
        cfg.rx_id = uds29::encode_phys_id(self.local_addr, remote);
        cfg
    }

    fn peer_index(&self, remote: u8) -> Option<usize> {
        self.peers
            .iter()
            .position(|p| p.as_ref().is_some_and(|pp| pp.remote == remote))
    }

    fn peer_index_or_alloc(&mut self, remote: u8) -> Result<usize, IsoTpError<Tx::Error>> {
        if let Some(i) = self.peer_index(remote) {
            return Ok(i);
        }
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

    fn deliver_ready(&mut self, deliver: &mut dyn FnMut(u8, &[u8])) -> Option<()> {
        let idx = self.ready.pop()?;
        let peer = self.peers[idx].as_mut().expect("ready peer exists");
        let data = peer.rx_machine.take_completed();
        let reply_to = peer.remote;
        peer.rx_ready = false;
        deliver(reply_to, data);
        Some(())
    }

    async fn pump_rx<R: AsyncRuntime>(
        &mut self,
        rt: &R,
        global_start: C::Instant,
        global_timeout: Duration,
        max_frames: usize,
    ) -> Result<(), IsoTpError<Tx::Error>> {
        for _ in 0..max_frames {
            let _remaining = remaining(global_timeout, self.clock.elapsed(global_start))
                .ok_or(IsoTpError::Timeout(TimeoutKind::NAs))?;
            match self
                .recv_frame_with_timeout(rt, Duration::from_micros(1))
                .await
            {
                Ok(Some(frame)) => {
                    if let Some((remote, status, bs, st)) = self.ingest_frame(&frame)? {
                        let _ = self
                            .send_flow_control(rt, global_start, global_timeout, remote, status, bs, st)
                            .await;
                    }
                }
                Ok(None) => break,
                Err(IsoTpError::LinkError(err)) => {
                    warn!("isotp rx link-error kind=N_Ar");
                    return Err(IsoTpError::LinkError(err));
                }
                Err(err) => return Err(err),
            }
        }
        Ok(())
    }

    async fn wait_with_rx<R: AsyncRuntime>(
        &mut self,
        rt: &R,
        global_start: C::Instant,
        global_timeout: Duration,
        kind: TimeoutKind,
        duration: Duration,
    ) -> Result<(), IsoTpError<Tx::Error>> {
        if duration.is_zero() {
            return Ok(());
        }

        let wait_start = self.clock.now();
        loop {
            let elapsed_wait = self.clock.elapsed(wait_start);
            if elapsed_wait >= duration {
                return Ok(());
            }
            let remaining_wait = duration - elapsed_wait;
            let remaining_global = remaining(global_timeout, self.clock.elapsed(global_start))
                .ok_or(IsoTpError::Timeout(kind))?;
            let global_tighter = remaining_global <= remaining_wait;
            let wait_for = if global_tighter {
                remaining_global
            } else {
                remaining_wait
            };

            match self.recv_frame_with_timeout(rt, wait_for).await {
                Ok(Some(frame)) => {
                    if let Some((remote, status, bs, st)) = self.ingest_frame(&frame)? {
                        let _ = self
                            .send_flow_control(rt, global_start, global_timeout, remote, status, bs, st)
                            .await;
                    }
                }
                Ok(None) => {
                    if global_tighter {
                        warn!("isotp rx timeout kind={}", timeout_kind_str(kind));
                        return Err(IsoTpError::Timeout(kind));
                    }
                    return Ok(());
                }
                Err(IsoTpError::LinkError(err)) => {
                    warn!("isotp rx link-error kind={}", timeout_kind_str(kind));
                    return Err(IsoTpError::LinkError(err));
                }
                Err(err) => return Err(err),
            }
        }
    }

    fn ingest_frame(
        &mut self,
        frame: &F,
    ) -> Result<Option<(u8, FlowStatus, u8, u8)>, IsoTpError<Tx::Error>> {
        let uds = match uds29::decode_id(frame.id()) {
            Some(v) => v,
            None => return Ok(None),
        };
        let (kind, target, source) = (uds.kind, uds.target, uds.source);

        match kind {
            Uds29Kind::Physical => {
                if target != self.local_addr {
                    return Ok(None);
                }
            }
            Uds29Kind::Functional => {
                if Some(target) != self.functional_addr {
                    return Ok(None);
                }
            }
        }

        let peer_idx = self.peer_index_or_alloc(source)?;
        let cfg = self.cfg_for_peer(source);
        let peer = self.peers[peer_idx].as_mut().expect("peer exists");

        if peer.rx_ready {
            // Only accept FC frames while the completed payload is still buffered.
            if let Some(expected) = cfg.rx_addr
                && frame.data().first().copied() != Some(expected)
            {
                return Ok(None);
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
            } else if matches!(pdu, Pdu::FirstFrame { .. }) {
                // The application has not drained the previous completed payload yet; explicitly
                // reject new segmented transfers so the sender doesn't just time out.
                return Ok(Some((
                    source,
                    FlowStatus::Overflow,
                    0,
                    duration_to_st_min(self.rx_flow_control.st_min),
                )));
            }
            return Ok(None);
        }

        if let Some(expected) = cfg.rx_addr
            && frame.data().first().copied() != Some(expected)
        {
            return Ok(None);
        }

        let pdu = decode_with_offset(frame.data(), cfg.rx_pci_offset())
            .map_err(|_| IsoTpError::InvalidFrame)?;
        if let Pdu::FirstFrame { len, .. } = pdu {
            debug!("isotp rx ff remote={} len={}", source, len);
        }

        if kind == Uds29Kind::Functional && !matches!(pdu, Pdu::SingleFrame { .. }) {
            return Ok(None);
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
                Ok(None)
            }
            _ => {
                let outcome = match peer.rx_machine.on_pdu(&cfg, &self.rx_flow_control, pdu) {
                    Ok(o) => o,
                    Err(IsoTpError::Overflow) => {
                        peer.rx_machine.reset();
                        peer.rx_ready = false;
                        return Err(IsoTpError::RxOverflow);
                    }
                    Err(IsoTpError::UnexpectedPdu) => return Ok(None),
                    Err(IsoTpError::BadSequence) => {
                        peer.rx_machine.reset();
                        peer.rx_ready = false;
                        return Err(IsoTpError::BadSequence);
                    }
                    Err(IsoTpError::InvalidFrame) => {
                        peer.rx_machine.reset();
                        peer.rx_ready = false;
                        return Err(IsoTpError::InvalidFrame);
                    }
                    Err(IsoTpError::InvalidConfig) => return Err(IsoTpError::InvalidConfig),
                    Err(IsoTpError::Timeout(kind)) => return Err(IsoTpError::Timeout(kind)),
                    Err(IsoTpError::WouldBlock) => return Err(IsoTpError::WouldBlock),
                    Err(IsoTpError::RxOverflow) => {
                        peer.rx_machine.reset();
                        peer.rx_ready = false;
                        return Err(IsoTpError::RxOverflow);
                    }
                    Err(IsoTpError::NotIdle) => return Err(IsoTpError::NotIdle),
                    Err(IsoTpError::LinkError(_)) => {
                        peer.rx_machine.reset();
                        peer.rx_ready = false;
                        return Err(IsoTpError::InvalidFrame);
                    }
                };

                match outcome {
                    RxOutcome::None => Ok(None),
                    RxOutcome::SendFlowControl {
                        status,
                        block_size,
                        st_min,
                    } => Ok(Some((source, status, block_size, st_min))),
                    RxOutcome::Completed(_len) => {
                        peer.rx_ready = true;
                        self.ready
                            .push(peer_idx)
                            .map_err(|_| IsoTpError::RxOverflow)?;
                        Ok(None)
                    }
                }
            }
        }
    }

    async fn send_flow_control<R: AsyncRuntime>(
        &mut self,
        rt: &R,
        start: C::Instant,
        timeout: Duration,
        remote: u8,
        status: FlowStatus,
        block_size: u8,
        st_min: u8,
    ) -> Result<(), IsoTpError<Tx::Error>> {
        debug!(
            "isotp fc tx remote={} status={} bs={} st_min=0x{:02x}",
            remote,
            flow_status_str(status),
            block_size,
            st_min
        );
        let cfg = self.cfg_for_peer(remote);
        let fc = Pdu::FlowControl {
            status,
            block_size,
            st_min,
        };
        let frame =
            encode_with_prefix_sized(cfg.tx_id, &fc, cfg.padding, cfg.tx_addr, cfg.frame_len)
                .map_err(|_| IsoTpError::InvalidFrame)?;
        self.send_frame(rt, start, timeout, TimeoutKind::NAs, &frame)
            .await
    }

    async fn send_overflow_fc<R: AsyncRuntime>(
        &mut self,
        rt: &R,
        start: C::Instant,
        timeout: Duration,
        remote: u8,
    ) -> Result<(), IsoTpError<Tx::Error>> {
        self.send_flow_control(
            rt,
            start,
            timeout,
            remote,
            FlowStatus::Overflow,
            0,
            duration_to_st_min(self.base_cfg.st_min),
        )
        .await
    }

    async fn send_frame<R: AsyncRuntime>(
        &mut self,
        rt: &R,
        start: C::Instant,
        timeout: Duration,
        kind: TimeoutKind,
        frame: &F,
    ) -> Result<(), IsoTpError<Tx::Error>> {
        let remaining =
            remaining(timeout, self.clock.elapsed(start)).ok_or(IsoTpError::Timeout(kind))?;
        match rt.timeout(remaining, self.tx.send(frame)).await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(err)) => {
                warn!("isotp tx link-error kind={}", timeout_kind_str(kind));
                Err(IsoTpError::LinkError(err))
            }
            Err(_) => {
                warn!("isotp tx timeout kind={}", timeout_kind_str(kind));
                Err(IsoTpError::Timeout(kind))
            }
        }
    }

    async fn recv_frame<R: AsyncRuntime>(
        &mut self,
        rt: &R,
        start: C::Instant,
        timeout: Duration,
        kind: TimeoutKind,
    ) -> Result<F, IsoTpError<Tx::Error>> {
        let remaining =
            remaining(timeout, self.clock.elapsed(start)).ok_or(IsoTpError::Timeout(kind))?;
        match self.recv_frame_with_timeout(rt, remaining).await {
            Ok(Some(frame)) => Ok(frame),
            Ok(None) => {
                if kind != TimeoutKind::NAr {
                    warn!("isotp rx timeout kind={}", timeout_kind_str(kind));
                }
                Err(IsoTpError::Timeout(kind))
            }
            Err(IsoTpError::LinkError(err)) => {
                if kind != TimeoutKind::NAr {
                    warn!("isotp rx link-error kind={}", timeout_kind_str(kind));
                }
                Err(IsoTpError::LinkError(err))
            }
            Err(err) => Err(err),
        }
    }

    async fn wait_for_flow_control<R: AsyncRuntime>(
        &mut self,
        rt: &R,
        global_start: C::Instant,
        global_timeout: Duration,
        remote: u8,
        mut fc_start: C::Instant,
        mut wait_count: u8,
    ) -> Result<(u8, Duration, u8), IsoTpError<Tx::Error>> {
        loop {
            if self.clock.elapsed(fc_start) >= self.base_cfg.n_bs {
                warn!("isotp fc timeout kind=N_Bs remote={}", remote);
                return Err(IsoTpError::Timeout(TimeoutKind::NBs));
            }

            if let Some(peer_idx) = self.peer_index(remote) {
                if let Some(peer) = self.peers[peer_idx].as_mut()
                    && let Some(fc) = peer.pending_fc.take()
                {
                    debug!(
                        "isotp fc rx remote={} status={} bs={} st_min=0x{:02x}",
                        remote,
                        flow_status_str(fc.status),
                        fc.block_size,
                        fc.st_min
                    );
                    match fc.status {
                        FlowStatus::ClearToSend => {
                            let cfg = self.cfg_for_peer(remote);
                            let bs = if fc.block_size == 0 {
                                cfg.block_size
                            } else {
                                fc.block_size
                            };
                            let st_min = st_min_to_duration(fc.st_min).unwrap_or(cfg.st_min);
                            return Ok((bs, st_min, 0));
                        }
                        FlowStatus::Wait => {
                            wait_count = wait_count.saturating_add(1);
                            if wait_count > self.base_cfg.wft_max {
                                warn!(
                                    "isotp fc wait overflow remote={} count={}",
                                    remote,
                                    wait_count
                                );
                                return Err(IsoTpError::Timeout(TimeoutKind::NBs));
                            }
                            fc_start = self.clock.now();
                            continue;
                        }
                        FlowStatus::Overflow => {
                            warn!("isotp fc overflow remote={}", remote);
                            return Err(IsoTpError::Overflow);
                        }
                    }
                }
            }

            let fc_remaining = self.base_cfg.n_bs - self.clock.elapsed(fc_start);
            let global_remaining = remaining(global_timeout, self.clock.elapsed(global_start))
                .ok_or(IsoTpError::Timeout(TimeoutKind::NAs))?;
            let wait_for = fc_remaining.min(global_remaining);

            let frame = match self.recv_frame_with_timeout(rt, wait_for).await {
                Ok(Some(frame)) => frame,
                Ok(None) => {
                    if global_remaining <= fc_remaining {
                        warn!("isotp fc timeout kind=N_As remote={}", remote);
                        return Err(IsoTpError::Timeout(TimeoutKind::NAs));
                    }
                    warn!("isotp fc timeout kind=N_Bs remote={}", remote);
                    return Err(IsoTpError::Timeout(TimeoutKind::NBs));
                }
                Err(IsoTpError::LinkError(err)) => {
                    warn!("isotp fc rx link-error remote={}", remote);
                    return Err(IsoTpError::LinkError(err));
                }
                Err(err) => return Err(err),
            };

            if let Some((remote, status, bs, st)) = self.ingest_frame(&frame)? {
                let _ = self
                    .send_flow_control(rt, global_start, global_timeout, remote, status, bs, st)
                    .await;
            }
        }
    }

    async fn recv_frame_with_timeout<R: AsyncRuntime>(
        &mut self,
        rt: &R,
        wait_for: Duration,
    ) -> Result<Option<F>, IsoTpError<Tx::Error>> {
        match rt.timeout(wait_for, self.rx.recv()).await {
            Ok(Ok(frame)) => Ok(Some(frame)),
            Ok(Err(err)) => Err(IsoTpError::LinkError(err)),
            Err(_) => Ok(None),
        }
    }

    /// Send an ISO-TP payload to a specific peer address (physical addressing).
    pub async fn send_to<R: AsyncRuntime>(
        &mut self,
        rt: &R,
        remote: u8,
        payload: &[u8],
        timeout: Duration,
    ) -> Result<(), IsoTpError<Tx::Error>> {
        let cfg = self.cfg_for_peer(remote);
        if payload.len() > cfg.max_payload_len {
            return Err(IsoTpError::Overflow);
        }

        // Ensure the peer exists so we have a slot for FC frames.
        let _ = self.peer_index_or_alloc(remote)?;

        let start = self.clock.now();
        if self.clock.elapsed(start) >= timeout {
            return Err(IsoTpError::Timeout(TimeoutKind::NAs));
        }

        if payload.len() <= cfg.max_single_frame_payload() {
            let pdu = Pdu::SingleFrame {
                len: payload.len() as u8,
                data: payload,
            };
            let frame =
                encode_with_prefix_sized(cfg.tx_id, &pdu, cfg.padding, cfg.tx_addr, cfg.frame_len)
                    .map_err(|_| IsoTpError::InvalidFrame)?;
            self.send_frame(rt, start, timeout, TimeoutKind::NAs, &frame)
                .await?;
            return Ok(());
        }

        let mut offset = payload.len().min(cfg.max_first_frame_payload());
        let mut next_sn: u8 = 1;
        let pdu = Pdu::FirstFrame {
            len: payload.len() as u16,
            data: &payload[..offset],
        };
        let frame =
            encode_with_prefix_sized(cfg.tx_id, &pdu, cfg.padding, cfg.tx_addr, cfg.frame_len)
                .map_err(|_| IsoTpError::InvalidFrame)?;
        debug!("isotp tx ff start remote={} len={}", remote, payload.len());
        self.send_frame(rt, start, timeout, TimeoutKind::NAs, &frame)
            .await?;
        debug!("isotp tx ff remote={} len={}", remote, payload.len());

        let fc_start = self.clock.now();
        let (mut block_size, mut st_min, mut wait_count) = self
            .wait_for_flow_control(rt, start, timeout, remote, fc_start, 0)
            .await?;
        debug!(
            "isotp tx fc remote={} bs={} st_min_us={}",
            remote,
            block_size,
            st_min.as_micros().min(u128::from(u64::MAX)) as u64
        );
        let mut block_remaining = block_size;

        let mut last_cf_sent: Option<C::Instant> = None;
        while offset < payload.len() {
            // Opportunistically drain pending RX frames so FC responses don't stall.
            // If `st_min` enforces pacing, `wait_with_rx` will service RX during that delay.
            if st_min.is_zero() {
                self.pump_rx(rt, start, timeout, 4).await?;
            }

            if block_size > 0 && block_remaining == 0 {
                let fc_start = self.clock.now();
                let (new_bs, new_st_min, new_wait_count) = self
                    .wait_for_flow_control(rt, start, timeout, remote, fc_start, wait_count)
                    .await?;
                block_size = new_bs;
                block_remaining = new_bs;
                st_min = new_st_min;
                wait_count = new_wait_count;
                continue;
            }

            if let Some(sent_at) = last_cf_sent {
                let elapsed = self.clock.elapsed(sent_at);
                if elapsed < st_min {
                    let wait_for = st_min - elapsed;
                    self.wait_with_rx(
                        rt,
                        start,
                        timeout,
                        TimeoutKind::NAs,
                        wait_for,
                    )
                    .await?;
                }
            }

            let remaining_len = payload.len() - offset;
            let chunk = remaining_len.min(cfg.max_consecutive_frame_payload());
            let pdu = Pdu::ConsecutiveFrame {
                sn: next_sn & 0x0F,
                data: &payload[offset..offset + chunk],
            };
            let frame =
                encode_with_prefix_sized(cfg.tx_id, &pdu, cfg.padding, cfg.tx_addr, cfg.frame_len)
                    .map_err(|_| IsoTpError::InvalidFrame)?;
            self.send_frame(rt, start, timeout, TimeoutKind::NAs, &frame)
                .await?;

            // Drain pending frames after each CF only when block-sized pacing is active to
            // avoid throttling sustained transfers with redundant RX polls.
            if block_size > 0 {
                self.pump_rx(rt, start, timeout, 2).await?;
            }

            last_cf_sent = Some(self.clock.now());
            offset += chunk;
            next_sn = (next_sn + 1) & 0x0F;

            if block_size > 0 {
                block_remaining = block_remaining.saturating_sub(1);
            }
        }

        Ok(())
    }

    /// Send a functional-addressing **single-frame** payload to `functional_target`.
    ///
    /// This is intended for small broadcast/group messages. Payloads that do not fit in a single
    /// ISO-TP frame return [`IsoTpError::Overflow`].
    pub async fn send_functional_to<R: AsyncRuntime>(
        &mut self,
        rt: &R,
        functional_target: u8,
        payload: &[u8],
        timeout: Duration,
    ) -> Result<(), IsoTpError<Tx::Error>> {
        if payload.len() > self.base_cfg.max_single_frame_payload() {
            return Err(IsoTpError::Overflow);
        }
        let start = self.clock.now();
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
        self.send_frame(rt, start, timeout, TimeoutKind::NAs, &frame)
            .await
    }

    /// Receive until a full payload arrives or timeout.
    pub async fn recv<R: AsyncRuntime>(
        &mut self,
        rt: &R,
        timeout: Duration,
        deliver: &mut dyn FnMut(u8, &[u8]),
    ) -> Result<(), IsoTpError<Tx::Error>> {
        let start = self.clock.now();
        loop {
            if self.deliver_ready(deliver).is_some() {
                return Ok(());
            }

            let frame = self
                .recv_frame(rt, start, timeout, TimeoutKind::NAr)
                .await?;
            match self.ingest_frame(&frame) {
                Ok(Some((remote, status, bs, st))) => {
                    self.send_flow_control(rt, start, timeout, remote, status, bs, st)
                        .await?;
                }
                Ok(None) => continue,
                Err(IsoTpError::RxOverflow) => {
                    // Best effort overflow FC to the physical sender.
                    if let Some(uds) = uds29::decode_id(frame.id())
                        && uds.kind == Uds29Kind::Physical
                        && uds.target == self.local_addr
                    {
                        let _ = self.send_overflow_fc(rt, start, timeout, uds.source).await;
                    }
                    return Err(IsoTpError::RxOverflow);
                }
                Err(e) => return Err(e),
            }
        }
    }
}

fn remaining(timeout: Duration, elapsed: Duration) -> Option<Duration> {
    timeout.checked_sub(elapsed)
}

async fn sleep_or_timeout<C: Clock, R: AsyncRuntime, E>(
    clock: &C,
    rt: &R,
    start: C::Instant,
    timeout: Duration,
    kind: TimeoutKind,
    duration: Duration,
) -> Result<(), IsoTpError<E>> {
    let remaining = remaining(timeout, clock.elapsed(start)).ok_or(IsoTpError::Timeout(kind))?;
    let wait_for = duration.min(remaining);
    let sleep_fut = rt.sleep(wait_for);
    match rt.timeout(wait_for, sleep_fut).await {
        Ok(()) => Ok(()),
        Err(_) => Err(IsoTpError::Timeout(kind)),
    }
}

#[cfg(feature = "std")]
impl<'a, Tx, Rx, F, const MAX_PEERS: usize>
    IsoTpAsyncDemux<'a, Tx, Rx, F, crate::StdClock, MAX_PEERS>
where
    Tx: AsyncTxFrameIo<Frame = F>,
    Rx: AsyncRxFrameIo<Frame = F, Error = Tx::Error>,
    F: Frame,
{
    /// Convenience constructor using `StdClock`.
    pub fn with_std_clock(
        tx: Tx,
        rx: Rx,
        base_cfg: IsoTpConfig,
        local_addr: u8,
        storages: [RxStorage<'a>; MAX_PEERS],
    ) -> Result<Self, IsoTpError<()>> {
        Self::new(tx, rx, base_cfg, crate::StdClock, local_addr, storages)
    }
}
