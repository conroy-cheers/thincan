#![cfg(all(not(feature = "std"), feature = "capnp"))]
#![no_std]

#[path = "support/person_capnp.rs"]
mod person_capnp;

thincan::bus_atlas! {
    pub mod atlas {
        0x0100 => Ping(capnp = crate::person_capnp::person::Owned);
        0x0101 => Pong(capnp = crate::person_capnp::person::Owned);
    }
}

pub mod protocol_bundle {
    #[derive(Clone, Copy, Debug, Default)]
    pub struct Bundle;

    pub const MESSAGE_COUNT: usize = 2;

    impl thincan::BundleSpec<MESSAGE_COUNT> for Bundle {
        const MESSAGE_IDS: [u16; MESSAGE_COUNT] = [
            <super::atlas::Ping as thincan::Message>::ID,
            <super::atlas::Pong as thincan::Message>::ID,
        ];
    }
}

thincan::maplet! {
    pub mod maplet: atlas {
        bundles [protocol_bundle];
    }
}

#[test]
fn no_std_atlas_markers_and_decode_wire_compile() {
    let payload = [
        (<atlas::Ping as thincan::Message>::ID as u8),
        ((<atlas::Ping as thincan::Message>::ID >> 8) as u8),
    ];
    let raw = thincan::decode_wire(&payload).unwrap();
    assert_eq!(raw.id, <atlas::Ping as thincan::Message>::ID);
    assert!(raw.body.is_empty());

    let _iface =
        maplet::Interface::<embassy_sync::blocking_mutex::raw::NoopRawMutex, _, _, 1, 8, 1>::new(
            (),
            &mut [0u8; 8][..],
        );
}
