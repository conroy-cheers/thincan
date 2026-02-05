use can_iso_tp::pdu::{Pdu, encode};
use can_iso_tp::{IsoTpConfig, IsoTpNode, Progress, RxStorage, StdClock, TimeoutKind};
use embedded_can::{Frame, StandardId};
use embedded_can_interface::{Id, SplitTxRx};
use embedded_can_mock::{BusHandle, MockCan, MockFrame};
use std::time::{Duration, Instant};

fn cfg(tx: u16, rx: u16, block_size: u8, max_len: usize) -> IsoTpConfig {
    IsoTpConfig {
        tx_id: Id::Standard(StandardId::new(tx).unwrap()),
        rx_id: Id::Standard(StandardId::new(rx).unwrap()),
        tx_addr: None,
        rx_addr: None,
        block_size,
        st_min: Duration::from_millis(0),
        wft_max: 2,
        padding: None,
        max_payload_len: max_len,
        n_as: Duration::from_millis(200),
        n_ar: Duration::from_millis(200),
        n_bs: Duration::from_millis(200),
        n_br: Duration::from_millis(200),
        n_cs: Duration::from_millis(200),
        frame_len: 8,
    }
}

fn make_frame(id: u16, data: &[u8]) -> MockFrame {
    MockFrame::new(
        embedded_can::Id::Standard(StandardId::new(id).unwrap()),
        data,
    )
    .unwrap()
}

#[test]
fn config_validation_rejects_mirrored_ids_and_bad_lengths() {
    let mut cfg = cfg(0x100, 0x100, 0, 32);
    assert!(cfg.validate().is_err());
    cfg.tx_id = Id::Standard(StandardId::new(0x101).unwrap());
    cfg.max_payload_len = 0;
    assert!(cfg.validate().is_err());
    cfg.max_payload_len = 4096;
    assert!(cfg.validate().is_err());
}

#[test]
fn zero_length_payload_roundtrip() {
    let bus = BusHandle::new();
    let a = MockCan::new_with_bus(&bus, vec![]).unwrap();
    let b = MockCan::new_with_bus(&bus, vec![]).unwrap();
    let (tx_a, rx_a) = a.split();
    let (tx_b, rx_b) = b.split();
    let mut sender = IsoTpNode::with_clock_and_storage(
        tx_a,
        rx_a,
        cfg(0x200, 0x201, 0, 32),
        StdClock,
        RxStorage::Owned(vec![0u8; 32]),
    )
    .unwrap();
    let mut receiver = IsoTpNode::with_clock_and_storage(
        tx_b,
        rx_b,
        cfg(0x201, 0x200, 0, 32),
        StdClock,
        RxStorage::Owned(vec![0u8; 32]),
    )
    .unwrap();

    sender
        .send(&[], Duration::from_millis(100))
        .expect("send empty");
    let mut delivered = Vec::new();
    receiver
        .recv(Duration::from_millis(100), &mut |data| {
            delivered = data.to_vec()
        })
        .expect("recv");
    assert!(delivered.is_empty());
}

#[test]
fn oversized_payload_triggers_overflow_on_both_sides() {
    let bus = BusHandle::new();
    let a = MockCan::new_with_bus(&bus, vec![]).unwrap();
    let b = MockCan::new_with_bus(&bus, vec![]).unwrap();
    let (tx_a, rx_a) = a.split();
    let (tx_b, rx_b) = b.split();
    let mut sender = IsoTpNode::with_clock_and_storage(
        tx_a,
        rx_a,
        cfg(0x300, 0x301, 0, 32),
        StdClock,
        RxStorage::Owned(vec![0u8; 32]),
    )
    .unwrap();
    let mut receiver = IsoTpNode::with_clock_and_storage(
        tx_b,
        rx_b,
        cfg(0x301, 0x300, 0, 8),
        StdClock,
        RxStorage::Owned(vec![0u8; 8]),
    )
    .unwrap();

    let payload: Vec<u8> = (0..20u8).collect();
    let now = Instant::now();
    assert_eq!(
        sender.poll_send(&payload, now).unwrap(),
        Progress::WaitingForFlowControl
    );

    let mut delivered = Vec::new();
    let recv_err = receiver.poll_recv(now, &mut |data| delivered = data.to_vec());
    assert!(matches!(recv_err, Err(can_iso_tp::IsoTpError::RxOverflow)));
    assert!(delivered.is_empty());

    let send_err = sender.poll_send(&payload, now);
    assert!(matches!(send_err, Err(can_iso_tp::IsoTpError::Overflow)));
}

#[test]
fn flow_control_wait_exceeds_wft_max() {
    let bus = BusHandle::new();
    let can = MockCan::new_with_bus(&bus, vec![]).unwrap();
    let injector = bus.add_interface(vec![]).unwrap();
    let (tx, rx) = can.split();
    let mut sender = IsoTpNode::with_clock_and_storage(
        tx,
        rx,
        cfg(0x400, 0x401, 0, 64),
        StdClock,
        RxStorage::Owned(vec![0u8; 64]),
    )
    .unwrap();

    let payload: Vec<u8> = (0..24u8).collect();
    let now = Instant::now();
    let first = sender.poll_send(&payload, now).expect("first ff");
    assert_eq!(first, Progress::WaitingForFlowControl);

    // Send Wait responses beyond wft_max.
    for _ in 0..=2 {
        injector
            .transmit(make_frame(0x401, &[0x31, 0x00, 0x00]))
            .unwrap();
        let res = sender.poll_send(&payload, now);
        if res.is_err() {
            break;
        }
    }

    let res = sender.poll_send(&payload, now);
    match res {
        Err(can_iso_tp::IsoTpError::Timeout(TimeoutKind::NBs)) => {}
        other => panic!("expected NBs timeout, got {:?}", other),
    }
}

#[test]
fn st_min_pacing_blocks_until_deadline() {
    let bus = BusHandle::new();
    let can = MockCan::new_with_bus(&bus, vec![]).unwrap();
    let injector = bus.add_interface(vec![]).unwrap();
    let (tx, rx) = can.split();
    let mut sender = IsoTpNode::with_clock_and_storage(
        tx,
        rx,
        cfg(0x500, 0x501, 0, 64),
        StdClock,
        RxStorage::Owned(vec![0u8; 64]),
    )
    .unwrap();
    let payload: Vec<u8> = (0..16u8).collect();
    let t0 = Instant::now();
    assert_eq!(
        sender.poll_send(&payload, t0).unwrap(),
        Progress::WaitingForFlowControl
    );
    injector
        .transmit(make_frame(0x501, &[0x30, 0x00, 0x7F]))
        .unwrap();
    assert_eq!(sender.poll_send(&payload, t0).unwrap(), Progress::InFlight);
    // Still before stmin deadline, should block.
    assert_eq!(
        sender.poll_send(&payload, t0).unwrap(),
        Progress::WouldBlock
    );
    // Advance beyond 127ms to allow next CF.
    let later = t0 + Duration::from_millis(200);
    assert_ne!(
        sender.poll_send(&payload, later).unwrap(),
        Progress::WouldBlock
    );
}

#[test]
fn bad_sequence_number_is_rejected() {
    let bus = BusHandle::new();
    let can_rx = MockCan::new_with_bus(&bus, vec![]).unwrap();
    let injector = bus.add_interface(vec![]).unwrap();
    let (tx_b, rx_b) = can_rx.split();
    let mut receiver = IsoTpNode::with_clock_and_storage(
        tx_b,
        rx_b,
        cfg(0x601, 0x600, 0, 64),
        StdClock,
        RxStorage::Owned(vec![0u8; 64]),
    )
    .unwrap();

    // First frame claims total len 10 with six bytes in payload.
    let ff: MockFrame = encode(
        Id::Standard(StandardId::new(0x600).unwrap()),
        &Pdu::FirstFrame {
            len: 10,
            data: &[1, 2, 3, 4, 5, 6],
        },
        None,
    )
    .unwrap();
    injector.transmit(ff).unwrap();
    assert_eq!(
        receiver.poll_recv(Instant::now(), &mut |_| {}).unwrap(),
        Progress::InFlight
    );

    // Send CF with wrong sequence number.
    let cf_bad: MockFrame = encode(
        Id::Standard(StandardId::new(0x600).unwrap()),
        &Pdu::ConsecutiveFrame {
            sn: 3,
            data: &[7, 8, 9, 10],
        },
        None,
    )
    .unwrap();
    injector.transmit(cf_bad).unwrap();
    let err = receiver.poll_recv(Instant::now(), &mut |_| {});
    assert!(matches!(err, Err(can_iso_tp::IsoTpError::BadSequence)));
}
