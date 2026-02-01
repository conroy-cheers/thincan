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
        fn recv(
            &mut self,
            _timeout: Duration,
            _deliver: &mut dyn FnMut(&[u8]),
        ) -> Result<(), thincan::Error> {
            Err(thincan::Error::timeout())
        }
    }

    let mut iface = thincan::Interface::new(Sink, maplet_unhandled::Router::new());
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

    let bus = BusHandle::new();
    let can_a = MockCan::new_with_bus(&bus, vec![]).unwrap();
    let (tx_a, rx_a) = can_a.split();

    let cfg_other = IsoTpConfig {
        tx_id: Id::Standard(StandardId::new(0x700).unwrap()),
        rx_id: Id::Standard(StandardId::new(0x701).unwrap()),
        max_payload_len: 1,
        rx_buffer_len: 8,
        n_ar: Duration::from_millis(1),
        n_as: Duration::from_millis(1),
        n_br: Duration::from_millis(1),
        n_bs: Duration::from_millis(1),
        n_cs: Duration::from_millis(1),
        ..IsoTpConfig::default()
    };
    let mut node = IsoTpNode::with_std_clock(tx_a, rx_a, cfg_other).unwrap();

    // Non-timeout error from ISO-TP maps to ErrorKind::Other.
    let err =
        thincan::Transport::send(&mut node, &[0x01, 0x02], Duration::from_millis(1)).unwrap_err();
    assert!(matches!(err.kind, ErrorKind::Other));

    // Timeout from ISO-TP maps to ErrorKind::Timeout.
    let err =
        thincan::Transport::recv(&mut node, Duration::from_millis(1), &mut |_| {}).unwrap_err();
    assert!(matches!(err.kind, ErrorKind::Timeout));

    // Send timeout from ISO-TP maps to ErrorKind::Timeout (multi-frame requires flow control).
    let can_b = MockCan::new_with_bus(&bus, vec![]).unwrap();
    let (tx_b, rx_b) = can_b.split();
    let cfg_timeout = IsoTpConfig {
        tx_id: Id::Standard(StandardId::new(0x710).unwrap()),
        rx_id: Id::Standard(StandardId::new(0x711).unwrap()),
        max_payload_len: 64,
        rx_buffer_len: 64,
        n_ar: Duration::from_millis(1),
        n_as: Duration::from_millis(1),
        n_br: Duration::from_millis(1),
        n_bs: Duration::from_millis(1),
        n_cs: Duration::from_millis(1),
        ..IsoTpConfig::default()
    };
    let mut node2 = IsoTpNode::with_std_clock(tx_b, rx_b, cfg_timeout).unwrap();
    let payload = [0u8; 8];
    let err = thincan::Transport::send(&mut node2, &payload, Duration::from_millis(1)).unwrap_err();
    assert!(matches!(err.kind, ErrorKind::Timeout));
}

#[cfg(feature = "async")]
#[tokio::test]
async fn async_interface_validates_body_len_and_encode_limits() {
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};

    #[derive(Clone)]
    struct AsyncNode {
        sent: Arc<Mutex<Vec<Vec<u8>>>>,
        rx: Arc<Mutex<VecDeque<Vec<u8>>>>,
    }

    impl thincan::AsyncTransport for AsyncNode {
        type SendFuture<'a>
            = core::future::Ready<Result<(), thincan::Error>>
        where
            Self: 'a;
        type RecvFuture<'a>
            = core::future::Ready<Result<Vec<Vec<u8>>, thincan::Error>>
        where
            Self: 'a;

        fn send<'a>(&'a mut self, payload: &'a [u8], _timeout: Duration) -> Self::SendFuture<'a> {
            self.sent.lock().unwrap().push(payload.to_vec());
            core::future::ready(Ok(()))
        }

        fn recv<'a>(&'a mut self, _timeout: Duration) -> Self::RecvFuture<'a> {
            let mut q = self.rx.lock().unwrap();
            let out: Vec<Vec<u8>> = q.drain(..).collect();
            core::future::ready(Ok(out))
        }
    }

    let sent = Arc::new(Mutex::new(Vec::new()));
    let rx = Arc::new(Mutex::new(VecDeque::new()));
    let mut iface = thincan::AsyncInterface::new(
        AsyncNode {
            sent: sent.clone(),
            rx: rx.clone(),
        },
        maplet_unhandled::Router::new(),
    );

    // send_msg length mismatch.
    let err = iface
        .send_msg::<atlas::Ping>(&[], Duration::from_millis(1))
        .await
        .unwrap_err();
    assert!(matches!(err.kind, ErrorKind::InvalidBodyLen { .. }));

    // send_encoded used > max_encoded_len.
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
        .send_encoded::<atlas::Ping, _>(&Lying, Duration::from_millis(1))
        .await
        .unwrap_err();
    assert!(matches!(err.kind, ErrorKind::Other));

    // send_encoded invalid body length for fixed-len messages (used <= max).
    struct WrongLen;
    impl thincan::Encode<atlas::Ping> for WrongLen {
        fn max_encoded_len(&self) -> usize {
            2
        }
        fn encode(&self, out: &mut [u8]) -> Result<usize, thincan::Error> {
            out[..2].copy_from_slice(&[0xAA, 0xBB]);
            Ok(2)
        }
    }
    let err = iface
        .send_encoded::<atlas::Ping, _>(&WrongLen, Duration::from_millis(1))
        .await
        .unwrap_err();
    assert!(matches!(err.kind, ErrorKind::InvalidBodyLen { .. }));

    // Ensure we did not send anything.
    assert!(sent.lock().unwrap().is_empty());
}
