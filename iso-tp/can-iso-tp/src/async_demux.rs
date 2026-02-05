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

use crate::async_io::AsyncRuntime;
use crate::config::IsoTpConfig;
use crate::errors::{IsoTpError, TimeoutKind};
use crate::pdu::{
    FlowStatus, Pdu, decode_with_offset, duration_to_st_min, encode_with_prefix_sized,
    st_min_to_duration,
};
use crate::rx::{RxMachine, RxOutcome, RxStorage};
use crate::timer::Clock;

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
    pub fn with_functional_addr(mut self, functional_addr: u8) -> Self {
        self.functional_addr = Some(functional_addr);
        self
    }

    /// Acceptance filters matching frames addressed to this node.
    pub fn acceptance_filters(&self) -> uds29::AcceptanceFilters {
        uds29::filters_for_targets(self.local_addr, self.functional_addr)
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
                let outcome = match peer.rx_machine.on_pdu(&cfg, pdu) {
                    Ok(o) => o,
                    Err(IsoTpError::Overflow) => return Err(IsoTpError::RxOverflow),
                    Err(IsoTpError::UnexpectedPdu) => return Ok(None),
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
            Ok(Err(err)) => Err(IsoTpError::LinkError(err)),
            Err(_) => Err(IsoTpError::Timeout(kind)),
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
        match rt.timeout(remaining, self.rx.recv()).await {
            Ok(Ok(frame)) => Ok(frame),
            Ok(Err(err)) => Err(IsoTpError::LinkError(err)),
            Err(_) => Err(IsoTpError::Timeout(kind)),
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
                return Err(IsoTpError::Timeout(TimeoutKind::NBs));
            }

            if let Some(peer_idx) = self.peer_index(remote) {
                if let Some(peer) = self.peers[peer_idx].as_mut()
                    && let Some(fc) = peer.pending_fc.take()
                {
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
                                return Err(IsoTpError::Timeout(TimeoutKind::NBs));
                            }
                            fc_start = self.clock.now();
                            continue;
                        }
                        FlowStatus::Overflow => return Err(IsoTpError::Overflow),
                    }
                }
            }

            let fc_remaining = self.base_cfg.n_bs - self.clock.elapsed(fc_start);
            let global_remaining = remaining(global_timeout, self.clock.elapsed(global_start))
                .ok_or(IsoTpError::Timeout(TimeoutKind::NAs))?;
            let wait_for = fc_remaining.min(global_remaining);

            let frame = match rt.timeout(wait_for, self.rx.recv()).await {
                Ok(Ok(f)) => f,
                Ok(Err(err)) => return Err(IsoTpError::LinkError(err)),
                Err(_) => {
                    if global_remaining <= fc_remaining {
                        return Err(IsoTpError::Timeout(TimeoutKind::NAs));
                    }
                    return Err(IsoTpError::Timeout(TimeoutKind::NBs));
                }
            };

            if let Some((remote, status, bs, st)) = self.ingest_frame(&frame)? {
                let _ = self
                    .send_flow_control(rt, global_start, global_timeout, remote, status, bs, st)
                    .await;
            }
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
        self.send_frame(rt, start, timeout, TimeoutKind::NAs, &frame)
            .await?;

        let fc_start = self.clock.now();
        let (mut block_size, mut st_min, mut wait_count) = self
            .wait_for_flow_control(rt, start, timeout, remote, fc_start, 0)
            .await?;
        let mut block_remaining = block_size;

        let mut last_cf_sent: Option<C::Instant> = None;
        while offset < payload.len() {
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
                    sleep_or_timeout(&self.clock, rt, start, timeout, TimeoutKind::NAs, wait_for)
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
