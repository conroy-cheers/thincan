#![cfg(feature = "isotp-interface")]

use can_iso_tp::{IsoTpConfig, IsoTpNode};
use can_isotp_interface::{IsoTpEndpoint, RecvControl, RecvStatus};
use embedded_can::StandardId;
use embedded_can_interface::{Id, SplitTxRx};
use embedded_can_mock::{BusHandle, MockCan};

#[test]
fn endpoint_send_and_recv_single_frame() {
    let bus = BusHandle::new();
    let a = MockCan::new_with_bus(&bus, vec![]).unwrap();
    let b = MockCan::new_with_bus(&bus, vec![]).unwrap();

    let (a_tx, a_rx) = a.split();
    let (b_tx, b_rx) = b.split();

    let id_a = Id::Standard(StandardId::new(0x123).unwrap());
    let id_b = Id::Standard(StandardId::new(0x321).unwrap());

    let mut cfg_a = IsoTpConfig::default();
    cfg_a.tx_id = id_a;
    cfg_a.rx_id = id_b;

    let mut cfg_b = IsoTpConfig::default();
    cfg_b.tx_id = id_b;
    cfg_b.rx_id = id_a;

    let mut buf_a = [0u8; 4095];
    let mut buf_b = [0u8; 4095];

    let mut node_a = IsoTpNode::with_std_clock(a_tx, a_rx, cfg_a, &mut buf_a).unwrap();
    let mut node_b = IsoTpNode::with_std_clock(b_tx, b_rx, cfg_b, &mut buf_b).unwrap();

    node_a
        .send(b"hi", core::time::Duration::from_millis(10))
        .unwrap();

    let mut got = Vec::new();
    let status = node_b
        .recv_one(core::time::Duration::from_millis(10), |payload| {
            got.extend_from_slice(payload);
            Ok(RecvControl::Continue)
        })
        .unwrap();

    assert_eq!(status, RecvStatus::DeliveredOne);
    assert_eq!(got, b"hi");
}

#[cfg(feature = "uds")]
mod uds_demux {
    use super::*;
    use can_iso_tp::IsoTpDemux;
    use can_isotp_interface::{IsoTpEndpointMeta, RecvMeta};

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

        let mut demux_a = IsoTpDemux::new(
            a_tx,
            a_rx,
            cfg.clone(),
            can_iso_tp::StdClock,
            0x10,
            storages_a,
        )
        .unwrap();
        let mut demux_b =
            IsoTpDemux::new(b_tx, b_rx, cfg, can_iso_tp::StdClock, 0x20, storages_b).unwrap();

        demux_a
            .send_to(0x20, b"yo", core::time::Duration::from_millis(10))
            .unwrap();

        let mut reply_to = None;
        let mut got = Vec::new();
        let status = demux_b
            .recv_one_meta(
                core::time::Duration::from_millis(10),
                |meta: RecvMeta<u8>, payload| {
                    reply_to = Some(meta.reply_to);
                    got.extend_from_slice(payload);
                    Ok(RecvControl::Continue)
                },
            )
            .unwrap();

        assert_eq!(status, RecvStatus::DeliveredOne);
        assert_eq!(reply_to, Some(0x10));
        assert_eq!(got, b"yo");
    }
}
