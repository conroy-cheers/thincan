#![cfg(all(feature = "isotp-interface", feature = "uds"))]

use can_iso_tp::{IsoTpConfig, IsoTpDemux};
use can_isotp_interface::{IsoTpEndpoint, RecvControl, RecvMeta, RecvStatus};
use embedded_can_interface::SplitTxRx;
use embedded_can_mock::{BusHandle, MockCan};

#[test]
fn demux_send_and_recv_single_frame() {
    let bus = BusHandle::new();
    let a = MockCan::new_with_bus(&bus, vec![]).unwrap();
    let b = MockCan::new_with_bus(&bus, vec![]).unwrap();

    let (a_tx, a_rx) = a.split();
    let (b_tx, b_rx) = b.split();

    let mut cfg = IsoTpConfig::default();
    cfg.block_size = 0;
    cfg.st_min = core::time::Duration::from_millis(0);
    cfg.frame_len = 8;

    const MAX: usize = 2;
    let storages_a: [can_iso_tp::RxStorage<'static>; MAX] =
        core::array::from_fn(|_| can_iso_tp::RxStorage::Owned(vec![0u8; 4095]));
    let storages_b: [can_iso_tp::RxStorage<'static>; MAX] =
        core::array::from_fn(|_| can_iso_tp::RxStorage::Owned(vec![0u8; 4095]));

    let mut demux_a =
        IsoTpDemux::new(a_tx, a_rx, cfg.clone(), can_iso_tp::StdClock, 0x10, storages_a).unwrap();
    let mut demux_b = IsoTpDemux::new(b_tx, b_rx, cfg, can_iso_tp::StdClock, 0x20, storages_b)
        .unwrap();

    demux_a
        .send_to(0x20, b"yo", core::time::Duration::from_millis(10))
        .unwrap();

    let mut reply_to = None;
    let mut got = Vec::new();
    let status = demux_b
        .recv_one(core::time::Duration::from_millis(10), |meta: RecvMeta, payload| {
            reply_to = Some(meta.reply_to);
            got.extend_from_slice(payload);
            Ok(RecvControl::Continue)
        })
        .unwrap();

    assert_eq!(status, RecvStatus::DeliveredOne);
    assert_eq!(reply_to, Some(0x10));
    assert_eq!(got, b"yo");
}
