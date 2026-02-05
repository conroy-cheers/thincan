use core::future::Future;
use core::pin::Pin;
use core::time::Duration;

use can_iso_tp::pdu::{Pdu, decode_with_offset, encode_with_prefix};
use can_iso_tp::{AsyncRuntime, Clock, IsoTpAsyncNode, IsoTpConfig, IsoTpError, TimeoutKind};
use embedded_can::Frame;
use embedded_can::StandardId;
use embedded_can_interface::{AsyncRxFrameIo, AsyncTxFrameIo, Id};
use embedded_can_mock::MockFrame;

#[derive(Clone, Copy, Debug)]
struct TestClock;

impl Clock for TestClock {
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

struct PassthroughRt;

impl AsyncRuntime for PassthroughRt {
    type TimeoutError = ();

    type Sleep<'a>
        = core::future::Ready<()>
    where
        Self: 'a;

    fn sleep<'a>(&'a self, _duration: Duration) -> Self::Sleep<'a> {
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

#[derive(Clone, Default)]
struct RecordingTx {
    sent: std::sync::Arc<std::sync::Mutex<Vec<MockFrame>>>,
}

impl AsyncTxFrameIo for RecordingTx {
    type Frame = MockFrame;
    type Error = ();

    async fn send(&mut self, frame: &Self::Frame) -> Result<(), Self::Error> {
        self.sent.lock().unwrap().push(frame.clone());
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

struct ScriptedRx {
    frames: std::collections::VecDeque<Result<MockFrame, ()>>,
}

impl ScriptedRx {
    fn new(frames: impl IntoIterator<Item = Result<MockFrame, ()>>) -> Self {
        Self {
            frames: frames.into_iter().collect(),
        }
    }
}

impl AsyncRxFrameIo for ScriptedRx {
    type Frame = MockFrame;
    type Error = ();

    async fn recv(&mut self) -> Result<Self::Frame, Self::Error> {
        self.frames.pop_front().expect("test rx script exhausted")
    }

    async fn recv_timeout(&mut self, _timeout: Duration) -> Result<Self::Frame, Self::Error> {
        self.recv().await
    }

    async fn wait_not_empty(&mut self) -> Result<(), Self::Error> {
        Ok(())
    }
}

fn cfg_with_addr(max_payload_len: usize, addr: u8) -> IsoTpConfig {
    IsoTpConfig {
        tx_id: Id::Standard(StandardId::new(0x700).unwrap()),
        rx_id: Id::Standard(StandardId::new(0x701).unwrap()),
        rx_addr: Some(addr),
        max_payload_len,
        ..IsoTpConfig::default()
    }
}

#[tokio::test(flavor = "current_thread")]
async fn async_recv_skips_wrong_addr_then_ignores_unexpected_pdu_then_delivers() {
    let cfg = cfg_with_addr(64, 0xAA);
    let mut rx_buf = [0u8; 64];

    let wrong_addr = encode_with_prefix::<MockFrame>(
        cfg.rx_id,
        &Pdu::SingleFrame {
            len: 1,
            data: &[0x01],
        },
        None,
        Some(0xAB),
    )
    .unwrap();
    let unexpected_cf = encode_with_prefix::<MockFrame>(
        cfg.rx_id,
        &Pdu::ConsecutiveFrame {
            sn: 1,
            data: &[0xFF],
        },
        None,
        cfg.rx_addr,
    )
    .unwrap();
    let expected_payload = [0x11, 0x22];
    let good = encode_with_prefix::<MockFrame>(
        cfg.rx_id,
        &Pdu::SingleFrame {
            len: expected_payload.len() as u8,
            data: &expected_payload,
        },
        None,
        cfg.rx_addr,
    )
    .unwrap();

    let tx = RecordingTx::default();
    let rx = ScriptedRx::new([Ok(wrong_addr), Ok(unexpected_cf), Ok(good)]);
    let mut node = IsoTpAsyncNode::with_clock(tx, rx, cfg, TestClock, &mut rx_buf).unwrap();

    let rt = PassthroughRt;
    let mut delivered = Vec::new();
    node.recv(&rt, Duration::from_millis(10), &mut |data| {
        delivered = data.to_vec()
    })
    .await
    .unwrap();
    assert_eq!(delivered, expected_payload);
}

#[tokio::test(flavor = "current_thread")]
async fn async_recv_overflow_sends_overflow_flow_control() {
    let cfg = cfg_with_addr(8, 0xCC);
    let mut rx_buf = [0u8; 64];
    let tx_pci_offset = cfg.tx_pci_offset();
    let first = encode_with_prefix::<MockFrame>(
        cfg.rx_id,
        &Pdu::FirstFrame {
            len: 20,
            data: &[0x00, 0x01, 0x02, 0x03, 0x04, 0x05],
        },
        None,
        cfg.rx_addr,
    )
    .unwrap();

    let tx = RecordingTx::default();
    let sent = tx.sent.clone();
    let rx = ScriptedRx::new([Ok(first)]);
    let mut node = IsoTpAsyncNode::with_clock(tx, rx, cfg, TestClock, &mut rx_buf).unwrap();

    let rt = PassthroughRt;
    let err = node
        .recv(&rt, Duration::from_millis(10), &mut |_data| {})
        .await
        .unwrap_err();
    assert!(matches!(err, IsoTpError::RxOverflow));

    let sent = sent.lock().unwrap().clone();
    assert_eq!(sent.len(), 1);
    let fc = decode_with_offset(sent[0].data(), tx_pci_offset).unwrap();
    assert!(matches!(
        fc,
        Pdu::FlowControl {
            status: can_iso_tp::pdu::FlowStatus::Overflow,
            ..
        }
    ));
}

#[tokio::test(flavor = "current_thread")]
async fn async_recv_maps_link_error() {
    let cfg = cfg_with_addr(8, 0x01);
    let mut rx_buf = [0u8; 64];
    let tx = RecordingTx::default();
    let rx = ScriptedRx::new([Err(())]);
    let mut node = IsoTpAsyncNode::with_clock(tx, rx, cfg, TestClock, &mut rx_buf).unwrap();

    let rt = PassthroughRt;
    let err = node
        .recv(&rt, Duration::from_millis(10), &mut |_data| {})
        .await
        .unwrap_err();
    assert!(matches!(err, IsoTpError::LinkError(())));
}

#[tokio::test(flavor = "current_thread")]
async fn async_send_maps_link_error_and_timeout() {
    // Exercise wait-for-flow-control branches that are hard to hit in the broader integration tests.
    let mut cfg = cfg_with_addr(64, 0xDD);
    cfg.wft_max = 1;

    let wrong_addr_fc = encode_with_prefix::<MockFrame>(
        cfg.rx_id,
        &Pdu::FlowControl {
            status: can_iso_tp::pdu::FlowStatus::ClearToSend,
            block_size: 0,
            st_min: 0,
        },
        None,
        Some(0xDE),
    )
    .unwrap();
    let non_fc_noise = encode_with_prefix::<MockFrame>(
        cfg.rx_id,
        &Pdu::SingleFrame {
            len: 1,
            data: &[0x00],
        },
        None,
        cfg.rx_addr,
    )
    .unwrap();
    let wait_fc = encode_with_prefix::<MockFrame>(
        cfg.rx_id,
        &Pdu::FlowControl {
            status: can_iso_tp::pdu::FlowStatus::Wait,
            block_size: 0,
            st_min: 0,
        },
        None,
        cfg.rx_addr,
    )
    .unwrap();
    let cts_fc = encode_with_prefix::<MockFrame>(
        cfg.rx_id,
        &Pdu::FlowControl {
            status: can_iso_tp::pdu::FlowStatus::ClearToSend,
            block_size: 0,
            st_min: 0,
        },
        None,
        cfg.rx_addr,
    )
    .unwrap();

    let mut rx_buf = [0u8; 64];
    let tx = RecordingTx::default();
    let rx = ScriptedRx::new([Ok(wrong_addr_fc), Ok(non_fc_noise), Ok(wait_fc), Ok(cts_fc)]);
    let mut node = IsoTpAsyncNode::with_clock(tx, rx, cfg, TestClock, &mut rx_buf).unwrap();

    let rt = PassthroughRt;
    node.send(&rt, &[0u8; 8], Duration::from_millis(10))
        .await
        .unwrap();

    #[derive(Default)]
    struct FailingTx;

    impl AsyncTxFrameIo for FailingTx {
        type Frame = MockFrame;
        type Error = ();

        async fn send(&mut self, _frame: &Self::Frame) -> Result<(), Self::Error> {
            Err(())
        }

        async fn send_timeout(
            &mut self,
            frame: &Self::Frame,
            _timeout: Duration,
        ) -> Result<(), Self::Error> {
            self.send(frame).await
        }
    }

    struct AlwaysTimeoutRt;

    impl AsyncRuntime for AlwaysTimeoutRt {
        type TimeoutError = ();

        type Sleep<'a>
            = core::future::Ready<()>
        where
            Self: 'a;

        fn sleep<'a>(&'a self, _duration: Duration) -> Self::Sleep<'a> {
            core::future::ready(())
        }

        type Timeout<'a, F>
            = core::future::Ready<Result<F::Output, Self::TimeoutError>>
        where
            Self: 'a,
            F: Future + 'a;

        fn timeout<'a, F>(&'a self, _duration: Duration, _future: F) -> Self::Timeout<'a, F>
        where
            F: Future + 'a,
        {
            core::future::ready(Err(()))
        }
    }

    struct NoRx;

    impl AsyncRxFrameIo for NoRx {
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

    let cfg = cfg_with_addr(8, 0x01);
    let mut rx_buf = [0u8; 8];
    let rt = PassthroughRt;
    let timeout_rt = AlwaysTimeoutRt;

    let tx = FailingTx;
    let rx = NoRx;
    let mut node = IsoTpAsyncNode::with_clock(tx, rx, cfg, TestClock, &mut rx_buf).unwrap();

    let err = node
        .send(&rt, &[0x55], Duration::from_millis(10))
        .await
        .unwrap_err();
    assert!(matches!(err, IsoTpError::LinkError(())));

    let err = node
        .send(&timeout_rt, &[0x55], Duration::from_millis(10))
        .await
        .unwrap_err();
    assert!(matches!(err, IsoTpError::Timeout(TimeoutKind::NAs)));
}
