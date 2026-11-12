#![cfg(all(feature = "std", feature = "capnp"))]

#[path = "support/person_capnp.rs"]
mod person_capnp;
mod support;
use support::*;

use std::time::Duration;

use can_isotp_interface::{Destination, IsoTpAsyncWrite, TransmitError};
use embassy_sync::blocking_mutex::raw::NoopRawMutex;
use thincan::{ErrorKind, Message};

thincan::bus_atlas! {
    pub mod atlas {
        0x0100 => Ping(capnp = crate::person_capnp::person::Owned);
    }
}

pub mod protocol_bundle {
    #[derive(Clone, Copy, Debug, Default)]
    pub struct Bundle;

    pub const MESSAGE_COUNT: usize = 1;

    impl thincan::BundleSpec<MESSAGE_COUNT> for Bundle {
        const MESSAGE_IDS: [u16; MESSAGE_COUNT] = [<super::atlas::Ping as thincan::Message>::ID];
    }
}

thincan::maplet! {
    pub mod maplet: atlas {
        bundles [protocol_bundle];
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

#[cfg(feature = "std")]
#[test]
fn interface_send_capnp_rejects_encode_overflowing_max_len() {
    struct LyingValue;
    impl thincan::EncodeCapnp<atlas::Ping> for LyingValue {
        fn max_encoded_len(&self) -> usize {
            1
        }
        fn encode(&self, _out: &mut [u8]) -> Result<usize, thincan::Error> {
            Ok(2)
        }
    }

    let mut iface = maplet::Interface::<NoopRawMutex, _, _, 1, 8, 1>::new((), [0u8; 8]);
    let err = iface
        .encode_capnp_into::<atlas::Ping, _>(&LyingValue)
        .unwrap_err();
    assert!(matches!(err.kind, ErrorKind::Other));
}

#[tokio::test(flavor = "current_thread")]
async fn async_recv_timeout_maps_to_timeout_error() {
    #[derive(Debug, Default)]
    struct DummyAsync;

    impl IsoTpAsyncWrite for DummyAsync {
        type Error = thincan::Error;

        type Deadline = tokio::time::Instant;
        async fn send(
            &mut self,
            _destination: Destination,
            _payload: &[u8],
            _timeout: tokio::time::Instant,
        ) -> Result<(), TransmitError<Self::Error>> {
            Ok(())
        }
    }

    let iface = maplet::Interface::<NoopRawMutex, _, _, 4, 64, 2>::new(DummyAsync, [0u8; 64]);
    let bus = iface.bus().scope::<protocol_bundle::Bundle>();
    let timed_out = tokio::time::timeout(
        Duration::from_millis(1),
        bus.__recv_next_capnp_from::<atlas::Ping>(0x12),
    )
    .await
    .is_err();
    assert!(timed_out);
}

#[tokio::test(flavor = "current_thread")]
async fn ingest_rejects_body_larger_than_mailbox_capacity() {
    #[derive(Default)]
    struct BufferTooSmallAsync;

    impl IsoTpAsyncWrite for BufferTooSmallAsync {
        type Error = thincan::Error;

        type Deadline = tokio::time::Instant;
        async fn send(
            &mut self,
            _destination: Destination,
            _payload: &[u8],
            _timeout: tokio::time::Instant,
        ) -> Result<(), TransmitError<Self::Error>> {
            Ok(())
        }
    }

    let iface =
        maplet::Interface::<NoopRawMutex, _, _, 4, 8, 2>::new(BufferTooSmallAsync, [0u8; 64]);
    let bus = iface.bus().scope::<protocol_bundle::Bundle>();
    let mut payload = [0u8; 18];
    payload[..2].copy_from_slice(&atlas::Ping::ID.to_le_bytes());

    let err = bus.ingest(0x22, &payload).await.unwrap_err();
    assert!(matches!(
        err,
        thincan::IngestError::BodyTooLarge { got: 16, max: 8 }
    ));
}

#[tokio::test(flavor = "current_thread")]
async fn async_send_helpers_map_backend_errors() {
    #[derive(Default)]
    struct BackendErrorNode;

    impl IsoTpAsyncWrite for BackendErrorNode {
        type Error = ();

        type Deadline = tokio::time::Instant;
        async fn send(
            &mut self,
            _destination: Destination,
            _payload: &[u8],
            _timeout: tokio::time::Instant,
        ) -> Result<(), TransmitError<Self::Error>> {
            Err(TransmitError::Rejected(()))
        }
    }

    let iface =
        maplet::Interface::<NoopRawMutex, _, _, 4, 128, 2>::new(BackendErrorNode, [0u8; 128]);
    let bus = iface.bus().scope::<protocol_bundle::Bundle>();
    let err = bus
        .__send_capnp_to::<atlas::Ping, _>(
            0,
            &PersonValue::named("x"),
            deadline(Duration::from_secs(1)),
        )
        .await
        .unwrap_err();
    assert!(matches!(
        err,
        thincan::SendMessageError::Transport(TransmitError::Rejected(()))
    ));

    let err = bus
        .__send_capnp_functional_to::<atlas::Ping, _>(
            0,
            &PersonValue::named("x"),
            deadline(Duration::from_secs(1)),
        )
        .await
        .unwrap_err();
    assert!(matches!(
        err,
        thincan::SendMessageError::Transport(TransmitError::Rejected(()))
    ));
}
