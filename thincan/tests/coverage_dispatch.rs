#![cfg(all(feature = "std", feature = "capnp"))]

#[path = "support/person_capnp.rs"]
mod person_capnp;

use capnp::message::SingleSegmentAllocator;
use std::time::Duration;

use can_isotp_interface::{
    IsoTpAsyncEndpoint, IsoTpAsyncEndpointRecvInto, RecvControl, RecvError, RecvMeta,
    RecvMetaIntoStatus, RecvStatus, SendError,
};
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

#[derive(Clone, Copy)]
struct PersonValue {
    name: &'static str,
}

impl<M> thincan::EncodeCapnp<M> for PersonValue
where
    M: thincan::CapnpMessage<Owned = person_capnp::person::Owned>,
{
    fn max_encoded_len(&self) -> usize {
        96
    }

    fn encode(&self, out: &mut [u8]) -> Result<usize, thincan::Error> {
        let mut scratch = [0u8; 96];
        let mut msg = capnp::message::Builder::new(SingleSegmentAllocator::new(&mut scratch));
        let mut root: person_capnp::person::Builder = msg.init_root();
        root.set_name(self.name);
        root.set_email("e");

        let body = msg.get_segments_for_output()[0];
        if out.len() < body.len() {
            return Err(thincan::Error {
                kind: thincan::ErrorKind::BufferTooSmall {
                    needed: body.len(),
                    got: out.len(),
                },
            });
        }
        out[..body.len()].copy_from_slice(body);
        Ok(body.len())
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

    impl IsoTpAsyncEndpoint for DummyAsync {
        type Error = thincan::Error;

        async fn send_to(
            &mut self,
            _to: u8,
            _payload: &[u8],
            _timeout: Duration,
        ) -> Result<(), SendError<Self::Error>> {
            Ok(())
        }

        async fn send_functional_to(
            &mut self,
            _functional_to: u8,
            _payload: &[u8],
            _timeout: Duration,
        ) -> Result<(), SendError<Self::Error>> {
            Ok(())
        }

        async fn recv_one<Cb>(
            &mut self,
            _timeout: Duration,
            _on_payload: Cb,
        ) -> Result<RecvStatus, RecvError<Self::Error>>
        where
            Cb: FnMut(RecvMeta, &[u8]) -> Result<RecvControl, Self::Error>,
        {
            Ok(RecvStatus::TimedOut)
        }
    }

    impl IsoTpAsyncEndpointRecvInto for DummyAsync {
        type Error = thincan::Error;

        async fn recv_one_into(
            &mut self,
            _timeout: Duration,
            _out: &mut [u8],
        ) -> Result<RecvMetaIntoStatus, RecvError<Self::Error>> {
            Ok(RecvMetaIntoStatus::TimedOut)
        }
    }

    let iface = maplet::Interface::<NoopRawMutex, _, _, 4, 64, 2>::new(DummyAsync, [0u8; 64]);
    let doodad = iface.handle().scope::<protocol_bundle::Bundle>();
    let timed_out = tokio::time::timeout(
        Duration::from_millis(1),
        doodad.__recv_next_capnp_from::<atlas::Ping>(0x12),
    )
    .await
    .is_err();
    assert!(timed_out);
}

#[tokio::test(flavor = "current_thread")]
async fn ingest_rejects_body_larger_than_mailbox_capacity() {
    #[derive(Default)]
    struct BufferTooSmallAsync;

    impl IsoTpAsyncEndpoint for BufferTooSmallAsync {
        type Error = thincan::Error;

        async fn send_to(
            &mut self,
            _to: u8,
            _payload: &[u8],
            _timeout: Duration,
        ) -> Result<(), SendError<Self::Error>> {
            Ok(())
        }

        async fn send_functional_to(
            &mut self,
            _functional_to: u8,
            _payload: &[u8],
            _timeout: Duration,
        ) -> Result<(), SendError<Self::Error>> {
            Ok(())
        }

        async fn recv_one<Cb>(
            &mut self,
            _timeout: Duration,
            _on_payload: Cb,
        ) -> Result<RecvStatus, RecvError<Self::Error>>
        where
            Cb: FnMut(RecvMeta, &[u8]) -> Result<RecvControl, Self::Error>,
        {
            Ok(RecvStatus::TimedOut)
        }
    }

    impl IsoTpAsyncEndpointRecvInto for BufferTooSmallAsync {
        type Error = thincan::Error;

        async fn recv_one_into(
            &mut self,
            _timeout: Duration,
            _out: &mut [u8],
        ) -> Result<RecvMetaIntoStatus, RecvError<Self::Error>> {
            Ok(RecvMetaIntoStatus::TimedOut)
        }
    }

    let iface =
        maplet::Interface::<NoopRawMutex, _, _, 4, 8, 2>::new(BufferTooSmallAsync, [0u8; 64]);
    let doodad = iface.handle().scope::<protocol_bundle::Bundle>();
    let mut payload = [0u8; 18];
    payload[..2].copy_from_slice(&atlas::Ping::ID.to_le_bytes());

    let err = doodad.ingest(0x22, &payload).await.unwrap_err();
    assert!(matches!(
        err,
        thincan::IngestError::BodyTooLarge { got: 16, max: 8 }
    ));
}

#[tokio::test(flavor = "current_thread")]
async fn async_send_helpers_map_backend_errors() {
    #[derive(Default)]
    struct BackendErrorNode;

    impl IsoTpAsyncEndpoint for BackendErrorNode {
        type Error = ();

        async fn send_to(
            &mut self,
            _to: u8,
            _payload: &[u8],
            _timeout: Duration,
        ) -> Result<(), SendError<Self::Error>> {
            Err(SendError::Backend(()))
        }

        async fn send_functional_to(
            &mut self,
            _functional_to: u8,
            _payload: &[u8],
            _timeout: Duration,
        ) -> Result<(), SendError<Self::Error>> {
            Err(SendError::Backend(()))
        }

        async fn recv_one<Cb>(
            &mut self,
            _timeout: Duration,
            _on_payload: Cb,
        ) -> Result<RecvStatus, RecvError<Self::Error>>
        where
            Cb: FnMut(RecvMeta, &[u8]) -> Result<RecvControl, Self::Error>,
        {
            Ok(RecvStatus::TimedOut)
        }
    }

    let iface =
        maplet::Interface::<NoopRawMutex, _, _, 4, 128, 2>::new(BackendErrorNode, [0u8; 128]);
    let doodad = iface.handle().scope::<protocol_bundle::Bundle>();
    let err = doodad
        .__send_capnp_to::<atlas::Ping, _>(0, &PersonValue { name: "x" }, Duration::from_millis(1))
        .await
        .unwrap_err();
    assert!(matches!(err.kind, ErrorKind::Other));

    let err = doodad
        .__send_capnp_functional_to::<atlas::Ping, _>(
            0,
            &PersonValue { name: "x" },
            Duration::from_millis(1),
        )
        .await
        .unwrap_err();
    assert!(matches!(err.kind, ErrorKind::Other));
}
