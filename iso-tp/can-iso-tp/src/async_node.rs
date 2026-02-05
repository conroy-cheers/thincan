//! Async ISO-TP node implementation.

use core::time::Duration;

use embedded_can::Frame;
use embedded_can_interface::{AsyncRxFrameIo, AsyncTxFrameIo};

use crate::async_io::AsyncRuntime;
use crate::errors::{IsoTpError, TimeoutKind};
use crate::pdu::{
    FlowStatus, Pdu, decode_with_offset, duration_to_st_min, encode_with_prefix_sized,
    st_min_to_duration,
};
use crate::rx::{RxMachine, RxOutcome, RxStorage};
use crate::timer::Clock;
use crate::{IsoTpConfig, id_matches};

/// Async ISO-TP endpoint backed by async transmit/receive halves and a clock.
pub struct IsoTpAsyncNode<'a, Tx, Rx, F, C>
where
    Tx: AsyncTxFrameIo<Frame = F>,
    Rx: AsyncRxFrameIo<Frame = F, Error = Tx::Error>,
    C: Clock,
{
    tx: Tx,
    rx: Rx,
    cfg: IsoTpConfig,
    clock: C,
    rx_machine: RxMachine<'a>,
}

impl<'a, Tx, Rx, F, C> IsoTpAsyncNode<'a, Tx, Rx, F, C>
where
    Tx: AsyncTxFrameIo<Frame = F>,
    Rx: AsyncRxFrameIo<Frame = F, Error = Tx::Error>,
    F: Frame,
    C: Clock,
{
    /// Start building an [`IsoTpAsyncNode`] (requires RX storage before `build()`).
    pub fn builder(tx: Tx, rx: Rx, cfg: IsoTpConfig, clock: C) -> IsoTpAsyncNodeBuilder<Tx, Rx, C> {
        IsoTpAsyncNodeBuilder { tx, rx, cfg, clock }
    }

    /// Construct using a provided clock and caller-provided RX buffer.
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

    /// Blocking-by-await send until completion or timeout.
    ///
    /// This method performs the full ISO-TP handshake and segmentation as needed, using the
    /// provided runtime for sleeps/timeouts.
    pub async fn send<R: AsyncRuntime>(
        &mut self,
        rt: &R,
        payload: &[u8],
        timeout: Duration,
    ) -> Result<(), IsoTpError<Tx::Error>> {
        if payload.len() > self.cfg.max_payload_len {
            return Err(IsoTpError::Overflow);
        }

        let start = self.clock.now();
        if self.clock.elapsed(start) >= timeout {
            return Err(IsoTpError::Timeout(TimeoutKind::NAs));
        }

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
            self.send_frame(rt, start, timeout, TimeoutKind::NAs, &frame)
                .await?;
            return Ok(());
        }

        let mut offset = payload.len().min(self.cfg.max_first_frame_payload());
        let mut next_sn: u8 = 1;
        let wait_count: u8 = 0;

        let pdu = Pdu::FirstFrame {
            len: payload.len() as u16,
            data: &payload[..offset],
        };
        let frame = encode_with_prefix_sized(
            self.cfg.tx_id,
            &pdu,
            self.cfg.padding,
            self.cfg.tx_addr,
            self.cfg.frame_len,
        )
        .map_err(|_| IsoTpError::InvalidFrame)?;
        self.send_frame(rt, start, timeout, TimeoutKind::NAs, &frame)
            .await?;

        let fc_start = self.clock.now();
        let (mut block_size, mut st_min, mut wait_count) = self
            .wait_for_flow_control(rt, start, timeout, fc_start, wait_count)
            .await?;
        let mut block_remaining = block_size;

        let mut last_cf_sent: Option<C::Instant> = None;
        while offset < payload.len() {
            if block_size > 0 && block_remaining == 0 {
                let fc_start = self.clock.now();
                let (new_bs, new_st_min, new_wait_count) = self
                    .wait_for_flow_control(rt, start, timeout, fc_start, wait_count)
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

            let remaining = payload.len() - offset;
            let chunk = remaining.min(self.cfg.max_consecutive_frame_payload());
            let pdu = Pdu::ConsecutiveFrame {
                sn: next_sn & 0x0F,
                data: &payload[offset..offset + chunk],
            };
            let frame = encode_with_prefix_sized(
                self.cfg.tx_id,
                &pdu,
                self.cfg.padding,
                self.cfg.tx_addr,
                self.cfg.frame_len,
            )
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

    /// Blocking-by-await receive until a full payload arrives or timeout.
    ///
    /// The provided `deliver` callback is invoked only when a full payload has been reassembled.
    /// The slice passed to `deliver` is valid until the next receive operation mutates the internal
    /// reassembly buffer.
    pub async fn recv<R: AsyncRuntime>(
        &mut self,
        rt: &R,
        timeout: Duration,
        deliver: &mut dyn FnMut(&[u8]),
    ) -> Result<(), IsoTpError<Tx::Error>> {
        let start = self.clock.now();

        loop {
            let frame = self
                .recv_frame(rt, start, timeout, TimeoutKind::NAr)
                .await?;

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

            let outcome = match self.rx_machine.on_pdu(&self.cfg, pdu) {
                Ok(o) => o,
                Err(IsoTpError::Overflow) => {
                    let _ = self.send_overflow_fc(rt, start, timeout).await;
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
                RxOutcome::None => continue,
                RxOutcome::SendFlowControl {
                    status,
                    block_size,
                    st_min,
                } => {
                    self.send_flow_control(rt, start, timeout, status, block_size, st_min)
                        .await?;
                }
                RxOutcome::Completed(_len) => {
                    let data = self.rx_machine.take_completed();
                    deliver(data);
                    return Ok(());
                }
            }
        }
    }

    async fn wait_for_flow_control<R: AsyncRuntime>(
        &mut self,
        rt: &R,
        global_start: C::Instant,
        global_timeout: Duration,
        mut fc_start: C::Instant,
        mut wait_count: u8,
    ) -> Result<(u8, Duration, u8), IsoTpError<Tx::Error>> {
        loop {
            if self.clock.elapsed(fc_start) >= self.cfg.n_bs {
                return Err(IsoTpError::Timeout(TimeoutKind::NBs));
            }

            let fc_remaining = self.cfg.n_bs - self.clock.elapsed(fc_start);
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
                        let bs = if block_size == 0 {
                            self.cfg.block_size
                        } else {
                            block_size
                        };
                        let st_min = st_min_to_duration(st_min).unwrap_or(self.cfg.st_min);
                        return Ok((bs, st_min, 0));
                    }
                    FlowStatus::Wait => {
                        wait_count = wait_count.saturating_add(1);
                        if wait_count > self.cfg.wft_max {
                            return Err(IsoTpError::Timeout(TimeoutKind::NBs));
                        }
                        fc_start = self.clock.now();
                        continue;
                    }
                    FlowStatus::Overflow => return Err(IsoTpError::Overflow),
                },
                _ => continue,
            }
        }
    }

    async fn send_flow_control<R: AsyncRuntime>(
        &mut self,
        rt: &R,
        start: C::Instant,
        timeout: Duration,
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
        self.send_frame(rt, start, timeout, TimeoutKind::NAs, &frame)
            .await?;
        Ok(())
    }

    async fn send_overflow_fc<R: AsyncRuntime>(
        &mut self,
        rt: &R,
        start: C::Instant,
        timeout: Duration,
    ) -> Result<(), IsoTpError<Tx::Error>> {
        self.send_flow_control(
            rt,
            start,
            timeout,
            FlowStatus::Overflow,
            0,
            duration_to_st_min(self.cfg.st_min),
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
impl<'a, Tx, Rx, F> IsoTpAsyncNode<'a, Tx, Rx, F, crate::StdClock>
where
    Tx: AsyncTxFrameIo<Frame = F>,
    Rx: AsyncRxFrameIo<Frame = F, Error = Tx::Error>,
    F: Frame,
{
    /// Convenience constructor using `StdClock`.
    pub fn with_std_clock(
        tx: Tx,
        rx: Rx,
        cfg: IsoTpConfig,
        rx_buffer: &'a mut [u8],
    ) -> Result<Self, IsoTpError<()>> {
        IsoTpAsyncNode::with_clock(tx, rx, cfg, crate::StdClock, rx_buffer)
    }
}

/// Builder for [`IsoTpAsyncNode`] that enforces providing RX storage before construction.
pub struct IsoTpAsyncNodeBuilder<Tx, Rx, C> {
    tx: Tx,
    rx: Rx,
    cfg: IsoTpConfig,
    clock: C,
}

/// Builder state after RX storage has been provided.
pub struct IsoTpAsyncNodeBuilderWithRx<'a, Tx, Rx, C> {
    tx: Tx,
    rx: Rx,
    cfg: IsoTpConfig,
    clock: C,
    rx_storage: RxStorage<'a>,
}

impl<Tx, Rx, C> IsoTpAsyncNodeBuilder<Tx, Rx, C> {
    /// Provide the RX buffer used for receive-side reassembly.
    pub fn rx_buffer<'a>(self, buffer: &'a mut [u8]) -> IsoTpAsyncNodeBuilderWithRx<'a, Tx, Rx, C> {
        self.rx_storage(RxStorage::Borrowed(buffer))
    }

    /// Provide explicit RX storage (borrowed or owned, depending on features).
    pub fn rx_storage<'a>(
        self,
        rx_storage: RxStorage<'a>,
    ) -> IsoTpAsyncNodeBuilderWithRx<'a, Tx, Rx, C> {
        IsoTpAsyncNodeBuilderWithRx {
            tx: self.tx,
            rx: self.rx,
            cfg: self.cfg,
            clock: self.clock,
            rx_storage,
        }
    }
}

impl<'a, Tx, Rx, C> IsoTpAsyncNodeBuilderWithRx<'a, Tx, Rx, C>
where
    Tx: AsyncTxFrameIo,
    Rx: AsyncRxFrameIo<Frame = <Tx as AsyncTxFrameIo>::Frame, Error = <Tx as AsyncTxFrameIo>::Error>,
    <Tx as AsyncTxFrameIo>::Frame: Frame,
    C: Clock,
{
    /// Validate configuration and build an [`IsoTpAsyncNode`].
    pub fn build(
        self,
    ) -> Result<IsoTpAsyncNode<'a, Tx, Rx, <Tx as AsyncTxFrameIo>::Frame, C>, IsoTpError<()>> {
        self.cfg.validate().map_err(|_| IsoTpError::InvalidConfig)?;
        if self.rx_storage.capacity() < self.cfg.max_payload_len {
            return Err(IsoTpError::InvalidConfig);
        }
        Ok(IsoTpAsyncNode {
            tx: self.tx,
            rx: self.rx,
            cfg: self.cfg,
            clock: self.clock,
            rx_machine: RxMachine::new(self.rx_storage),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::boxed::Box;
    use core::future::Future;
    use core::pin::Pin;
    use std::sync::Mutex;

    #[derive(Debug, Clone, Copy)]
    struct TestClock {
        now_ms: u64,
    }

    impl Clock for TestClock {
        type Instant = u64;

        fn now(&self) -> Self::Instant {
            self.now_ms
        }

        fn elapsed(&self, earlier: Self::Instant) -> Duration {
            Duration::from_millis(self.now_ms.saturating_sub(earlier))
        }

        fn add(&self, instant: Self::Instant, dur: Duration) -> Self::Instant {
            instant.saturating_add(dur.as_millis() as u64)
        }
    }

    #[derive(Default)]
    struct TestRuntime {
        sleeps: Mutex<Vec<Duration>>,
    }

    impl AsyncRuntime for TestRuntime {
        type TimeoutError = ();

        type Sleep<'a>
            = core::future::Ready<()>
        where
            Self: 'a;

        fn sleep<'a>(&'a self, duration: Duration) -> Self::Sleep<'a> {
            self.sleeps.lock().unwrap().push(duration);
            core::future::ready(())
        }

        type Timeout<'a, F>
            = Pin<Box<dyn Future<Output = Result<F::Output, Self::TimeoutError>> + 'a>>
        where
            Self: 'a,
            F: Future + 'a;

        fn timeout<'a, F>(&'a self, _duration: Duration, future: F) -> Self::Timeout<'a, F>
        where
            F: Future + 'a,
        {
            Box::pin(async move { Ok(future.await) })
        }
    }

    #[test]
    fn remaining_handles_underflow() {
        assert_eq!(
            remaining(Duration::from_millis(10), Duration::from_millis(5)),
            Some(Duration::from_millis(5))
        );
        assert_eq!(
            remaining(Duration::from_millis(10), Duration::from_millis(10)),
            Some(Duration::from_millis(0))
        );
        assert_eq!(
            remaining(Duration::from_millis(10), Duration::from_millis(11)),
            None
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn sleep_or_timeout_uses_min_of_requested_and_remaining() {
        let clock = TestClock { now_ms: 30 };
        let rt = TestRuntime::default();
        let start = 0u64;

        let res = sleep_or_timeout::<_, _, ()>(
            &clock,
            &rt,
            start,
            Duration::from_millis(100),
            TimeoutKind::NAs,
            Duration::from_millis(200),
        )
        .await;
        assert!(res.is_ok());

        let sleeps = rt.sleeps.lock().unwrap().clone();
        assert_eq!(sleeps, vec![Duration::from_millis(70)]);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn sleep_or_timeout_returns_timeout_when_elapsed_exceeds_deadline() {
        let clock = TestClock { now_ms: 100 };
        let rt = TestRuntime::default();
        let start = 0u64;

        let err = sleep_or_timeout::<_, _, ()>(
            &clock,
            &rt,
            start,
            Duration::from_millis(50),
            TimeoutKind::NAs,
            Duration::from_millis(1),
        )
        .await
        .unwrap_err();
        assert!(matches!(err, IsoTpError::Timeout(TimeoutKind::NAs)));

        let sleeps = rt.sleeps.lock().unwrap().clone();
        assert!(sleeps.is_empty());
    }

    #[test]
    fn test_clock_now_and_add_are_exercised() {
        let clock = TestClock { now_ms: 5 };
        assert_eq!(clock.now(), 5);
        assert_eq!(clock.add(10, Duration::from_millis(7)), 17);
    }
}
