use core::time::Duration;

use can_iso_tp::{IsoTpAsyncNode, IsoTpConfig, IsoTpError, IsoTpNode, Progress};
use embedded_can::StandardId;
use embedded_can_interface::{AsyncRxFrameIo, AsyncTxFrameIo, Id, RxFrameIo, TxFrameIo};
use embedded_can_mock::MockFrame;

#[derive(Clone, Copy, Debug, Default)]
struct TestClock;

impl can_iso_tp::Clock for TestClock {
    type Instant = u64;

    fn now(&self) -> Self::Instant {
        0
    }

    fn elapsed(&self, _earlier: Self::Instant) -> Duration {
        Duration::from_millis(0)
    }

    fn add(&self, instant: Self::Instant, dur: Duration) -> Self::Instant {
        instant.saturating_add(dur.as_millis() as u64)
    }
}

fn cfg_pair(max_payload_len: usize) -> (IsoTpConfig, IsoTpConfig) {
    (
        IsoTpConfig {
            tx_id: Id::Standard(StandardId::new(0x700).unwrap()),
            rx_id: Id::Standard(StandardId::new(0x701).unwrap()),
            max_payload_len,
            ..IsoTpConfig::default()
        },
        IsoTpConfig {
            tx_id: Id::Standard(StandardId::new(0x701).unwrap()),
            rx_id: Id::Standard(StandardId::new(0x700).unwrap()),
            max_payload_len,
            ..IsoTpConfig::default()
        },
    )
}

#[derive(Default)]
struct NoTx;

impl TxFrameIo for NoTx {
    type Frame = MockFrame;
    type Error = ();

    fn send(&mut self, _frame: &Self::Frame) -> Result<(), Self::Error> {
        Ok(())
    }

    fn try_send(&mut self, _frame: &Self::Frame) -> Result<(), Self::Error> {
        Ok(())
    }

    fn send_timeout(&mut self, frame: &Self::Frame, _timeout: Duration) -> Result<(), Self::Error> {
        self.send(frame)
    }
}

#[derive(Default)]
struct NoRx;

impl RxFrameIo for NoRx {
    type Frame = MockFrame;
    type Error = ();

    fn recv(&mut self) -> Result<Self::Frame, Self::Error> {
        Err(())
    }

    fn try_recv(&mut self) -> Result<Self::Frame, Self::Error> {
        Err(())
    }

    fn recv_timeout(&mut self, _timeout: Duration) -> Result<Self::Frame, Self::Error> {
        Err(())
    }

    fn wait_not_empty(&mut self) -> Result<(), Self::Error> {
        Ok(())
    }
}

#[test]
fn node_builder_requires_sufficient_borrowed_buffer() {
    let (cfg_a, _cfg_b) = cfg_pair(16);
    let mut rx_buf_too_small = [0u8; 8];
    let err = match IsoTpNode::builder(NoTx, NoRx, cfg_a, TestClock)
        .rx_buffer(&mut rx_buf_too_small)
        .build()
    {
        Ok(_) => panic!("expected InvalidConfig for insufficient rx buffer"),
        Err(err) => err,
    };
    assert!(matches!(err, IsoTpError::InvalidConfig));
}

#[test]
fn node_with_clock_borrowed_buffer_construction_and_poll_paths() {
    let (cfg_a, _cfg_b) = cfg_pair(16);
    let mut rx_buf = [0u8; 16];
    let mut node = IsoTpNode::with_clock(NoTx, NoRx, cfg_a, TestClock, &mut rx_buf).unwrap();

    // No RX frames available: should WouldBlock.
    let res = node.poll_recv(0, &mut |_| {}).unwrap();
    assert_eq!(res, Progress::WouldBlock);
}

#[derive(Default)]
struct AsyncNoTx;

impl AsyncTxFrameIo for AsyncNoTx {
    type Frame = MockFrame;
    type Error = ();

    async fn send(&mut self, _frame: &Self::Frame) -> Result<(), Self::Error> {
        Ok(())
    }

    async fn send_timeout(
        &mut self,
        frame: &Self::Frame,
        _timeout: Duration,
    ) -> Result<(), Self::Error> {
        self.send(frame).await
    }
}

#[derive(Default)]
struct AsyncNoRx;

impl AsyncRxFrameIo for AsyncNoRx {
    type Frame = MockFrame;
    type Error = ();

    async fn recv(&mut self) -> Result<Self::Frame, Self::Error> {
        Err(())
    }

    async fn recv_timeout(&mut self, _timeout: Duration) -> Result<Self::Frame, Self::Error> {
        Err(())
    }

    async fn wait_not_empty(&mut self) -> Result<(), Self::Error> {
        Ok(())
    }
}

#[tokio::test(flavor = "current_thread")]
async fn async_node_builder_with_borrowed_buffer_constructs() {
    let (cfg_a, _cfg_b) = cfg_pair(8);
    let mut rx_buf = [0u8; 8];

    let mut node = IsoTpAsyncNode::builder(AsyncNoTx, AsyncNoRx, cfg_a, TestClock)
        .rx_buffer(&mut rx_buf)
        .build()
        .unwrap();

    // Oversized send should fail before touching runtime or RX.
    let rt = TokioRtShim;
    let payload = [0u8; 16];
    let err = node
        .send(&rt, &payload, Duration::from_millis(10))
        .await
        .unwrap_err();
    assert!(matches!(err, IsoTpError::Overflow));
}

// Minimal AsyncRuntime shim for this file: uses tokio's timeouts/sleeps.
struct TokioRtShim;

impl can_iso_tp::AsyncRuntime for TokioRtShim {
    type TimeoutError = tokio::time::error::Elapsed;

    type Sleep<'a>
        = tokio::time::Sleep
    where
        Self: 'a;

    fn sleep<'a>(&'a self, duration: Duration) -> Self::Sleep<'a> {
        tokio::time::sleep(duration)
    }

    type Timeout<'a, F>
        = tokio::time::Timeout<F>
    where
        Self: 'a,
        F: core::future::Future + 'a;

    fn timeout<'a, F>(&'a self, duration: Duration, future: F) -> Self::Timeout<'a, F>
    where
        F: core::future::Future + 'a,
    {
        tokio::time::timeout(duration, future)
    }
}
