#![cfg(all(feature = "std", feature = "capnp"))]

use std::sync::{Arc, Mutex};
use std::time::Duration;

#[path = "support/person_capnp.rs"]
mod person_capnp;
mod support;
use support::*;

use can_isotp_interface::{Destination, IsoTpAsyncWrite, TransmitError};
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

#[test]
fn send_side_validation_rejects_too_small_buffer() {
    let mut iface = maplet::Interface::<NoopRawMutex, _, _, 1, 8, 1>::new((), [0u8; 8]);
    let err = iface
        .encode_capnp_into::<atlas::A, _>(&PersonValue::named("A"))
        .unwrap_err();
    assert!(matches!(
        err.kind,
        thincan::ErrorKind::BufferTooSmall { .. }
    ));
}

#[derive(Debug, Default, Clone)]
struct CountingNode {
    sends: Arc<Mutex<usize>>,
    functional_sends: Arc<Mutex<usize>>,
}

impl IsoTpAsyncWrite for CountingNode {
    type Error = thincan::Error;

    type Deadline = tokio::time::Instant;
    async fn send(
        &mut self,
        destination: Destination,
        _payload: &[u8],
        _timeout: tokio::time::Instant,
    ) -> Result<(), TransmitError<Self::Error>> {
        match destination {
            Destination::Physical(_) => {
                *self.sends.lock().unwrap() += 1;
                Ok(())
            }
            Destination::Functional(_) => {
                *self.functional_sends.lock().unwrap() += 1;
                Ok(())
            }
        }
    }
}

#[tokio::test(flavor = "current_thread")]
async fn send_encoded_writes_and_validates() -> Result<(), thincan::SendMessageError<thincan::Error>>
{
    let sends = Arc::new(Mutex::new(0usize));
    let functional_sends = Arc::new(Mutex::new(0usize));
    let node = CountingNode {
        sends: sends.clone(),
        functional_sends: functional_sends.clone(),
    };

    let iface = maplet::Interface::<NoopRawMutex, _, _, 4, 128, 2>::new(node.clone(), [0u8; 128]);
    let bus = iface.bus().scope::<protocol_bundle::Bundle>();
    bus.__send_capnp_to::<atlas::A, _>(
        0x11,
        &PersonValue::named("A"),
        deadline(Duration::from_millis(1)),
    )
    .await?;
    bus.__send_capnp_functional_to::<atlas::A, _>(
        0x7F,
        &PersonValue::named("A"),
        deadline(Duration::from_millis(1)),
    )
    .await?;

    assert_eq!(*sends.lock().unwrap(), 1);
    assert_eq!(*functional_sends.lock().unwrap(), 1);
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn recv_one_dispatch_requires_one_transport_recv_per_payload()
-> Result<(), thincan::SendMessageError<thincan::Error>> {
    #[derive(Default)]
    struct TimeoutNode;

    impl IsoTpAsyncWrite for TimeoutNode {
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

    let iface = maplet::Interface::<NoopRawMutex, _, _, 4, 64, 2>::new(TimeoutNode, [0u8; 64]);
    let bus = iface.bus().scope::<protocol_bundle::Bundle>();
    let timed_out = tokio::time::timeout(
        Duration::from_millis(1),
        bus.__recv_next_capnp_from::<atlas::A>(0x22),
    )
    .await
    .is_err();
    assert!(timed_out);

    Ok(())
}
