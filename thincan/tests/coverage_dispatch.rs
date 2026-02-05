use std::time::Duration;

use thincan::{DispatchOutcome, ErrorKind, Message, RawMessage, RouterDispatch};

thincan::bus_atlas! {
    pub mod atlas {
        0x0100 => Ping(len = 1);
        0x0101 => Pong(len = 2);
        0x0102 => Nop(len = 1);
        0x0103 => Ignored(len = 0);
    }
}

thincan::bundle! {
    pub mod demo_bundle(atlas) {
        parser: thincan::DefaultParser;
        use msgs [Ping, Pong, Nop, Ignored];
        handles { Ping => on_ping }
    }
}

thincan::bus_maplet! {
    pub mod maplet_unhandled: atlas {
        bundles [demo_bundle];
        parser: thincan::DefaultParser;
        use msgs [Ping, Pong, Nop, Ignored];
        handles { Pong => on_pong }
        unhandled_by_default = true;
        ignore [Ignored];
    }
}

thincan::bus_maplet! {
    pub mod maplet_handled: atlas {
        bundles [demo_bundle];
        parser: thincan::DefaultParser;
        use msgs [Ping, Pong, Nop, Ignored];
        handles { Pong => on_pong }
        unhandled_by_default = false;
        ignore [Ignored];
    }
}

#[derive(Default)]
struct AppHandlers {
    fail_pong: bool,
}

impl<'a> maplet_unhandled::Handlers<'a> for AppHandlers {
    type Error = ();

    fn on_pong(&mut self, _msg: &'a [u8; atlas::Pong::BODY_LEN]) -> Result<(), Self::Error> {
        if self.fail_pong {
            return Err(());
        }
        Ok(())
    }
}

impl<'a> maplet_handled::Handlers<'a> for AppHandlers {
    type Error = ();

    fn on_pong(&mut self, _msg: &'a [u8; atlas::Pong::BODY_LEN]) -> Result<(), Self::Error> {
        if self.fail_pong {
            return Err(());
        }
        Ok(())
    }
}

#[derive(Default)]
struct BundleHandlers {
    fail_ping: bool,
}

impl<'a> demo_bundle::Handlers<'a> for BundleHandlers {
    type Error = ();

    fn on_ping(&mut self, _msg: &'a [u8; atlas::Ping::BODY_LEN]) -> Result<(), Self::Error> {
        if self.fail_ping {
            return Err(());
        }
        Ok(())
    }
}

#[test]
fn decode_wire_rejects_payload_without_header() {
    let err = thincan::decode_wire(&[0x01]).unwrap_err();
    assert!(matches!(err.kind, ErrorKind::Other));
}

#[cfg(feature = "std")]
#[test]
fn error_type_formats_via_display() {
    let msg = format!("{}", thincan::Error::timeout());
    assert!(!msg.is_empty());
}

#[test]
fn encode_slice_and_array_reject_too_small_output() {
    struct Msg;
    impl Message for Msg {
        const ID: u16 = 0x0001;
    }

    let bytes: &[u8] = &[1, 2];
    assert_eq!(thincan::Encode::<Msg>::max_encoded_len(&bytes), 2);
    let mut out = [0u8; 1];
    let err = thincan::Encode::<Msg>::encode(&bytes, &mut out).unwrap_err();
    assert!(matches!(err.kind, ErrorKind::Other));

    let bytes: &[u8; 2] = &[3, 4];
    assert_eq!(thincan::Encode::<Msg>::max_encoded_len(&bytes), 2);
    let mut out = [0u8; 1];
    let err = thincan::Encode::<Msg>::encode(&bytes, &mut out).unwrap_err();
    assert!(matches!(err.kind, ErrorKind::Other));
}

#[test]
fn encode_slice_and_array_success_paths_are_exercised() {
    struct Msg;
    impl Message for Msg {
        const ID: u16 = 0x0002;
    }

    let bytes: &[u8] = &[9, 8];
    let mut out = [0u8; 2];
    let n = thincan::Encode::<Msg>::encode(&bytes, &mut out).unwrap();
    assert_eq!(n, 2);
    assert_eq!(out, [9, 8]);

    let bytes: &[u8; 2] = &[7, 6];
    let mut out = [0u8; 2];
    let n = thincan::Encode::<Msg>::encode(&bytes, &mut out).unwrap();
    assert_eq!(n, 2);
    assert_eq!(out, [7, 6]);
}

#[test]
#[should_panic(expected = "duplicate handled message id")]
fn assert_unique_u16_slices_panics_on_duplicates() {
    thincan::__assert_unique_u16_slices([&[1, 2], &[2]]);
}

#[test]
fn assert_unique_u16_slices_accepts_unique_slices() {
    thincan::__assert_unique_u16_slices([&[1, 2], &[3]]);
}

#[test]
fn dispatch_outcomes_and_error_paths_are_exercised() {
    // Bundle-handled Ping.
    {
        let mut router = maplet_unhandled::Router::new();
        let mut handlers = maplet_unhandled::HandlersImpl {
            app: AppHandlers::default(),
            demo_bundle: BundleHandlers::default(),
        };

        let msg = RawMessage {
            id: <atlas::Ping as Message>::ID,
            body: &[0x01],
        };
        assert_eq!(
            router.dispatch(&mut handlers, msg).unwrap(),
            DispatchOutcome::Handled
        );
    }

    // Bundle DefaultHandlers path.
    {
        let mut router = maplet_unhandled::Router::new();
        let mut handlers = maplet_unhandled::HandlersImpl {
            app: AppHandlers::default(),
            demo_bundle: demo_bundle::DefaultHandlers,
        };

        let msg = RawMessage {
            id: <atlas::Ping as Message>::ID,
            body: &[0x01],
        };
        assert_eq!(
            router.dispatch(&mut handlers, msg).unwrap(),
            DispatchOutcome::Handled
        );
    }

    // App-handled Pong.
    {
        let mut router = maplet_unhandled::Router::new();
        let mut handlers = maplet_unhandled::HandlersImpl {
            app: AppHandlers::default(),
            demo_bundle: BundleHandlers::default(),
        };

        let msg = RawMessage {
            id: <atlas::Pong as Message>::ID,
            body: &[0x01, 0x02],
        };
        assert_eq!(
            router.dispatch(&mut handlers, msg).unwrap(),
            DispatchOutcome::Handled
        );
    }

    // Ignored id is treated as handled.
    {
        let mut router = maplet_unhandled::Router::new();
        let mut handlers = maplet_unhandled::HandlersImpl {
            app: AppHandlers::default(),
            demo_bundle: BundleHandlers::default(),
        };

        let msg = RawMessage {
            id: <atlas::Ignored as Message>::ID,
            body: &[],
        };
        assert_eq!(
            router.dispatch(&mut handlers, msg).unwrap(),
            DispatchOutcome::Handled
        );
    }

    // Known but unhandled: Unhandled if configured.
    {
        let mut router = maplet_unhandled::Router::new();
        let mut handlers = maplet_unhandled::HandlersImpl {
            app: AppHandlers::default(),
            demo_bundle: BundleHandlers::default(),
        };

        let msg = RawMessage {
            id: <atlas::Nop as Message>::ID,
            body: &[0x00],
        };
        assert!(matches!(
            router.dispatch(&mut handlers, msg).unwrap(),
            DispatchOutcome::Unhandled(_)
        ));
    }

    // Known but unhandled: treated as handled if configured.
    {
        let mut router = maplet_handled::Router::new();
        let mut handlers = maplet_handled::HandlersImpl {
            app: AppHandlers::default(),
            demo_bundle: BundleHandlers::default(),
        };

        let msg = RawMessage {
            id: <atlas::Nop as Message>::ID,
            body: &[0x00],
        };
        assert_eq!(
            router.dispatch(&mut handlers, msg).unwrap(),
            DispatchOutcome::Handled
        );
    }

    // Unknown id.
    {
        let mut router = maplet_unhandled::Router::new();
        let mut handlers = maplet_unhandled::HandlersImpl {
            app: AppHandlers::default(),
            demo_bundle: BundleHandlers::default(),
        };

        let msg = RawMessage {
            id: 0xFFFF,
            body: &[1, 2, 3],
        };
        assert!(matches!(
            router.dispatch(&mut handlers, msg).unwrap(),
            DispatchOutcome::Unknown { .. }
        ));
    }

    // Parse error (fixed-len mismatch) propagates out.
    {
        let mut router = maplet_unhandled::Router::new();
        let mut handlers = maplet_unhandled::HandlersImpl {
            app: AppHandlers::default(),
            demo_bundle: BundleHandlers::default(),
        };
        let msg = RawMessage {
            id: <atlas::Ping as Message>::ID,
            body: &[],
        };
        let err = router.dispatch(&mut handlers, msg).unwrap_err();
        assert!(matches!(err.kind, ErrorKind::Other));
    }

    // Handler error is mapped to ErrorKind::Other.
    {
        let mut router = maplet_unhandled::Router::new();
        let mut handlers = maplet_unhandled::HandlersImpl {
            app: AppHandlers::default(),
            demo_bundle: BundleHandlers { fail_ping: true },
        };
        let msg = RawMessage {
            id: <atlas::Ping as Message>::ID,
            body: &[0x01],
        };
        let err = router.dispatch(&mut handlers, msg).unwrap_err();
        assert!(matches!(err.kind, ErrorKind::Other));
    }

    // App handler error is mapped to ErrorKind::Other.
    {
        let mut router = maplet_unhandled::Router::new();
        let mut handlers = maplet_unhandled::HandlersImpl {
            app: AppHandlers { fail_pong: true },
            demo_bundle: BundleHandlers::default(),
        };
        let msg = RawMessage {
            id: <atlas::Pong as Message>::ID,
            body: &[0x01, 0x02],
        };
        let err = router.dispatch(&mut handlers, msg).unwrap_err();
        assert!(matches!(err.kind, ErrorKind::Other));
    }
}

#[cfg(feature = "std")]
#[test]
fn interface_send_encoded_rejects_encode_overflowing_max_len() {
    #[derive(Clone, Copy)]
    struct Msg;
    impl Message for Msg {
        const ID: u16 = 0x9999;
        const BODY_LEN: Option<usize> = None;
    }

    struct LyingValue;
    impl thincan::Encode<Msg> for LyingValue {
        fn max_encoded_len(&self) -> usize {
            1
        }
        fn encode(&self, _out: &mut [u8]) -> Result<usize, thincan::Error> {
            Ok(2)
        }
    }

    struct Sink;
    impl thincan::Transport for Sink {
        fn send(&mut self, _payload: &[u8], _timeout: Duration) -> Result<(), thincan::Error> {
            Ok(())
        }
        fn recv_one<F>(
            &mut self,
            _timeout: Duration,
            _on_payload: F,
        ) -> Result<thincan::RecvStatus, thincan::Error>
        where
            F: FnMut(&[u8]) -> Result<thincan::RecvControl, thincan::Error>,
        {
            Ok(thincan::RecvStatus::TimedOut)
        }
    }

    let mut tx = [0u8; 64];
    let mut iface = thincan::Interface::new(Sink, maplet_unhandled::Router::new(), &mut tx);
    let err = iface
        .send_encoded::<Msg, _>(&LyingValue, Duration::from_millis(1))
        .unwrap_err();
    assert!(matches!(err.kind, ErrorKind::Other));

    struct WrongLen;
    impl thincan::Encode<atlas::Ping> for WrongLen {
        fn max_encoded_len(&self) -> usize {
            1
        }
        fn encode(&self, _out: &mut [u8]) -> Result<usize, thincan::Error> {
            Ok(0)
        }
    }

    let err = iface
        .send_encoded::<atlas::Ping, _>(&WrongLen, Duration::from_millis(1))
        .unwrap_err();
    assert!(matches!(err.kind, ErrorKind::InvalidBodyLen { .. }));
}

#[cfg(feature = "std")]
#[test]
fn can_iso_tp_node_transport_impl_maps_timeout_and_other_errors() {
    use can_iso_tp::{IsoTpConfig, IsoTpNode};
    use embedded_can::StandardId;
    use embedded_can_interface::Id;
    use embedded_can_interface::SplitTxRx;
    use embedded_can_mock::{BusHandle, MockCan};
    use std::cell::Cell;

    #[derive(Default)]
    struct TestClock {
        now_ms: Cell<u64>,
    }

    impl can_iso_tp::Clock for TestClock {
        type Instant = u64;
        fn now(&self) -> Self::Instant {
            let next = self.now_ms.get().saturating_add(1);
            self.now_ms.set(next);
            next
        }
        fn elapsed(&self, earlier: Self::Instant) -> Duration {
            let now = self.now_ms.get();
            Duration::from_millis(now.saturating_sub(earlier))
        }
        fn add(&self, instant: Self::Instant, dur: Duration) -> Self::Instant {
            instant.saturating_add(dur.as_millis() as u64)
        }
    }

    let bus = BusHandle::new();
    let can_a = MockCan::new_with_bus(&bus, vec![]).unwrap();
    let (tx_a, rx_a) = can_a.split();

    let cfg_other = IsoTpConfig {
        tx_id: Id::Standard(StandardId::new(0x700).unwrap()),
        rx_id: Id::Standard(StandardId::new(0x701).unwrap()),
        max_payload_len: 1,
        n_ar: Duration::from_millis(1),
        n_as: Duration::from_millis(1),
        n_br: Duration::from_millis(1),
        n_bs: Duration::from_millis(1),
        n_cs: Duration::from_millis(1),
        ..IsoTpConfig::default()
    };
    let mut rx_buf_a = [0u8; 64];
    let mut node =
        IsoTpNode::with_clock(tx_a, rx_a, cfg_other, TestClock::default(), &mut rx_buf_a).unwrap();

    // Non-timeout error from ISO-TP maps to ErrorKind::Other.
    let err =
        thincan::Transport::send(&mut node, &[0x01, 0x02], Duration::from_millis(10)).unwrap_err();
    assert!(matches!(err.kind, ErrorKind::Other));

    // Receive timeout maps to a TimedOut status (not an error).
    let st = thincan::Transport::recv_one(&mut node, Duration::from_millis(1), |_| {
        Ok(thincan::RecvControl::Continue)
    })
    .unwrap();
    assert_eq!(st, thincan::RecvStatus::TimedOut);

    // Send timeout from ISO-TP maps to ErrorKind::Timeout (multi-frame requires flow control).
    let can_b = MockCan::new_with_bus(&bus, vec![]).unwrap();
    let (tx_b, rx_b) = can_b.split();
    let cfg_timeout = IsoTpConfig {
        tx_id: Id::Standard(StandardId::new(0x710).unwrap()),
        rx_id: Id::Standard(StandardId::new(0x711).unwrap()),
        max_payload_len: 64,
        n_ar: Duration::from_millis(1),
        n_as: Duration::from_millis(1),
        n_br: Duration::from_millis(1),
        n_bs: Duration::from_millis(1),
        n_cs: Duration::from_millis(1),
        ..IsoTpConfig::default()
    };
    let mut rx_buf_b = [0u8; 256];
    let mut node2 =
        IsoTpNode::with_clock(tx_b, rx_b, cfg_timeout, TestClock::default(), &mut rx_buf_b)
            .unwrap();
    let payload = [0u8; 8];
    let err = thincan::Transport::send(&mut node2, &payload, Duration::from_millis(1)).unwrap_err();
    assert!(matches!(err.kind, ErrorKind::Timeout));
}

#[cfg(feature = "async")]
#[tokio::test]
async fn async_helpers_validate_before_sending() {
    use can_iso_tp::{IsoTpAsyncNode, IsoTpConfig};
    use embedded_can::StandardId;
    use embedded_can_interface::Id;
    use embedded_can_interface::SplitTxRx;
    use embedded_can_mock::{BusHandle, MockCan};
    use std::cell::Cell;

    #[derive(Default)]
    struct TestClock {
        now_ms: Cell<u64>,
    }

    impl can_iso_tp::Clock for TestClock {
        type Instant = u64;
        fn now(&self) -> Self::Instant {
            let next = self.now_ms.get().saturating_add(1);
            self.now_ms.set(next);
            next
        }
        fn elapsed(&self, earlier: Self::Instant) -> Duration {
            let now = self.now_ms.get();
            Duration::from_millis(now.saturating_sub(earlier))
        }
        fn add(&self, instant: Self::Instant, dur: Duration) -> Self::Instant {
            instant.saturating_add(dur.as_millis() as u64)
        }
    }

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

    let bus = BusHandle::new();
    let can = MockCan::new_with_bus(&bus, vec![]).unwrap();
    let (tx, rx) = can.split();

    let cfg = IsoTpConfig {
        tx_id: Id::Standard(StandardId::new(0x700).unwrap()),
        rx_id: Id::Standard(StandardId::new(0x701).unwrap()),
        max_payload_len: 64,
        ..IsoTpConfig::default()
    };

    let mut rx_buf = [0u8; 128];
    let node = IsoTpAsyncNode::with_clock(tx, rx, cfg, TestClock::default(), &mut rx_buf).unwrap();

    let mut tx_buf = [0u8; 64];
    let mut iface = thincan::Interface::new(node, maplet_unhandled::Router::new(), &mut tx_buf);
    let rt = TokioRuntime;

    // send_msg length mismatch (validated before touching the transport).
    let err = iface
        .send_msg_async::<TokioRuntime, atlas::Ping>(&rt, &[], Duration::from_millis(1))
        .await
        .unwrap_err();
    assert!(matches!(err.kind, ErrorKind::InvalidBodyLen { .. }));

    // send_encoded used > max_encoded_len (validated before touching the transport).
    struct Lying;
    impl thincan::Encode<atlas::Ping> for Lying {
        fn max_encoded_len(&self) -> usize {
            1
        }
        fn encode(&self, _out: &mut [u8]) -> Result<usize, thincan::Error> {
            Ok(2)
        }
    }
    let err = iface
        .send_encoded_async::<TokioRuntime, atlas::Ping, _>(&rt, &Lying, Duration::from_millis(1))
        .await
        .unwrap_err();
    assert!(matches!(err.kind, ErrorKind::Other));
}
