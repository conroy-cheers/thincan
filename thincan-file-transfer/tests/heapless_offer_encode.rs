#![cfg(feature = "embassy")]

use capnp::message::ReaderOptions;

thincan::bus_atlas! {
    pub mod atlas {
        0x1001 => FileReq(capnp = thincan_file_transfer::schema::file_req::Owned);
        0x1002 => FileChunk(capnp = thincan_file_transfer::schema::file_chunk::Owned);
        0x1003 => FileAck(capnp = thincan_file_transfer::schema::file_ack::Owned);
    }
}

pub mod protocol_bundle {
    pub type Bundle = thincan_file_transfer::FileTransferBundle<super::atlas::Atlas>;
    pub const MESSAGE_COUNT: usize = thincan_file_transfer::FILE_TRANSFER_MESSAGE_COUNT;
}

thincan::maplet! {
    pub mod maplet: atlas {
        bundles [file_transfer = protocol_bundle];
    }
}

impl thincan_file_transfer::Atlas for atlas::Atlas {
    type FileReq = atlas::FileReq;
    type FileChunk = atlas::FileChunk;
    type FileAck = atlas::FileAck;
}

#[test]
fn heapless_file_offer_roundtrips_key_fields() {
    let mut tx_buf = [0u8; 512];
    let mut iface = maplet::Interface::<thincan::NoopRawMutex, _, _, 8, 512, 4>::new(
        (),
        (),
        tx_buf.as_mut_slice(),
    );

    let encoded = iface
        .encode_capnp_into::<atlas::FileReq, _>(&thincan_file_transfer::file_offer::<atlas::Atlas>(
            0x11,
            4096,
            512,
            b"",
            thincan_file_transfer::schema::FileHashAlgo::Sha256,
            &[0xAB; 32],
        ))
        .unwrap();

    assert_eq!(
        u16::from_le_bytes([encoded[0], encoded[1]]),
        <atlas::FileReq as thincan::Message>::ID
    );

    let body = &encoded[thincan::HEADER_LEN..];
    let parsed = thincan::CapnpTyped::<thincan_file_transfer::schema::file_req::Owned>::new(body)
        .with_root(ReaderOptions::default(), |root| {
            (
                root.get_transfer_id(),
                root.get_total_len(),
                root.get_sender_max_chunk_size(),
                root.get_file_metadata().unwrap_or(&[]).len(),
                root.get_file_hash_algo().ok(),
                root.get_file_hash().unwrap_or(&[]).len(),
            )
        })
        .unwrap();

    assert_eq!(parsed.0, 0x11);
    assert_eq!(parsed.1, 4096);
    assert_eq!(parsed.2, 512);
    assert_eq!(parsed.3, 0);
    assert_eq!(
        parsed.4,
        Some(thincan_file_transfer::schema::FileHashAlgo::Sha256)
    );
    assert_eq!(parsed.5, 32);
}

#[test]
fn heapless_file_offer_direct_encode_into_aligned_slice_roundtrips_key_fields() {
    let value = thincan_file_transfer::file_offer::<atlas::Atlas>(
        0x22,
        4096,
        512,
        b"",
        thincan_file_transfer::schema::FileHashAlgo::Sha256,
        &[0xCD; 32],
    );
    let mut out = [0u8; 128];
    let used = <thincan_file_transfer::FileOfferValue<'_, atlas::Atlas> as thincan::EncodeCapnp<
        atlas::FileReq,
    >>::encode(&value, &mut out)
    .unwrap();

    let body = &out[..used];
    let parsed = thincan::CapnpTyped::<thincan_file_transfer::schema::file_req::Owned>::new(body)
        .with_root(ReaderOptions::default(), |root| {
            (
                root.get_transfer_id(),
                root.get_total_len(),
                root.get_sender_max_chunk_size(),
                root.get_file_hash_algo().ok(),
                root.get_file_hash().unwrap_or(&[]).len(),
            )
        })
        .unwrap();

    assert_eq!(parsed.0, 0x22);
    assert_eq!(parsed.1, 4096);
    assert_eq!(parsed.2, 512);
    assert_eq!(
        parsed.3,
        Some(thincan_file_transfer::schema::FileHashAlgo::Sha256)
    );
    assert_eq!(parsed.4, 32);
}

#[test]
fn heapless_file_offer_encode_with_capnp_scratch_roundtrips_key_fields() {
    let mut scratch = thincan_file_transfer::CapnpScratch::<16>::new();
    let mut out = [0u8; 128];
    let used = thincan_file_transfer::encode_file_offer_into::<atlas::Atlas, 16>(
        &mut scratch,
        0x33,
        4096,
        512,
        b"",
        thincan_file_transfer::schema::FileHashAlgo::Sha256,
        &[0xEF; 32],
        &mut out,
    )
    .unwrap();

    let body = &out[..used];
    let parsed = thincan::CapnpTyped::<thincan_file_transfer::schema::file_req::Owned>::new(body)
        .with_root(ReaderOptions::default(), |root| {
            (
                root.get_transfer_id(),
                root.get_total_len(),
                root.get_sender_max_chunk_size(),
                root.get_file_hash_algo().ok(),
                root.get_file_hash().unwrap_or(&[]).len(),
            )
        })
        .unwrap();

    assert_eq!(parsed.0, 0x33);
    assert_eq!(parsed.1, 4096);
    assert_eq!(parsed.2, 512);
    assert_eq!(
        parsed.3,
        Some(thincan_file_transfer::schema::FileHashAlgo::Sha256)
    );
    assert_eq!(parsed.4, 32);
}
