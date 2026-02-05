use core::time::Duration;
use std::sync::Arc;
use std::time::Instant;

use can_iso_tp::pdu::{Pdu, decode, encode};
use can_iso_tp::{AsyncRuntime, IsoTpAsyncNode, IsoTpConfig, RxStorage, StdClock};
use embedded_can::Frame;
use embedded_can_interface::AsyncTxFrameIo;
use embedded_can_interface::Id;
use embedded_can_mock::MockFrame;
use tokio::sync::{Mutex, mpsc};

#[derive(Debug)]
enum TokioCanError {
    Closed,
}

#[derive(Clone)]
struct LoggedFrame {
    at: Instant,
    frame: MockFrame,
}

struct TokioBus {
    peers: Mutex<Vec<mpsc::UnboundedSender<MockFrame>>>,
    log: Mutex<Vec<LoggedFrame>>,
}

impl TokioBus {
    fn new() -> Self {
        Self {
            peers: Mutex::new(Vec::new()),
            log: Mutex::new(Vec::new()),
        }
    }

    async fn add_interface(self: &Arc<Self>) -> (TokioTx, TokioRx) {
        let (tx, rx) = mpsc::unbounded_channel();
        self.peers.lock().await.push(tx);
        (TokioTx { bus: self.clone() }, TokioRx { rx })
    }

    async fn transmit(&self, frame: MockFrame) {
        self.log.lock().await.push(LoggedFrame {
            at: Instant::now(),
            frame: frame.clone(),
        });
        let peers = self.peers.lock().await;
        for peer in peers.iter() {
            let _ = peer.send(frame.clone());
        }
    }

    async fn logged_frames(&self) -> Vec<LoggedFrame> {
        self.log.lock().await.clone()
    }
}

struct TokioTx {
    bus: Arc<TokioBus>,
}

struct TokioRx {
    rx: mpsc::UnboundedReceiver<MockFrame>,
}

impl embedded_can_interface::AsyncTxFrameIo for TokioTx {
    type Frame = MockFrame;
    type Error = TokioCanError;

    async fn send(&mut self, frame: &Self::Frame) -> Result<(), Self::Error> {
        self.bus.transmit(frame.clone()).await;
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

impl embedded_can_interface::AsyncRxFrameIo for TokioRx {
    type Frame = MockFrame;
    type Error = TokioCanError;

    async fn recv(&mut self) -> Result<Self::Frame, Self::Error> {
        self.rx.recv().await.ok_or(TokioCanError::Closed)
    }

    async fn recv_timeout(&mut self, _timeout: Duration) -> Result<Self::Frame, Self::Error> {
        self.recv().await
    }

    async fn wait_not_empty(&mut self) -> Result<(), Self::Error> {
        Ok(())
    }
}

struct TokioRt;

impl AsyncRuntime for TokioRt {
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

fn cfg_pair(a_to_b: u16, b_to_a: u16) -> (IsoTpConfig, IsoTpConfig) {
    (
        IsoTpConfig {
            tx_id: Id::Standard(embedded_can::StandardId::new(a_to_b).unwrap()),
            rx_id: Id::Standard(embedded_can::StandardId::new(b_to_a).unwrap()),
            max_payload_len: 256,
            ..IsoTpConfig::default()
        },
        IsoTpConfig {
            tx_id: Id::Standard(embedded_can::StandardId::new(b_to_a).unwrap()),
            rx_id: Id::Standard(embedded_can::StandardId::new(a_to_b).unwrap()),
            max_payload_len: 256,
            ..IsoTpConfig::default()
        },
    )
}

fn to_can_id(id: Id) -> embedded_can::Id {
    match id {
        Id::Standard(std_id) => embedded_can::Id::Standard(std_id),
        Id::Extended(ext_id) => embedded_can::Id::Extended(ext_id),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn async_single_frame_roundtrip() {
    let bus = Arc::new(TokioBus::new());
    let (tx_a, rx_a) = bus.add_interface().await;
    let (tx_b, rx_b) = bus.add_interface().await;

    let (cfg_a, cfg_b) = cfg_pair(0x123, 0x321);
    let mut node_a: IsoTpAsyncNode<'static, _, _, _, _> = IsoTpAsyncNode::with_clock_and_storage(
        tx_a,
        rx_a,
        cfg_a,
        StdClock,
        RxStorage::Owned(vec![0u8; 256]),
    )
    .unwrap();
    let mut node_b: IsoTpAsyncNode<'static, _, _, _, _> = IsoTpAsyncNode::with_clock_and_storage(
        tx_b,
        rx_b,
        cfg_b,
        StdClock,
        RxStorage::Owned(vec![0u8; 256]),
    )
    .unwrap();

    let rt = TokioRt;
    let payload = b"hello";

    let recv_fut = async {
        let mut delivered = Vec::new();
        node_b
            .recv(&rt, Duration::from_millis(200), &mut |data| {
                delivered = data.to_vec()
            })
            .await
            .unwrap();
        delivered
    };
    let send_fut = async {
        node_a
            .send(&rt, payload, Duration::from_millis(200))
            .await
            .unwrap()
    };

    let (delivered, ()) = tokio::join!(recv_fut, send_fut);
    assert_eq!(delivered, payload);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn async_multi_frame_roundtrip() {
    let bus = Arc::new(TokioBus::new());
    let (tx_a, rx_a) = bus.add_interface().await;
    let (tx_b, rx_b) = bus.add_interface().await;

    let (cfg_a, cfg_b) = cfg_pair(0x700, 0x701);
    let mut node_a: IsoTpAsyncNode<'static, _, _, _, _> = IsoTpAsyncNode::with_clock_and_storage(
        tx_a,
        rx_a,
        cfg_a,
        StdClock,
        RxStorage::Owned(vec![0u8; 256]),
    )
    .unwrap();
    let mut node_b: IsoTpAsyncNode<'static, _, _, _, _> = IsoTpAsyncNode::with_clock_and_storage(
        tx_b,
        rx_b,
        cfg_b,
        StdClock,
        RxStorage::Owned(vec![0u8; 256]),
    )
    .unwrap();

    let rt = TokioRt;
    let payload: Vec<u8> = (0..100).collect();

    let recv_fut = async {
        let mut delivered = Vec::new();
        node_b
            .recv(&rt, Duration::from_millis(500), &mut |data| {
                delivered = data.to_vec()
            })
            .await
            .unwrap();
        delivered
    };
    let send_fut = async {
        node_a
            .send(&rt, &payload, Duration::from_millis(500))
            .await
            .unwrap()
    };

    let (delivered, ()) = tokio::join!(recv_fut, send_fut);
    assert_eq!(delivered, payload);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn async_block_size_requires_multiple_flow_controls() {
    let bus = Arc::new(TokioBus::new());
    let (tx_a, rx_a) = bus.add_interface().await;
    let (tx_b, rx_b) = bus.add_interface().await;

    let (cfg_a, mut cfg_b) = cfg_pair(0x600, 0x601);
    cfg_b.block_size = 1;
    let fc_id = to_can_id(cfg_b.tx_id);

    let mut node_a: IsoTpAsyncNode<'static, _, _, _, _> = IsoTpAsyncNode::with_clock_and_storage(
        tx_a,
        rx_a,
        cfg_a,
        StdClock,
        RxStorage::Owned(vec![0u8; 256]),
    )
    .unwrap();
    let mut node_b: IsoTpAsyncNode<'static, _, _, _, _> = IsoTpAsyncNode::with_clock_and_storage(
        tx_b,
        rx_b,
        cfg_b,
        StdClock,
        RxStorage::Owned(vec![0u8; 256]),
    )
    .unwrap();

    let rt = TokioRt;
    let payload: Vec<u8> = (0..25).collect();

    let recv_fut = async {
        let mut delivered = Vec::new();
        node_b
            .recv(&rt, Duration::from_millis(800), &mut |data| {
                delivered = data.to_vec()
            })
            .await
            .unwrap();
        delivered
    };
    let send_fut = async {
        node_a
            .send(&rt, &payload, Duration::from_millis(800))
            .await
            .unwrap()
    };

    let (delivered, ()) = tokio::join!(recv_fut, send_fut);
    assert_eq!(delivered, payload);

    let log = bus.logged_frames().await;
    let mut fc_count = 0usize;
    for entry in log.iter() {
        if entry.frame.id() != fc_id {
            continue;
        }
        if let Ok(Pdu::FlowControl { .. }) = decode(entry.frame.data()) {
            fc_count += 1;
        }
    }
    assert_eq!(fc_count, 3, "expected initial FC + 2 mid-block FCs");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn async_st_min_enforced_between_consecutive_frames() {
    let bus = Arc::new(TokioBus::new());
    let (tx_a, rx_a) = bus.add_interface().await;
    let (tx_b, rx_b) = bus.add_interface().await;

    let (cfg_a, mut cfg_b) = cfg_pair(0x610, 0x611);
    let tx_id_a = to_can_id(cfg_a.tx_id);
    let st_min = Duration::from_millis(100);
    cfg_b.st_min = st_min;

    let mut node_a: IsoTpAsyncNode<'static, _, _, _, _> = IsoTpAsyncNode::with_clock_and_storage(
        tx_a,
        rx_a,
        cfg_a,
        StdClock,
        RxStorage::Owned(vec![0u8; 256]),
    )
    .unwrap();
    let mut node_b: IsoTpAsyncNode<'static, _, _, _, _> = IsoTpAsyncNode::with_clock_and_storage(
        tx_b,
        rx_b,
        cfg_b,
        StdClock,
        RxStorage::Owned(vec![0u8; 256]),
    )
    .unwrap();

    let rt = TokioRt;
    let payload: Vec<u8> = (0..20).collect();

    let recv_fut = async {
        let mut delivered = Vec::new();
        node_b
            .recv(&rt, Duration::from_secs(2), &mut |data| {
                delivered = data.to_vec()
            })
            .await
            .unwrap();
        delivered
    };
    let send_fut = async {
        node_a
            .send(&rt, &payload, Duration::from_secs(2))
            .await
            .unwrap()
    };

    let (delivered, ()) = tokio::join!(recv_fut, send_fut);
    assert_eq!(delivered, payload);

    let log = bus.logged_frames().await;
    let mut consecutive_times = Vec::new();
    for entry in log.iter() {
        if entry.frame.id() != tx_id_a {
            continue;
        }
        if let Ok(Pdu::ConsecutiveFrame { .. }) = decode(entry.frame.data()) {
            consecutive_times.push(entry.at);
        }
    }
    assert_eq!(
        consecutive_times.len(),
        2,
        "expected exactly 2 CFs for 20-byte payload"
    );

    let delta = consecutive_times[1].duration_since(consecutive_times[0]);
    let min_expected = st_min
        .checked_sub(Duration::from_millis(10))
        .unwrap_or_default();
    assert!(
        delta >= min_expected,
        "CF pacing too fast: delta={:?} st_min={:?}",
        delta,
        st_min
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn async_send_times_out_waiting_for_flow_control_nbs() {
    let bus = Arc::new(TokioBus::new());
    let (tx_a, rx_a) = bus.add_interface().await;

    let (mut cfg_a, _cfg_b) = cfg_pair(0x620, 0x621);
    cfg_a.n_bs = Duration::from_millis(120);

    let mut node_a: IsoTpAsyncNode<'static, _, _, _, _> = IsoTpAsyncNode::with_clock_and_storage(
        tx_a,
        rx_a,
        cfg_a,
        StdClock,
        RxStorage::Owned(vec![0u8; 256]),
    )
    .unwrap();
    let rt = TokioRt;
    let payload: Vec<u8> = (0..24).collect();

    let err = node_a
        .send(&rt, &payload, Duration::from_millis(800))
        .await
        .unwrap_err();
    assert!(matches!(
        err,
        can_iso_tp::IsoTpError::Timeout(can_iso_tp::TimeoutKind::NBs)
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn async_send_times_out_waiting_for_flow_control_nas() {
    let bus = Arc::new(TokioBus::new());
    let (tx_a, rx_a) = bus.add_interface().await;

    let (mut cfg_a, _cfg_b) = cfg_pair(0x630, 0x631);
    cfg_a.n_bs = Duration::from_millis(800);

    let mut node_a: IsoTpAsyncNode<'static, _, _, _, _> = IsoTpAsyncNode::with_clock_and_storage(
        tx_a,
        rx_a,
        cfg_a,
        StdClock,
        RxStorage::Owned(vec![0u8; 256]),
    )
    .unwrap();
    let rt = TokioRt;
    let payload: Vec<u8> = (0..24).collect();

    let err = node_a
        .send(&rt, &payload, Duration::from_millis(120))
        .await
        .unwrap_err();
    assert!(matches!(
        err,
        can_iso_tp::IsoTpError::Timeout(can_iso_tp::TimeoutKind::NAs)
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn async_recv_times_out_nar_even_with_noise_and_flow_control_frames() {
    let bus = Arc::new(TokioBus::new());
    let (tx_a, rx_a) = bus.add_interface().await;
    let (mut noise_tx, _noise_rx) = bus.add_interface().await;

    let (cfg_a, _cfg_b) = cfg_pair(0x640, 0x641);
    let mut node_a: IsoTpAsyncNode<'static, _, _, _, _> = IsoTpAsyncNode::with_clock_and_storage(
        tx_a,
        rx_a,
        cfg_a.clone(),
        StdClock,
        RxStorage::Owned(vec![0u8; 256]),
    )
    .unwrap();

    let noise_frame = MockFrame::new(
        embedded_can::Id::Standard(embedded_can::StandardId::new(0x7AA).unwrap()),
        &[0xDE, 0xAD, 0xBE, 0xEF],
    )
    .unwrap();
    noise_tx.send(&noise_frame).await.unwrap();
    noise_tx.send(&noise_frame).await.unwrap();

    let fc: MockFrame = encode(
        cfg_a.rx_id,
        &Pdu::FlowControl {
            status: can_iso_tp::pdu::FlowStatus::ClearToSend,
            block_size: 0,
            st_min: 0,
        },
        None,
    )
    .unwrap();
    noise_tx.send(&fc).await.unwrap();

    let rt = TokioRt;
    let err = node_a
        .recv(&rt, Duration::from_millis(150), &mut |_| {})
        .await
        .unwrap_err();
    assert!(matches!(
        err,
        can_iso_tp::IsoTpError::Timeout(can_iso_tp::TimeoutKind::NAr)
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn async_recv_rejects_bad_sequence_number() {
    let bus = Arc::new(TokioBus::new());
    let (tx_b, rx_b) = bus.add_interface().await;
    let (mut inject_tx, _inject_rx) = bus.add_interface().await;

    let mut cfg_b = IsoTpConfig {
        tx_id: Id::Standard(embedded_can::StandardId::new(0x651).unwrap()),
        rx_id: Id::Standard(embedded_can::StandardId::new(0x650).unwrap()),
        ..IsoTpConfig::default()
    };
    cfg_b.wft_max = 2;
    cfg_b.max_payload_len = 64;

    let mut node_b: IsoTpAsyncNode<'static, _, _, _, _> = IsoTpAsyncNode::with_clock_and_storage(
        tx_b,
        rx_b,
        cfg_b.clone(),
        StdClock,
        RxStorage::Owned(vec![0u8; 64]),
    )
    .unwrap();

    let ff: MockFrame = encode(
        cfg_b.rx_id,
        &Pdu::FirstFrame {
            len: 10,
            data: &[1, 2, 3, 4, 5, 6],
        },
        None,
    )
    .unwrap();
    inject_tx.send(&ff).await.unwrap();

    let cf_bad: MockFrame = encode(
        cfg_b.rx_id,
        &Pdu::ConsecutiveFrame {
            sn: 3,
            data: &[7, 8, 9, 10],
        },
        None,
    )
    .unwrap();
    inject_tx.send(&cf_bad).await.unwrap();

    let rt = TokioRt;
    let err = node_b
        .recv(&rt, Duration::from_millis(300), &mut |_| {})
        .await
        .unwrap_err();
    assert!(matches!(err, can_iso_tp::IsoTpError::BadSequence));
}
