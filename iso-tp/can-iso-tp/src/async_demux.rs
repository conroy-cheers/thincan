//! Async multi-peer ISO-TP demultiplexing for 29-bit UDS-style addressing.

#![cfg(feature = "uds")]

use core::time::Duration;

use can_uds::uds29::{self, Uds29Kind};
use embedded_can::Frame;
use embedded_can_interface::{AsyncRxFrameIo, AsyncTxFrameIo};

use crate::AsyncRuntime;
use crate::RxFlowControl;
use crate::config::IsoTpConfig;
use crate::errors::IsoTpError;
use crate::errors::TimeoutKind;
use crate::pdu::{
    FlowStatus, Pdu, decode_with_offset, duration_to_st_min, encode_with_prefix_sized,
    st_min_to_duration,
};
use crate::rx::{RxMachine, RxOutcome, RxState, RxStorage};
use crate::timer::Clock;
use crate::tx::Progress;

/// Receive-into error for the async demux app view.
#[derive(Debug)]
pub enum AppRecvIntoError<E> {
    /// Output buffer was too small for the completed payload.
    BufferTooSmall { needed: usize, got: usize },
    /// ISO-TP transport/backend error.
    IsoTp(IsoTpError<E>),
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct PendingFc {
    status: FlowStatus,
    block_size: u8,
    st_min: u8,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ReadyItem {
    Peer(usize),
    Completed(usize),
}

struct CompletedPayload<'a> {
    remote: u8,
    len: usize,
    storage: RxStorage<'a>,
}

struct Peer<'a, I> {
    remote: u8,
    rx_machine: RxMachine<'a>,
    rx_ready: bool,
    pending_fc: Option<PendingFc>,
    rx_last_activity: Option<I>,
}

impl<'a, I> Peer<'a, I> {
    fn new(remote: u8, storage: RxStorage<'a>) -> Self {
        Self {
            remote,
            rx_machine: RxMachine::new(storage),
            rx_ready: false,
            pending_fc: None,
            rx_last_activity: None,
        }
    }
}

struct ReadyQueue<const N: usize> {
    buf: [ReadyItem; N],
    head: usize,
    tail: usize,
    len: usize,
}

impl<const N: usize> ReadyQueue<N> {
    fn new() -> Self {
        Self {
            buf: [ReadyItem::Peer(0); N],
            head: 0,
            tail: 0,
            len: 0,
        }
    }

    fn push(&mut self, v: ReadyItem) -> Result<(), ()> {
        if self.len == N {
            return Err(());
        }
        self.buf[self.tail] = v;
        self.tail = (self.tail + 1) % N;
        self.len += 1;
        Ok(())
    }

    fn pop(&mut self) -> Option<ReadyItem> {
        if self.len == 0 {
            return None;
        }
        let v = self.buf[self.head];
        self.head = (self.head + 1) % N;
        self.len -= 1;
        Some(v)
    }

    fn is_empty(&self) -> bool {
        self.len == 0
    }
}

/// Async multi-peer ISO-TP demux for 29-bit UDS physical IDs (`0x18DA_TA_SA`).
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
    peers: [Option<Peer<'a, C::Instant>>; MAX_PEERS],
    free: [Option<RxStorage<'a>>; MAX_PEERS],
    completed: [Option<CompletedPayload<'a>>; MAX_PEERS],
    ready: ReadyQueue<MAX_PEERS>,
}

/// Runtime-facing view for protocol pump work.
pub struct IsoTpAsyncDemuxDriver<'d, 'a, Tx, Rx, F, C, const MAX_PEERS: usize>
where
    Tx: AsyncTxFrameIo<Frame = F>,
    Rx: AsyncRxFrameIo<Frame = F, Error = Tx::Error>,
    F: Frame,
    C: Clock,
{
    demux: &'d mut IsoTpAsyncDemux<'a, Tx, Rx, F, C, MAX_PEERS>,
}

/// Application-facing view for addressed async send/receive operations.
pub struct IsoTpAsyncDemuxApp<'d, 'a, Tx, Rx, F, C, const MAX_PEERS: usize>
where
    Tx: AsyncTxFrameIo<Frame = F>,
    Rx: AsyncRxFrameIo<Frame = F, Error = Tx::Error>,
    F: Frame,
    C: Clock,
{
    demux: &'d mut IsoTpAsyncDemux<'a, Tx, Rx, F, C, MAX_PEERS>,
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
        functional_addr: Option<u8>,
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
            rx_flow_control: RxFlowControl::from_config(&base_cfg),
            base_cfg,
            clock,
            local_addr,
            functional_addr,
            peers: core::array::from_fn(|_| None),
            free,
            completed: core::array::from_fn(|_| None),
            ready: ReadyQueue::new(),
        })
    }

    /// Borrow the application-facing view.
    pub fn app<'d>(&'d mut self) -> IsoTpAsyncDemuxApp<'d, 'a, Tx, Rx, F, C, MAX_PEERS> {
        IsoTpAsyncDemuxApp { demux: self }
    }

    /// Borrow the runtime-facing driver view.
    pub fn driver<'d>(&'d mut self) -> IsoTpAsyncDemuxDriver<'d, 'a, Tx, Rx, F, C, MAX_PEERS> {
        IsoTpAsyncDemuxDriver { demux: self }
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
            isotp_warn!("async demux alloc_peer invalid remote=local {}", remote);
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
        isotp_debug!("async demux peer allocated remote={} idx={}", remote, idx);
        Ok(idx)
    }

    fn peer_index_or_alloc(&mut self, remote: u8) -> Result<usize, IsoTpError<Tx::Error>> {
        match self.find_peer_index(remote) {
            Some(i) => Ok(i),
            None => self.alloc_peer(remote),
        }
    }

    fn refresh_peer_rx_timeout(peer: &mut Peer<'a, C::Instant>, now: C::Instant) {
        peer.rx_last_activity = if peer.rx_machine.state == RxState::Receiving {
            Some(now)
        } else {
            None
        };
    }

    fn expire_peer_rx_timeouts(&mut self) {
        for peer in &mut self.peers {
            let Some(peer) = peer.as_mut() else {
                continue;
            };
            let Some(last_activity) = peer.rx_last_activity else {
                continue;
            };
            if peer.rx_machine.state != RxState::Receiving {
                peer.rx_last_activity = None;
                continue;
            }
            if self.clock.elapsed(last_activity) >= self.base_cfg.n_br {
                isotp_warn!(
                    "async demux n_br timeout; aborting in-flight rx remote={}",
                    peer.remote
                );
                peer.rx_machine.reset();
                peer.rx_last_activity = None;
            }
        }
    }

    fn next_rx_timeout_wait(&self) -> Option<Duration> {
        let mut min_wait: Option<Duration> = None;
        for peer in &self.peers {
            let Some(peer) = peer.as_ref() else {
                continue;
            };
            if peer.rx_machine.state != RxState::Receiving {
                continue;
            }
            let Some(last_activity) = peer.rx_last_activity else {
                continue;
            };
            let elapsed = self.clock.elapsed(last_activity);
            let remaining = self
                .base_cfg
                .n_br
                .checked_sub(elapsed)
                .unwrap_or(Duration::from_millis(0));
            min_wait = Some(match min_wait {
                Some(current) => current.min(remaining),
                None => remaining,
            });
        }
        min_wait
    }

    fn take_pending_fc(&mut self, peer_idx: usize) -> Option<PendingFc> {
        self.peers[peer_idx]
            .as_mut()
            .and_then(|p| p.pending_fc.take())
    }

    fn try_spill_completed_from_peer(
        &mut self,
        peer_idx: usize,
    ) -> Result<bool, IsoTpError<Tx::Error>> {
        let free_idx = match self.free.iter().position(|s| s.is_some()) {
            Some(i) => i,
            None => return Ok(false),
        };
        let completed_idx = match self.completed.iter().position(|s| s.is_none()) {
            Some(i) => i,
            None => return Ok(false),
        };

        let replacement = self.free[free_idx].take().ok_or(IsoTpError::RxOverflow)?;
        let peer = self.peers[peer_idx].as_mut().expect("peer exists");
        if !peer.rx_ready {
            self.free[free_idx] = Some(replacement);
            return Ok(false);
        }

        let len = peer.rx_machine.completed_len();
        let remote = peer.remote;
        let storage = peer.rx_machine.replace_storage(replacement);
        peer.rx_machine.reset();
        peer.rx_ready = false;
        peer.rx_last_activity = None;
        isotp_trace!(
            "async demux spill completed remote={} len={} peer_idx={}",
            remote,
            len,
            peer_idx
        );
        self.completed[completed_idx] = Some(CompletedPayload {
            remote,
            len,
            storage,
        });
        self.ready
            .push(ReadyItem::Completed(completed_idx))
            .map_err(|_| IsoTpError::RxOverflow)?;
        Ok(true)
    }

    fn recycle_storage(&mut self, storage: RxStorage<'a>) -> Result<(), IsoTpError<Tx::Error>> {
        let free_idx = self
            .free
            .iter()
            .position(|s| s.is_none())
            .ok_or(IsoTpError::RxOverflow)?;
        self.free[free_idx] = Some(storage);
        Ok(())
    }

    fn deliver_ready_into(
        &mut self,
        out: &mut [u8],
    ) -> Result<Option<(u8, usize)>, AppRecvIntoError<Tx::Error>> {
        let Some(item) = self.ready.pop() else {
            return Ok(None);
        };

        match item {
            ReadyItem::Peer(idx) => {
                let peer = self.peers[idx].as_mut().expect("ready peer exists");
                let data = peer.rx_machine.take_completed();
                if data.len() > out.len() {
                    // Keep payload queued for a later call with a larger buffer.
                    let _ = self.ready.push(item);
                    return Err(AppRecvIntoError::BufferTooSmall {
                        needed: data.len(),
                        got: out.len(),
                    });
                }
                out[..data.len()].copy_from_slice(data);
                peer.rx_ready = false;
                peer.rx_last_activity = None;
                isotp_debug!(
                    "async demux deliver peer remote={} len={}",
                    peer.remote,
                    data.len()
                );
                Ok(Some((peer.remote, data.len())))
            }
            ReadyItem::Completed(idx) => {
                let completed = self.completed[idx].as_ref().expect("completed slot exists");
                if completed.len > out.len() {
                    let _ = self.ready.push(item);
                    return Err(AppRecvIntoError::BufferTooSmall {
                        needed: completed.len,
                        got: out.len(),
                    });
                }

                out[..completed.len].copy_from_slice(&completed.storage.as_ref()[..completed.len]);
                let remote = completed.remote;
                let len = completed.len;
                let completed = self.completed[idx].take().expect("completed slot exists");
                self.recycle_storage(completed.storage)
                    .map_err(AppRecvIntoError::IsoTp)?;
                isotp_debug!("async demux deliver spilled remote={} len={}", remote, len);
                Ok(Some((remote, len)))
            }
        }
    }

    async fn send_flow_control_frame(
        &mut self,
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
        self.tx.send(&frame).await.map_err(IsoTpError::LinkError)?;
        Ok(())
    }

    async fn ingest_frame(&mut self, frame: F) -> Result<Progress, IsoTpError<Tx::Error>> {
        let now = self.clock.now();
        self.expire_peer_rx_timeouts();

        let uds = match uds29::decode_id(frame.id()) {
            Some(v) => v,
            None => return Ok(Progress::InFlight),
        };
        let (kind, target, source) = (uds.kind, uds.target, uds.source);
        isotp_trace!(
            "async demux ingest kind={} target={} source={}",
            uds_kind_code(kind),
            target,
            source
        );

        match kind {
            Uds29Kind::Physical => {
                if target != self.local_addr {
                    isotp_trace!(
                        "async demux drop physical target mismatch local={}",
                        self.local_addr
                    );
                    return Ok(Progress::InFlight);
                }
            }
            Uds29Kind::Functional => {
                if Some(target) != self.functional_addr {
                    isotp_trace!("async demux drop functional target mismatch");
                    return Ok(Progress::InFlight);
                }
            }
        }

        let peer_idx = self.peer_index_or_alloc(source)?;
        let cfg = self.cfg_for_peer(source);
        let mut fc_to_send: Option<(FlowStatus, u8, u8)> = None;
        let mut final_result: Result<Progress, IsoTpError<Tx::Error>> = Ok(Progress::InFlight);
        let mut completed_ready = false;

        {
            let peer = self.peers[peer_idx].as_mut().expect("peer exists");

            if peer.rx_ready {
                if let Some(expected) = cfg.rx_addr
                    && frame.data().first().copied() != Some(expected)
                {
                    return Ok(Progress::InFlight);
                }
                let pdu = decode_with_offset(frame.data(), cfg.rx_pci_offset()).map_err(|_| {
                    isotp_warn!("async demux rx_ready invalid frame decode");
                    IsoTpError::InvalidFrame
                })?;
                if let Pdu::FlowControl {
                    status,
                    block_size,
                    st_min,
                } = pdu
                {
                    isotp_trace!(
                        "async demux store pending fc remote={} status={} bs={} st_min_raw={}",
                        source,
                        flow_status_code(status),
                        block_size,
                        st_min
                    );
                    peer.pending_fc = Some(PendingFc {
                        status,
                        block_size,
                        st_min,
                    });
                } else if matches!(pdu, Pdu::FirstFrame { .. }) {
                    fc_to_send = Some((
                        FlowStatus::Overflow,
                        0,
                        duration_to_st_min(self.rx_flow_control.st_min),
                    ));
                }
            } else {
                if let Some(expected) = cfg.rx_addr
                    && frame.data().first().copied() != Some(expected)
                {
                    return Ok(Progress::InFlight);
                }

                let pdu = decode_with_offset(frame.data(), cfg.rx_pci_offset()).map_err(|_| {
                    isotp_warn!("async demux invalid frame decode remote={}", source);
                    IsoTpError::InvalidFrame
                })?;

                if kind == Uds29Kind::Functional && !matches!(pdu, Pdu::SingleFrame { .. }) {
                    return Ok(Progress::InFlight);
                }

                match pdu {
                    Pdu::FlowControl {
                        status,
                        block_size,
                        st_min,
                    } => {
                        isotp_trace!(
                            "async demux got fc remote={} status={} bs={} st_min_raw={}",
                            source,
                            flow_status_code(status),
                            block_size,
                            st_min
                        );
                        peer.pending_fc = Some(PendingFc {
                            status,
                            block_size,
                            st_min,
                        });
                        final_result = Ok(Progress::InFlight);
                    }
                    _ => {
                        let restart_on_ff = matches!(pdu, Pdu::FirstFrame { .. })
                            && peer.rx_machine.state == RxState::Receiving;
                        let outcome = match peer.rx_machine.on_pdu(&cfg, &self.rx_flow_control, pdu)
                        {
                            Ok(o) => o,
                            Err(IsoTpError::Overflow) => {
                                isotp_warn!("async demux rx overflow remote={}", source);
                                peer.rx_machine.reset();
                                peer.rx_last_activity = None;
                                fc_to_send = Some((
                                    FlowStatus::Overflow,
                                    0,
                                    duration_to_st_min(self.rx_flow_control.st_min),
                                ));
                                final_result = Err(IsoTpError::RxOverflow);
                                // Delay return until after FC send.
                                RxOutcome::None
                            }
                            Err(IsoTpError::UnexpectedPdu) => {
                                Self::refresh_peer_rx_timeout(peer, now);
                                return Ok(Progress::InFlight);
                            }
                            Err(IsoTpError::BadSequence) => {
                                isotp_warn!("async demux bad sequence remote={}", source);
                                peer.rx_machine.reset();
                                peer.rx_last_activity = None;
                                return Err(IsoTpError::BadSequence);
                            }
                            Err(IsoTpError::InvalidFrame) => return Err(IsoTpError::InvalidFrame),
                            Err(IsoTpError::InvalidConfig) => {
                                return Err(IsoTpError::InvalidConfig);
                            }
                            Err(IsoTpError::Timeout(kind)) => {
                                return Err(IsoTpError::Timeout(kind));
                            }
                            Err(IsoTpError::WouldBlock) => return Err(IsoTpError::WouldBlock),
                            Err(IsoTpError::RxOverflow) => return Err(IsoTpError::RxOverflow),
                            Err(IsoTpError::NotIdle) => return Err(IsoTpError::NotIdle),
                            Err(IsoTpError::LinkError(_)) => return Err(IsoTpError::InvalidFrame),
                        };

                        if restart_on_ff {
                            isotp_warn!("async demux rx restart on ff remote={}", source);
                        }
                        Self::refresh_peer_rx_timeout(peer, now);

                        match outcome {
                            RxOutcome::None => {}
                            RxOutcome::SendFlowControl {
                                status,
                                block_size,
                                st_min,
                            } => {
                                isotp_trace!(
                                    "async demux send fc remote={} status={} bs={} st_min_raw={}",
                                    source,
                                    flow_status_code(status),
                                    block_size,
                                    st_min
                                );
                                fc_to_send = Some((status, block_size, st_min));
                            }
                            RxOutcome::Completed(_len) => {
                                peer.rx_ready = true;
                                peer.rx_last_activity = None;
                                isotp_debug!(
                                    "async demux rx complete remote={} len={}",
                                    source,
                                    peer.rx_machine.completed_len()
                                );
                                completed_ready = true;
                                final_result = Ok(Progress::Completed);
                            }
                        }
                    }
                }
            }
        }

        if completed_ready {
            if !self.try_spill_completed_from_peer(peer_idx)? {
                self.ready
                    .push(ReadyItem::Peer(peer_idx))
                    .map_err(|_| IsoTpError::RxOverflow)?;
            }
        }

        if let Some((status, block_size, st_min)) = fc_to_send {
            isotp_trace!(
                "async demux tx fc status={} bs={} st_min_raw={}",
                flow_status_code(status),
                block_size,
                st_min
            );
            self.send_flow_control_frame(&cfg, status, block_size, st_min)
                .await?;
        }

        final_result
    }

    async fn send_frame_with_global_timeout<R: AsyncRuntime>(
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

    async fn wait_for_flow_control<R: AsyncRuntime>(
        &mut self,
        rt: &R,
        peer_idx: usize,
        cfg: &IsoTpConfig,
        global_start: C::Instant,
        global_timeout: Duration,
        mut fc_start: C::Instant,
        mut wait_count: u8,
    ) -> Result<(u8, Duration, u8), IsoTpError<Tx::Error>> {
        loop {
            self.expire_peer_rx_timeouts();

            if self.clock.elapsed(fc_start) >= cfg.n_bs {
                return Err(IsoTpError::Timeout(TimeoutKind::NBs));
            }

            let fc_remaining = cfg.n_bs - self.clock.elapsed(fc_start);
            let global_remaining = remaining(global_timeout, self.clock.elapsed(global_start))
                .ok_or(IsoTpError::Timeout(TimeoutKind::NAs))?;
            let mut wait_for = fc_remaining.min(global_remaining);
            if let Some(rx_wait) = self.next_rx_timeout_wait() {
                wait_for = wait_for.min(rx_wait);
            }
            if wait_for == Duration::from_millis(0) {
                self.expire_peer_rx_timeouts();
                continue;
            }

            let frame = match rt.timeout(wait_for, self.rx.recv()).await {
                Ok(Ok(f)) => f,
                Ok(Err(err)) => return Err(IsoTpError::LinkError(err)),
                Err(_) => {
                    self.expire_peer_rx_timeouts();
                    if remaining(global_timeout, self.clock.elapsed(global_start)).is_none() {
                        return Err(IsoTpError::Timeout(TimeoutKind::NAs));
                    }
                    if self.clock.elapsed(fc_start) >= cfg.n_bs {
                        return Err(IsoTpError::Timeout(TimeoutKind::NBs));
                    }
                    continue;
                }
            };

            let _ = self.ingest_frame(frame).await?;
            let Some(fc) = self.take_pending_fc(peer_idx) else {
                continue;
            };

            match fc.status {
                FlowStatus::ClearToSend => {
                    let bs = if fc.block_size == 0 {
                        cfg.block_size
                    } else {
                        fc.block_size
                    };
                    let st_min = st_min_to_duration(fc.st_min).unwrap_or(cfg.st_min);
                    isotp_debug!(
                        "async demux wait_fc cts peer_idx={} bs={} st_min_ms={}",
                        peer_idx,
                        bs,
                        st_min.as_millis() as u64
                    );
                    return Ok((bs, st_min, 0));
                }
                FlowStatus::Wait => {
                    wait_count = wait_count.saturating_add(1);
                    isotp_trace!(
                        "async demux wait_fc wait peer_idx={} wait_count={} max={}",
                        peer_idx,
                        wait_count,
                        cfg.wft_max
                    );
                    if wait_count > cfg.wft_max {
                        return Err(IsoTpError::Timeout(TimeoutKind::NBs));
                    }
                    fc_start = self.clock.now();
                }
                FlowStatus::Overflow => return Err(IsoTpError::Overflow),
            }
        }
    }

    async fn send_to_inner<R: AsyncRuntime>(
        &mut self,
        rt: &R,
        remote: u8,
        payload: &[u8],
        timeout: Duration,
    ) -> Result<(), IsoTpError<Tx::Error>> {
        let cfg = self.cfg_for_peer(remote);
        if payload.len() > cfg.max_payload_len {
            isotp_warn!(
                "async demux send_to overflow remote={} payload_len={} max={}",
                remote,
                payload.len(),
                cfg.max_payload_len
            );
            return Err(IsoTpError::Overflow);
        }
        let peer_idx = self.peer_index_or_alloc(remote)?;
        let _ = self.take_pending_fc(peer_idx);

        let start = self.clock.now();

        if payload.len() <= cfg.max_single_frame_payload() {
            isotp_trace!(
                "async demux send_to single-frame remote={} len={}",
                remote,
                payload.len()
            );
            let pdu = Pdu::SingleFrame {
                len: payload.len() as u8,
                data: payload,
            };
            let frame =
                encode_with_prefix_sized(cfg.tx_id, &pdu, cfg.padding, cfg.tx_addr, cfg.frame_len)
                    .map_err(|_| IsoTpError::InvalidFrame)?;
            self.send_frame_with_global_timeout(rt, start, timeout, TimeoutKind::NAs, &frame)
                .await?;
            return Ok(());
        }

        let mut offset = payload.len().min(cfg.max_first_frame_payload());
        let mut next_sn: u8 = 1;
        let wait_count: u8 = 0;
        isotp_debug!(
            "async demux send_to first-frame remote={} payload_len={} ff_chunk={} bs={} st_min_ms={}",
            remote,
            payload.len(),
            offset,
            cfg.block_size,
            cfg.st_min.as_millis() as u64
        );

        let ff = Pdu::FirstFrame {
            len: payload.len() as u16,
            data: &payload[..offset],
        };
        let ff_frame =
            encode_with_prefix_sized(cfg.tx_id, &ff, cfg.padding, cfg.tx_addr, cfg.frame_len)
                .map_err(|_| IsoTpError::InvalidFrame)?;
        self.send_frame_with_global_timeout(rt, start, timeout, TimeoutKind::NAs, &ff_frame)
            .await?;

        let fc_start = self.clock.now();
        let (mut block_size, mut st_min, mut wait_count) = self
            .wait_for_flow_control(rt, peer_idx, &cfg, start, timeout, fc_start, wait_count)
            .await?;
        let mut block_remaining = block_size;
        let mut last_cf_sent: Option<C::Instant> = None;

        while offset < payload.len() {
            if block_size > 0 && block_remaining == 0 {
                let fc_start = self.clock.now();
                let (new_bs, new_st_min, new_wait_count) = self
                    .wait_for_flow_control(rt, peer_idx, &cfg, start, timeout, fc_start, wait_count)
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
                    sleep_or_timeout(
                        &self.clock,
                        rt,
                        start,
                        timeout,
                        TimeoutKind::NAs,
                        st_min - elapsed,
                    )
                    .await?;
                }
            }

            let remaining = payload.len() - offset;
            let chunk = remaining.min(cfg.max_consecutive_frame_payload());
            let cf = Pdu::ConsecutiveFrame {
                sn: next_sn & 0x0F,
                data: &payload[offset..offset + chunk],
            };
            let cf_frame =
                encode_with_prefix_sized(cfg.tx_id, &cf, cfg.padding, cfg.tx_addr, cfg.frame_len)
                    .map_err(|_| IsoTpError::InvalidFrame)?;
            self.send_frame_with_global_timeout(rt, start, timeout, TimeoutKind::NAs, &cf_frame)
                .await?;
            isotp_trace!(
                "async demux send_to cf remote={} sn={} chunk={} offset={} payload_len={} block_remaining={}",
                remote,
                next_sn & 0x0F,
                chunk,
                offset,
                payload.len(),
                block_remaining
            );

            last_cf_sent = Some(self.clock.now());
            offset += chunk;
            next_sn = (next_sn + 1) & 0x0F;

            if block_size > 0 {
                block_remaining = block_remaining.saturating_sub(1);
            }
        }

        Ok(())
    }

    async fn send_functional_to_inner<R: AsyncRuntime>(
        &mut self,
        rt: &R,
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
        let start = self.clock.now();
        self.send_frame_with_global_timeout(rt, start, timeout, TimeoutKind::NAs, &frame)
            .await
    }

    async fn recv_next_into_inner<R: AsyncRuntime>(
        &mut self,
        rt: &R,
        timeout: Duration,
        out: &mut [u8],
    ) -> Result<Option<(u8, usize)>, AppRecvIntoError<Tx::Error>> {
        if let Some(v) = self.deliver_ready_into(out)? {
            return Ok(Some(v));
        }

        let start = self.clock.now();
        loop {
            self.expire_peer_rx_timeouts();

            let global_remaining = match remaining(timeout, self.clock.elapsed(start)) {
                Some(r) => r,
                None => return Ok(None),
            };
            let mut wait_for = global_remaining;
            if let Some(rx_wait) = self.next_rx_timeout_wait() {
                wait_for = wait_for.min(rx_wait);
            }
            if wait_for == Duration::from_millis(0) {
                self.expire_peer_rx_timeouts();
                continue;
            }

            let frame = match rt.timeout(wait_for, self.rx.recv()).await {
                Ok(Ok(frame)) => frame,
                Ok(Err(err)) => return Err(AppRecvIntoError::IsoTp(IsoTpError::LinkError(err))),
                Err(_) => {
                    self.expire_peer_rx_timeouts();
                    if remaining(timeout, self.clock.elapsed(start)).is_none() {
                        return Ok(None);
                    }
                    continue;
                }
            };

            let _ = self
                .ingest_frame(frame)
                .await
                .map_err(AppRecvIntoError::IsoTp)?;
            if let Some(v) = self.deliver_ready_into(out)? {
                return Ok(Some(v));
            }
        }
    }
}

impl<'d, 'a, Tx, Rx, F, C, const MAX_PEERS: usize>
    IsoTpAsyncDemuxApp<'d, 'a, Tx, Rx, F, C, MAX_PEERS>
where
    Tx: AsyncTxFrameIo<Frame = F>,
    Rx: AsyncRxFrameIo<Frame = F, Error = Tx::Error>,
    F: Frame,
    C: Clock,
{
    /// Send a payload to a physical destination address.
    pub async fn send_to<R: AsyncRuntime>(
        &mut self,
        rt: &R,
        to: u8,
        payload: &[u8],
        timeout: Duration,
    ) -> Result<(), IsoTpError<Tx::Error>> {
        self.demux.send_to_inner(rt, to, payload, timeout).await
    }

    /// Send a payload to a functional destination address.
    pub async fn send_to_functional<R: AsyncRuntime>(
        &mut self,
        rt: &R,
        functional_to: u8,
        payload: &[u8],
        timeout: Duration,
    ) -> Result<(), IsoTpError<Tx::Error>> {
        self.demux
            .send_functional_to_inner(rt, functional_to, payload, timeout)
            .await
    }

    /// Compatibility alias for `send_to_functional`.
    pub async fn send_functional_to<R: AsyncRuntime>(
        &mut self,
        rt: &R,
        functional_to: u8,
        payload: &[u8],
        timeout: Duration,
    ) -> Result<(), IsoTpError<Tx::Error>> {
        self.send_to_functional(rt, functional_to, payload, timeout)
            .await
    }

    /// Receive the next payload into `out`.
    ///
    /// Returns `Ok(None)` on timeout, otherwise `Ok(Some((reply_to, len)))`.
    pub async fn recv_next_into<R: AsyncRuntime>(
        &mut self,
        rt: &R,
        timeout: Duration,
        out: &mut [u8],
    ) -> Result<Option<(u8, usize)>, AppRecvIntoError<Tx::Error>> {
        self.demux.recv_next_into_inner(rt, timeout, out).await
    }

    /// Receive the next payload and return a slice into `out`.
    pub async fn recv<'o, R: AsyncRuntime>(
        &mut self,
        rt: &R,
        timeout: Duration,
        out: &'o mut [u8],
    ) -> Result<Option<(u8, &'o [u8])>, AppRecvIntoError<Tx::Error>> {
        match self.recv_next_into(rt, timeout, out).await? {
            Some((reply_to, len)) => Ok(Some((reply_to, &out[..len]))),
            None => Ok(None),
        }
    }
}

impl<'d, 'a, Tx, Rx, F, C, const MAX_PEERS: usize>
    IsoTpAsyncDemuxDriver<'d, 'a, Tx, Rx, F, C, MAX_PEERS>
where
    Tx: AsyncTxFrameIo<Frame = F>,
    Rx: AsyncRxFrameIo<Frame = F, Error = Tx::Error>,
    F: Frame,
    C: Clock,
{
    /// Process at most one receive frame and protocol bookkeeping step.
    pub async fn step<R: AsyncRuntime>(
        &mut self,
        rt: &R,
        max_wait: Duration,
    ) -> Result<Progress, IsoTpError<Tx::Error>> {
        self.demux.expire_peer_rx_timeouts();

        if max_wait == Duration::from_millis(0) {
            return Ok(if self.demux.ready.is_empty() {
                Progress::WouldBlock
            } else {
                Progress::Completed
            });
        }

        let mut wait_for = max_wait;
        if let Some(rx_wait) = self.demux.next_rx_timeout_wait() {
            wait_for = wait_for.min(rx_wait);
        }
        if wait_for == Duration::from_millis(0) {
            self.demux.expire_peer_rx_timeouts();
            return Ok(if self.demux.ready.is_empty() {
                Progress::WouldBlock
            } else {
                Progress::Completed
            });
        }

        let frame = match rt.timeout(wait_for, self.demux.rx.recv()).await {
            Ok(Ok(frame)) => frame,
            Ok(Err(err)) => return Err(IsoTpError::LinkError(err)),
            Err(_) => {
                self.demux.expire_peer_rx_timeouts();
                return Ok(if self.demux.ready.is_empty() {
                    Progress::WouldBlock
                } else {
                    Progress::Completed
                });
            }
        };
        let progress = self.demux.ingest_frame(frame).await?;
        if !self.demux.ready.is_empty() {
            Ok(Progress::Completed)
        } else {
            Ok(progress)
        }
    }

    /// Continuously pump receive-side demux machinery.
    ///
    /// Intended for a dedicated runtime task.
    pub async fn run_forever<R: AsyncRuntime>(
        &mut self,
        rt: &R,
        idle_sleep: Duration,
    ) -> Result<(), IsoTpError<Tx::Error>> {
        let wait = if idle_sleep == Duration::from_millis(0) {
            Duration::from_millis(1)
        } else {
            idle_sleep
        };
        loop {
            let _ = self.step(rt, wait).await?;
        }
    }
}

fn remaining(timeout: Duration, elapsed: Duration) -> Option<Duration> {
    timeout.checked_sub(elapsed)
}

#[cfg(feature = "defmt")]
#[inline]
fn flow_status_code(status: FlowStatus) -> u8 {
    match status {
        FlowStatus::ClearToSend => 0,
        FlowStatus::Wait => 1,
        FlowStatus::Overflow => 2,
    }
}

#[cfg(feature = "defmt")]
#[inline]
fn uds_kind_code(kind: Uds29Kind) -> u8 {
    match kind {
        Uds29Kind::Physical => 0,
        Uds29Kind::Functional => 1,
    }
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
