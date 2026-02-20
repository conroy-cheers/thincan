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
use crate::rx::{RxMachine, RxOutcome, RxStorage};
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
    peers: [Option<Peer<'a>>; MAX_PEERS],
    free: [Option<RxStorage<'a>>; MAX_PEERS],
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

    fn take_pending_fc(&mut self, peer_idx: usize) -> Option<PendingFc> {
        self.peers[peer_idx]
            .as_mut()
            .and_then(|p| p.pending_fc.take())
    }

    fn deliver_ready_into(
        &mut self,
        out: &mut [u8],
    ) -> Result<Option<(u8, usize)>, AppRecvIntoError<Tx::Error>> {
        let Some(idx) = self.ready.pop() else {
            return Ok(None);
        };
        let peer = self.peers[idx].as_mut().expect("ready peer exists");
        let data = peer.rx_machine.take_completed();
        if data.len() > out.len() {
            // Keep payload queued for a later call with a larger buffer.
            let _ = self.ready.push(idx);
            return Err(AppRecvIntoError::BufferTooSmall {
                needed: data.len(),
                got: out.len(),
            });
        }
        out[..data.len()].copy_from_slice(data);
        peer.rx_ready = false;
        Ok(Some((peer.remote, data.len())))
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
        let mut fc_to_send: Option<(FlowStatus, u8, u8)> = None;
        let mut final_result: Result<Progress, IsoTpError<Tx::Error>> = Ok(Progress::InFlight);

        {
            let peer = self.peers[peer_idx].as_mut().expect("peer exists");

            if peer.rx_ready {
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

                let pdu = decode_with_offset(frame.data(), cfg.rx_pci_offset())
                    .map_err(|_| IsoTpError::InvalidFrame)?;

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
                        final_result = Ok(Progress::InFlight);
                    }
                    _ => {
                        let outcome = match peer.rx_machine.on_pdu(&cfg, &self.rx_flow_control, pdu)
                        {
                            Ok(o) => o,
                            Err(IsoTpError::Overflow) => {
                                fc_to_send = Some((
                                    FlowStatus::Overflow,
                                    0,
                                    duration_to_st_min(self.rx_flow_control.st_min),
                                ));
                                final_result = Err(IsoTpError::RxOverflow);
                                // Delay return until after FC send.
                                RxOutcome::None
                            }
                            Err(IsoTpError::UnexpectedPdu) => return Ok(Progress::InFlight),
                            Err(IsoTpError::BadSequence) => return Err(IsoTpError::BadSequence),
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

                        match outcome {
                            RxOutcome::None => {}
                            RxOutcome::SendFlowControl {
                                status,
                                block_size,
                                st_min,
                            } => {
                                fc_to_send = Some((status, block_size, st_min));
                            }
                            RxOutcome::Completed(_len) => {
                                peer.rx_ready = true;
                                self.ready
                                    .push(peer_idx)
                                    .map_err(|_| IsoTpError::RxOverflow)?;
                                final_result = Ok(Progress::Completed);
                            }
                        }
                    }
                }
            }
        }

        if let Some((status, block_size, st_min)) = fc_to_send {
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
            if self.clock.elapsed(fc_start) >= cfg.n_bs {
                return Err(IsoTpError::Timeout(TimeoutKind::NBs));
            }

            let fc_remaining = cfg.n_bs - self.clock.elapsed(fc_start);
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
                    return Ok((bs, st_min, 0));
                }
                FlowStatus::Wait => {
                    wait_count = wait_count.saturating_add(1);
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
            return Err(IsoTpError::Overflow);
        }
        let peer_idx = self.peer_index_or_alloc(remote)?;
        let _ = self.take_pending_fc(peer_idx);

        let start = self.clock.now();

        if payload.len() <= cfg.max_single_frame_payload() {
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
            let remaining = match remaining(timeout, self.clock.elapsed(start)) {
                Some(r) => r,
                None => return Ok(None),
            };

            let frame = match rt.timeout(remaining, self.rx.recv()).await {
                Ok(Ok(frame)) => frame,
                Ok(Err(err)) => return Err(AppRecvIntoError::IsoTp(IsoTpError::LinkError(err))),
                Err(_) => return Ok(None),
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
        if !self.demux.ready.is_empty() {
            return Ok(Progress::Completed);
        }
        if max_wait == Duration::from_millis(0) {
            return Ok(Progress::WouldBlock);
        }

        let frame = match rt.timeout(max_wait, self.demux.rx.recv()).await {
            Ok(Ok(frame)) => frame,
            Ok(Err(err)) => return Err(IsoTpError::LinkError(err)),
            Err(_) => return Ok(Progress::WouldBlock),
        };
        self.demux.ingest_frame(frame).await
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
