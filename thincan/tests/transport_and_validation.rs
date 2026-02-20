#![cfg(all(feature = "std", feature = "capnp"))]

use std::sync::{Arc, Mutex};
use std::time::Duration;

#[path = "support/person_capnp.rs"]
mod person_capnp;

use can_isotp_interface::{
    IsoTpAsyncEndpoint, IsoTpAsyncEndpointRecvInto, RecvControl, RecvError, RecvMeta,
    RecvMetaIntoStatus, RecvStatus, SendError,
};
use capnp::message::SingleSegmentAllocator;
use embassy_sync::blocking_mutex::raw::NoopRawMutex;

thincan::bus_atlas! {
    pub mod atlas {
        0x0010 => A(capnp = crate::person_capnp::person::Owned);
    }
}

pub mod protocol_bundle {
    #[derive(Clone, Copy, Debug, Default)]
    pub struct Bundle;

    pub const MESSAGE_COUNT: usize = 1;

    impl thincan::BundleSpec<MESSAGE_COUNT> for Bundle {
        const MESSAGE_IDS: [u16; MESSAGE_COUNT] = [<super::atlas::A as thincan::Message>::ID];
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
fn send_side_validation_rejects_too_small_buffer() {
    let mut iface = maplet::Interface::<NoopRawMutex, _, _, 1, 8, 1>::new((), [0u8; 8]);
    let err = iface
        .encode_capnp_into::<atlas::A, _>(&PersonValue { name: "A" })
        .unwrap_err();
    assert!(matches!(
        err.kind,
        thincan::ErrorKind::BufferTooSmall { .. }
    ));
}

#[derive(Debug, Default)]
struct CountingNode {
    sends: Arc<Mutex<usize>>,
    functional_sends: Arc<Mutex<usize>>,
}

impl IsoTpAsyncEndpoint for CountingNode {
    type Error = thincan::Error;

    async fn send_to(
        &mut self,
        _to: u8,
        _payload: &[u8],
        _timeout: Duration,
    ) -> Result<(), SendError<Self::Error>> {
        *self.sends.lock().unwrap() += 1;
        Ok(())
    }

    async fn send_functional_to(
        &mut self,
        _functional_to: u8,
        _payload: &[u8],
        _timeout: Duration,
    ) -> Result<(), SendError<Self::Error>> {
        *self.functional_sends.lock().unwrap() += 1;
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

impl IsoTpAsyncEndpointRecvInto for CountingNode {
    type Error = thincan::Error;

    async fn recv_one_into(
        &mut self,
        _timeout: Duration,
        _out: &mut [u8],
    ) -> Result<RecvMetaIntoStatus, RecvError<Self::Error>> {
        Ok(RecvMetaIntoStatus::TimedOut)
    }
}

#[tokio::test(flavor = "current_thread")]
async fn send_encoded_writes_and_validates() -> Result<(), thincan::Error> {
    let sends = Arc::new(Mutex::new(0usize));
    let functional_sends = Arc::new(Mutex::new(0usize));
    let node = CountingNode {
        sends: sends.clone(),
        functional_sends: functional_sends.clone(),
    };

    let iface = maplet::Interface::<NoopRawMutex, _, _, 4, 128, 2>::new(node, [0u8; 128]);
    let doodad = iface.handle().scope::<protocol_bundle::Bundle>();
    doodad
        .__send_capnp_to::<atlas::A, _>(0x11, &PersonValue { name: "A" }, Duration::from_millis(1))
        .await?;
    doodad
        .__send_capnp_functional_to::<atlas::A, _>(
            0x7F,
            &PersonValue { name: "A" },
            Duration::from_millis(1),
        )
        .await?;

    assert_eq!(*sends.lock().unwrap(), 1);
    assert_eq!(*functional_sends.lock().unwrap(), 1);
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn recv_one_dispatch_requires_one_transport_recv_per_payload() -> Result<(), thincan::Error> {
    #[derive(Default)]
    struct TimeoutNode;

    impl IsoTpAsyncEndpoint for TimeoutNode {
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

    impl IsoTpAsyncEndpointRecvInto for TimeoutNode {
        type Error = thincan::Error;

        async fn recv_one_into(
            &mut self,
            _timeout: Duration,
            _out: &mut [u8],
        ) -> Result<RecvMetaIntoStatus, RecvError<Self::Error>> {
            Ok(RecvMetaIntoStatus::TimedOut)
        }
    }

    let iface = maplet::Interface::<NoopRawMutex, _, _, 4, 64, 2>::new(TimeoutNode, [0u8; 64]);
    let doodad = iface.handle().scope::<protocol_bundle::Bundle>();
    let timed_out = tokio::time::timeout(
        Duration::from_millis(1),
        doodad.__recv_next_capnp_from::<atlas::A>(0x22),
    )
    .await
    .is_err();
    assert!(timed_out);

    Ok(())
}
