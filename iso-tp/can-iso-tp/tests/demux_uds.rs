#![cfg(feature = "uds")]

use core::time::Duration;

use can_iso_tp::{
    Clock, IsoTpConfig, IsoTpDemux, IsoTpNode, Progress, StdClock, rx_storages_from_buffers,
};
use embedded_can::Frame as _;
use embedded_can_interface::SplitTxRx as _;
use embedded_can_mock::MockFrame;
use embedded_can_mock::{BusHandle, MockCan};

fn base_cfg(max_payload_len: usize) -> IsoTpConfig {
    IsoTpConfig {
        // IDs are filled per-peer by the demux/node; keep them distinct to avoid accidental
        // `validate()` failures in other code paths.
        tx_id: embedded_can_interface::Id::Extended(embedded_can::ExtendedId::new(1).unwrap()),
        rx_id: embedded_can_interface::Id::Extended(embedded_can::ExtendedId::new(2).unwrap()),
        tx_addr: None,
        rx_addr: None,
        block_size: 2,
        st_min: Duration::from_millis(0),
        wft_max: 5,
        padding: None,
        max_payload_len,
        n_as: Duration::from_millis(1000),
        n_ar: Duration::from_millis(1000),
        n_bs: Duration::from_millis(1000),
        n_br: Duration::from_millis(1000),
        n_cs: Duration::from_millis(1000),
        frame_len: 8,
    }
}

#[test]
fn demux_receives_two_sources_without_mixing() {
    const MAX_PEERS: usize = 4;
    const BUF: usize = 512;

    let clock = StdClock;
    let bus = BusHandle::new();

    let mut rx_bufs = [[0u8; BUF]; MAX_PEERS];
    let storages = rx_storages_from_buffers(&mut rx_bufs);

    let local = 0x10u8;
    let sa1 = 0xA1u8;
    let sa2 = 0xA2u8;

    let mut receiver = MockCan::new_with_bus(&bus, vec![]).unwrap();
    // Ensure the receiver only gets frames addressed to `local` (prevents self-transmit echo).
    embedded_can_interface::FilterConfig::set_filters(
        &mut receiver,
        &[can_uds::uds29::filter_phys_for_target(local)],
    )
    .unwrap();
    let (rx_tx, rx_rx) = receiver.split();
    let mut demux = IsoTpDemux::<_, _, _, _, MAX_PEERS>::new(
        rx_tx,
        rx_rx,
        base_cfg(BUF),
        clock,
        local,
        storages,
    )
    .unwrap();
    // `receiver` is moved into the demux; filtering is configured above.

    let s1 = MockCan::new_with_bus(&bus, vec![]).unwrap();
    let s2 = MockCan::new_with_bus(&bus, vec![]).unwrap();

    let (s1_tx, s1_rx) = s1.split();
    let (s2_tx, s2_rx) = s2.split();

    let cfg1 = IsoTpConfig {
        tx_id: can_uds::uds29::encode_phys_id(local, sa1),
        rx_id: can_uds::uds29::encode_phys_id(sa1, local),
        ..base_cfg(BUF)
    };
    let cfg2 = IsoTpConfig {
        tx_id: can_uds::uds29::encode_phys_id(local, sa2),
        rx_id: can_uds::uds29::encode_phys_id(sa2, local),
        ..base_cfg(BUF)
    };

    let mut s1_rx_buf = [0u8; BUF];
    let mut s2_rx_buf = [0u8; BUF];
    let mut n1 = IsoTpNode::with_clock(s1_tx, s1_rx, cfg1, clock, &mut s1_rx_buf).unwrap();
    let mut n2 = IsoTpNode::with_clock(s2_tx, s2_rx, cfg2, clock, &mut s2_rx_buf).unwrap();

    let p1: Vec<u8> = (0..200).map(|i| (i as u8).wrapping_add(1)).collect();
    let p2: Vec<u8> = (0..200).map(|i| (i as u8).wrapping_add(101)).collect();

    let start = clock.now();
    let mut s1_done = false;
    let mut s2_done = false;
    let mut got1: Option<Vec<u8>> = None;
    let mut got2: Option<Vec<u8>> = None;

    while clock.elapsed(start) < Duration::from_secs(2) {
        let now = clock.now();

        if !s1_done {
            if n1.poll_send(&p1, now).unwrap() == Progress::Completed {
                s1_done = true;
            }
        }
        if !s2_done {
            if n2.poll_send(&p2, now).unwrap() == Progress::Completed {
                s2_done = true;
            }
        }

        let _ = demux.poll_recv(now, &mut |reply_to, payload| {
            if reply_to == sa1 {
                got1 = Some(payload.to_vec());
            } else if reply_to == sa2 {
                got2 = Some(payload.to_vec());
            }
        });

        if s1_done && s2_done && got1.is_some() && got2.is_some() {
            break;
        }
    }

    assert!(s1_done, "sender 1 did not complete in time");
    assert!(s2_done, "sender 2 did not complete in time");
    assert_eq!(got1.as_deref(), Some(p1.as_slice()));
    assert_eq!(got2.as_deref(), Some(p2.as_slice()));
}

#[test]
fn demux_can_send_to_and_peer_can_receive() {
    const MAX_PEERS: usize = 2;
    const BUF: usize = 512;

    let clock = StdClock;
    let bus = BusHandle::new();

    let mut rx_bufs = [[0u8; BUF]; MAX_PEERS];
    let storages = rx_storages_from_buffers(&mut rx_bufs);

    let local = 0x10u8;
    let remote = 0xA1u8;

    let receiver = MockCan::new_with_bus(&bus, vec![]).unwrap();
    let (rx_tx, rx_rx) = receiver.split();
    let mut demux = IsoTpDemux::<_, _, _, _, MAX_PEERS>::new(
        rx_tx,
        rx_rx,
        base_cfg(BUF),
        clock,
        local,
        storages,
    )
    .unwrap();

    let sender = MockCan::new_with_bus(&bus, vec![]).unwrap();
    let (s_tx, s_rx) = sender.split();
    let mut s_rx_buf = [0u8; BUF];
    let sender_cfg = IsoTpConfig {
        tx_id: can_uds::uds29::encode_phys_id(local, remote),
        rx_id: can_uds::uds29::encode_phys_id(remote, local),
        ..base_cfg(BUF)
    };
    let mut peer = IsoTpNode::with_clock(s_tx, s_rx, sender_cfg, clock, &mut s_rx_buf).unwrap();

    let payload: Vec<u8> = (0..200).map(|i| i as u8).collect();

    let start = clock.now();
    let mut send_done = false;
    let mut got: Option<Vec<u8>> = None;

    while clock.elapsed(start) < Duration::from_secs(2) {
        let now = clock.now();

        if !send_done {
            if demux.poll_send_to(remote, &payload, now).unwrap() == Progress::Completed {
                send_done = true;
            }
        }

        let _ = peer.poll_recv(now, &mut |data| {
            got = Some(data.to_vec());
        });

        if send_done && got.is_some() {
            break;
        }
    }

    assert!(send_done, "demux send did not complete in time");
    assert_eq!(got.as_deref(), Some(payload.as_slice()));
}

#[test]
fn demux_receives_functional_single_frame() {
    const MAX_PEERS: usize = 2;
    const BUF: usize = 64;

    let clock = StdClock;
    let bus = BusHandle::new();

    let mut rx_bufs = [[0u8; BUF]; MAX_PEERS];
    let storages = rx_storages_from_buffers(&mut rx_bufs);

    let local_phys = 0x10u8;
    let local_func = 0x33u8;
    let tester = 0xF1u8;

    let mut receiver = MockCan::new_with_bus(&bus, vec![]).unwrap();
    embedded_can_interface::FilterConfig::set_filters(
        &mut receiver,
        can_uds::uds29::filters_for_targets(local_phys, Some(local_func)).as_slice(),
    )
    .unwrap();
    let (rx_tx, rx_rx) = receiver.split();
    let mut demux = IsoTpDemux::<_, _, _, _, MAX_PEERS>::new(
        rx_tx,
        rx_rx,
        base_cfg(BUF),
        clock,
        local_phys,
        storages,
    )
    .unwrap()
    .with_functional_addr(local_func);

    let mut sender = MockCan::new_with_bus(&bus, vec![]).unwrap();
    let payload = b"ping";
    let mut data = [0u8; 8];
    data[0] = payload.len() as u8; // ISO-TP single-frame PCI (len)
    data[1..1 + payload.len()].copy_from_slice(payload);

    let can_id = can_uds::uds29::encode_func_id_raw(local_func, tester);
    let frame = MockFrame::new(
        embedded_can::Id::Extended(embedded_can::ExtendedId::new(can_id).unwrap()),
        &data[..1 + payload.len()],
    )
    .unwrap();
    embedded_can_interface::TxFrameIo::send(&mut sender, &frame).unwrap();

    let mut got: Option<(u8, Vec<u8>)> = None;
    demux
        .recv(Duration::from_millis(50), &mut |reply_to, bytes| {
            got = Some((reply_to, bytes.to_vec()));
        })
        .unwrap();

    let (reply_to, bytes) = got.expect("expected a delivered payload");
    assert_eq!(reply_to, tester);
    assert_eq!(bytes, payload);
}

#[test]
fn demux_ignores_functional_multi_frame() {
    const MAX_PEERS: usize = 2;
    const BUF: usize = 64;

    let clock = StdClock;
    let bus = BusHandle::new();

    let mut rx_bufs = [[0u8; BUF]; MAX_PEERS];
    let storages = rx_storages_from_buffers(&mut rx_bufs);

    let local_phys = 0x10u8;
    let local_func = 0x33u8;
    let tester = 0xF1u8;

    let mut receiver = MockCan::new_with_bus(&bus, vec![]).unwrap();
    embedded_can_interface::FilterConfig::set_filters(
        &mut receiver,
        can_uds::uds29::filters_for_targets(local_phys, Some(local_func)).as_slice(),
    )
    .unwrap();
    let (rx_tx, rx_rx) = receiver.split();
    let mut demux = IsoTpDemux::<_, _, _, _, MAX_PEERS>::new(
        rx_tx,
        rx_rx,
        base_cfg(BUF),
        clock,
        local_phys,
        storages,
    )
    .unwrap()
    .with_functional_addr(local_func);

    let mut sender = MockCan::new_with_bus(&bus, vec![]).unwrap();
    // Craft a First Frame on a functional ID (should be ignored).
    let mut data = [0u8; 8];
    data[0] = 0x10; // First Frame, high nibble 1
    data[1] = 0x14; // total len 0x014

    let can_id = can_uds::uds29::encode_func_id_raw(local_func, tester);
    let frame = MockFrame::new(
        embedded_can::Id::Extended(embedded_can::ExtendedId::new(can_id).unwrap()),
        &data[..8],
    )
    .unwrap();
    embedded_can_interface::TxFrameIo::send(&mut sender, &frame).unwrap();

    let now = clock.now();
    let mut delivered = false;
    loop {
        match demux
            .poll_recv(now, &mut |_reply_to, _payload| delivered = true)
            .unwrap()
        {
            Progress::WouldBlock => break,
            Progress::Completed | Progress::InFlight | Progress::WaitingForFlowControl => continue,
        }
    }
    assert!(!delivered, "functional multi-frame should be ignored");
}

#[test]
fn demux_can_send_functional_single_frame() {
    const MAX_PEERS: usize = 4;
    const BUF: usize = 64;

    let clock = StdClock;
    let bus = BusHandle::new();

    let local_ecu = 0x10u8;
    let functional = 0x33u8;
    let tester = 0xF1u8;

    let mut ecu_can = MockCan::new_with_bus(&bus, vec![]).unwrap();
    embedded_can_interface::FilterConfig::set_filters(
        &mut ecu_can,
        can_uds::uds29::filters_for_targets(local_ecu, Some(functional)).as_slice(),
    )
    .unwrap();
    let (ecu_tx, ecu_rx) = ecu_can.split();
    let mut ecu_bufs = [[0u8; BUF]; MAX_PEERS];
    let ecu_storages = rx_storages_from_buffers(&mut ecu_bufs);
    let mut ecu = IsoTpDemux::<_, _, _, _, MAX_PEERS>::new(
        ecu_tx,
        ecu_rx,
        base_cfg(BUF),
        clock,
        local_ecu,
        ecu_storages,
    )
    .unwrap()
    .with_functional_addr(functional);

    let mut tester_can = MockCan::new_with_bus(&bus, vec![]).unwrap();
    embedded_can_interface::FilterConfig::set_filters(
        &mut tester_can,
        &[can_uds::uds29::filter_phys_for_target(tester)],
    )
    .unwrap();
    let (tester_tx, tester_rx) = tester_can.split();
    let mut tester_bufs = [[0u8; BUF]; MAX_PEERS];
    let tester_storages = rx_storages_from_buffers(&mut tester_bufs);
    let mut tester_demux = IsoTpDemux::<_, _, _, _, MAX_PEERS>::new(
        tester_tx,
        tester_rx,
        base_cfg(BUF),
        clock,
        tester,
        tester_storages,
    )
    .unwrap();

    let payload = b"hello";
    tester_demux
        .send_functional_to(functional, payload, Duration::from_millis(50))
        .unwrap();

    let mut got: Option<(u8, Vec<u8>)> = None;
    ecu.recv(Duration::from_millis(50), &mut |reply_to, bytes| {
        got = Some((reply_to, bytes.to_vec()));
    })
    .unwrap();

    let (reply_to, bytes) = got.expect("expected a delivered payload");
    assert_eq!(reply_to, tester);
    assert_eq!(bytes, payload);
}
