#![cfg(feature = "std")]

use core::time::Duration;

use can_iso_tp::{IsoTpAsyncNode, IsoTpConfig};
use embedded_can::StandardId;
use embedded_can_interface::Id;
use embedded_can_interface::SplitTxRx;
use embedded_can_mock::{BusHandle, MockCan};

#[derive(Clone, Copy)]
struct TokioRuntime;

impl can_iso_tp::AsyncRuntime for TokioRuntime {
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

#[derive(Clone, Copy, Debug, Default)]
struct TokioClock;

impl can_iso_tp::Clock for TokioClock {
    type Instant = tokio::time::Instant;

    fn now(&self) -> Self::Instant {
        tokio::time::Instant::now()
    }

    fn elapsed(&self, earlier: Self::Instant) -> Duration {
        earlier.elapsed()
    }

    fn add(&self, instant: Self::Instant, dur: Duration) -> Self::Instant {
        instant + dur
    }
}

thincan::bus_atlas! {
    pub mod atlas {
        0x0100 => A(len = 1);
        0x0101 => B(len = 1);
    }
}

thincan::bundle! {
    pub mod none(atlas) {
        parser: thincan::DefaultParser;
        use msgs [];
        handles {}
        items {}
    }
}

thincan::bus_maplet! {
    pub mod maplet: atlas {
        bundles [none];
        parser: thincan::DefaultParser;
        use msgs [A, B];
        handles { A => on_a, B => on_b }
        unhandled_by_default = true;
        ignore [];
    }
}

#[derive(Default)]
struct App {
    seen: Vec<u16>,
}

impl<'a> maplet::Handlers<'a> for App {
    type Error = ();

    async fn on_a(
        &mut self,
        _meta: thincan::RecvMeta<maplet::ReplyTo>,
        msg: &'a [u8; atlas::A::BODY_LEN],
    ) -> Result<(), Self::Error> {
        assert_eq!(*msg, [0xAA]);
        self.seen.push(<atlas::A as thincan::Message>::ID);
        Ok(())
    }

    async fn on_b(
        &mut self,
        _meta: thincan::RecvMeta<maplet::ReplyTo>,
        msg: &'a [u8; atlas::B::BODY_LEN],
    ) -> Result<(), Self::Error> {
        assert_eq!(*msg, [0xBB]);
        self.seen.push(<atlas::B as thincan::Message>::ID);
        Ok(())
    }
}

fn cfg(tx: u16, rx: u16) -> IsoTpConfig {
    IsoTpConfig {
        tx_id: Id::Standard(StandardId::new(tx).unwrap()),
        rx_id: Id::Standard(StandardId::new(rx).unwrap()),
        max_payload_len: 64,
        ..IsoTpConfig::default()
    }
}

#[tokio::test]
async fn async_iso_tp_recv_one_dispatch_roundtrips() -> Result<(), thincan::Error> {
    let bus = BusHandle::new();
    let can_a = MockCan::new_with_bus(&bus, vec![]).unwrap();
    let can_b = MockCan::new_with_bus(&bus, vec![]).unwrap();
    let (tx_a, rx_a) = can_a.split();
    let (tx_b, rx_b) = can_b.split();

    let mut rx_buf_a = [0u8; 64];
    let mut rx_buf_b = [0u8; 64];
    let node_a =
        IsoTpAsyncNode::with_clock(tx_a, rx_a, cfg(0x700, 0x701), TokioClock, &mut rx_buf_a)
            .unwrap();
    let node_b =
        IsoTpAsyncNode::with_clock(tx_b, rx_b, cfg(0x701, 0x700), TokioClock, &mut rx_buf_b)
            .unwrap();

    let rt = TokioRuntime;
    let node_a = can_iso_tp::AsyncWithRt::new(&rt, node_a);
    let node_b = can_iso_tp::AsyncWithRt::new(&rt, node_b);

    let mut tx_buf_a = [0u8; 64];
    let mut tx_buf_b = [0u8; 64];
    let mut iface_a = thincan::Interface::new(node_a, maplet::Router::new(), &mut tx_buf_a);
    let mut iface_b = thincan::Interface::new(node_b, maplet::Router::new(), &mut tx_buf_b);

    iface_a
        .send_msg_async::<atlas::A>(&[0xAA], Duration::from_millis(50))
        .await?;
    iface_a
        .send_msg_async::<atlas::B>(&[0xBB], Duration::from_millis(50))
        .await?;

    let mut handlers = maplet::HandlersImpl {
        app: App::default(),
        none: none::DefaultHandlers,
    };
    let mut rx_buf = [0u8; 64];

    assert_eq!(
        iface_b
            .recv_one_dispatch_async(&mut handlers, Duration::from_millis(50), &mut rx_buf)
            .await?,
        thincan::RecvDispatch::Dispatched {
            id: <atlas::A as thincan::Message>::ID,
            kind: thincan::RecvDispatchKind::Handled,
        }
    );
    assert_eq!(
        iface_b
            .recv_one_dispatch_async(&mut handlers, Duration::from_millis(50), &mut rx_buf)
            .await?,
        thincan::RecvDispatch::Dispatched {
            id: <atlas::B as thincan::Message>::ID,
            kind: thincan::RecvDispatchKind::Handled,
        }
    );

    assert_eq!(
        handlers.app.seen,
        vec![
            <atlas::A as thincan::Message>::ID,
            <atlas::B as thincan::Message>::ID
        ]
    );
    Ok(())
}
