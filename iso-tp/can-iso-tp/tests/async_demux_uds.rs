#![cfg(feature = "uds")]

use core::time::Duration;
use std::sync::Arc;
use std::time::Instant;

use can_iso_tp::{AsyncRuntime, IsoTpAsyncDemux, IsoTpAsyncNode, IsoTpConfig, RxStorage, StdClock};
use embedded_can::Frame as _;
use embedded_can_interface::{AsyncTxFrameIo as _, Id};
use embedded_can_mock::MockFrame;
use tokio::sync::{Mutex, mpsc};

#[derive(Debug)]
enum TokioCanError {
    Closed,
}

#[derive(Clone)]
struct LoggedFrame {
    #[allow(dead_code)]
    at: Instant,
    #[allow(dead_code)]
    frame: MockFrame,
}

struct TokioBus {
    peers: Mutex<Vec<mpsc::UnboundedSender<MockFrame>>>,
    #[allow(dead_code)]
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

fn base_cfg(max_payload_len: usize) -> IsoTpConfig {
    IsoTpConfig {
        tx_id: Id::Extended(embedded_can::ExtendedId::new(1).unwrap()),
        rx_id: Id::Extended(embedded_can::ExtendedId::new(2).unwrap()),
        max_payload_len,
        block_size: 2,
        ..IsoTpConfig::default()
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn async_demux_receives_two_sources_without_mixing() {
    const MAX_PEERS: usize = 4;
    let bus = Arc::new(TokioBus::new());
    let (rx_tx, rx_rx) = bus.add_interface().await;
    let (s1_tx, s1_rx) = bus.add_interface().await;
    let (s2_tx, s2_rx) = bus.add_interface().await;

    let local = 0x10u8;
    let sa1 = 0xA1u8;
    let sa2 = 0xA2u8;

    let storages: [RxStorage<'static>; MAX_PEERS] =
        core::array::from_fn(|_| RxStorage::Owned(vec![0u8; 256]));
    let mut demux = IsoTpAsyncDemux::<_, _, _, _, MAX_PEERS>::with_std_clock(
        rx_tx,
        rx_rx,
        base_cfg(256),
        local,
        storages,
    )
    .unwrap();

    let cfg1 = IsoTpConfig {
        tx_id: can_uds::uds29::encode_phys_id(local, sa1),
        rx_id: can_uds::uds29::encode_phys_id(sa1, local),
        ..base_cfg(256)
    };
    let cfg2 = IsoTpConfig {
        tx_id: can_uds::uds29::encode_phys_id(local, sa2),
        rx_id: can_uds::uds29::encode_phys_id(sa2, local),
        ..base_cfg(256)
    };

    let mut node1: IsoTpAsyncNode<'static, _, _, _, _> = IsoTpAsyncNode::with_clock_and_storage(
        s1_tx,
        s1_rx,
        cfg1,
        StdClock,
        RxStorage::Owned(vec![0u8; 256]),
    )
    .unwrap();
    let mut node2: IsoTpAsyncNode<'static, _, _, _, _> = IsoTpAsyncNode::with_clock_and_storage(
        s2_tx,
        s2_rx,
        cfg2,
        StdClock,
        RxStorage::Owned(vec![0u8; 256]),
    )
    .unwrap();

    let p1: Vec<u8> = (0..200).map(|i| (i as u8).wrapping_add(1)).collect();
    let p2: Vec<u8> = (0..200).map(|i| (i as u8).wrapping_add(101)).collect();

    let rt = TokioRt;

    let recv_fut = async {
        let mut got: Vec<(u8, Vec<u8>)> = Vec::new();
        for _ in 0..2 {
            demux
                .recv(&rt, Duration::from_millis(500), &mut |reply_to, payload| {
                    got.push((reply_to, payload.to_vec()));
                })
                .await
                .unwrap();
        }
        got
    };

    let send1 = node1.send(&rt, &p1, Duration::from_millis(500));
    let send2 = node2.send(&rt, &p2, Duration::from_millis(500));

    let (got, _, _) = tokio::join!(recv_fut, send1, send2);

    assert_eq!(got.len(), 2);
    for (reply_to, payload) in got {
        if reply_to == sa1 {
            assert_eq!(payload, p1);
        } else if reply_to == sa2 {
            assert_eq!(payload, p2);
        } else {
            panic!("unexpected reply_to {reply_to:#x}");
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn async_demux_receives_functional_single_frame() {
    const MAX_PEERS: usize = 2;
    let bus = Arc::new(TokioBus::new());
    let (rx_tx, rx_rx) = bus.add_interface().await;
    let (mut s_tx, _s_rx) = bus.add_interface().await;

    let local_phys = 0x10u8;
    let local_func = 0x33u8;
    let tester = 0xF1u8;

    let storages: [RxStorage<'static>; MAX_PEERS] =
        core::array::from_fn(|_| RxStorage::Owned(vec![0u8; 64]));
    let mut demux = IsoTpAsyncDemux::<_, _, _, _, MAX_PEERS>::with_std_clock(
        rx_tx,
        rx_rx,
        base_cfg(64),
        local_phys,
        storages,
    )
    .unwrap()
    .with_functional_addr(local_func);

    let payload = b"ping";
    let mut data = [0u8; 8];
    data[0] = payload.len() as u8;
    data[1..1 + payload.len()].copy_from_slice(payload);

    let can_id = can_uds::uds29::encode_func_id_raw(local_func, tester);
    let frame = MockFrame::new(
        embedded_can::Id::Extended(embedded_can::ExtendedId::new(can_id).unwrap()),
        &data[..1 + payload.len()],
    )
    .unwrap();
    s_tx.send(&frame).await.unwrap();

    let rt = TokioRt;
    let mut got: Option<(u8, Vec<u8>)> = None;
    demux
        .recv(&rt, Duration::from_millis(200), &mut |reply_to, payload| {
            got = Some((reply_to, payload.to_vec()));
        })
        .await
        .unwrap();
    let (reply_to, bytes) = got.expect("expected a delivered payload");
    assert_eq!(reply_to, tester);
    assert_eq!(bytes, payload);
}
