use can_iso_tp::{IsoTpConfig, IsoTpNode, Progress, RxStorage, StdClock};
use embedded_can::{Frame, StandardId};
use embedded_can_interface::{Id, SplitTxRx};
use embedded_can_mock::{BusHandle, MockCan, MockFrame};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

fn cfg(tx: u16, rx: u16, block_size: u8) -> IsoTpConfig {
    IsoTpConfig {
        tx_id: Id::Standard(StandardId::new(tx).unwrap()),
        rx_id: Id::Standard(StandardId::new(rx).unwrap()),
        tx_addr: None,
        rx_addr: None,
        block_size,
        st_min: Duration::from_millis(0),
        wft_max: 3,
        padding: None,
        max_payload_len: 256,
        n_as: Duration::from_millis(500),
        n_ar: Duration::from_millis(500),
        n_bs: Duration::from_millis(500),
        n_br: Duration::from_millis(500),
        n_cs: Duration::from_millis(500),
        frame_len: 8,
    }
}

#[test]
fn single_frame_roundtrip() {
    let bus = BusHandle::new();
    let can_a = MockCan::new_with_bus(&bus, vec![]).unwrap();
    let can_b = MockCan::new_with_bus(&bus, vec![]).unwrap();

    let (tx_a, rx_a) = can_a.split();
    let (tx_b, rx_b) = can_b.split();

    let mut sender = IsoTpNode::with_clock_and_storage(
        tx_a,
        rx_a,
        cfg(0x100, 0x101, 0),
        StdClock,
        RxStorage::Owned(vec![0u8; 256]),
    )
    .unwrap();
    let mut receiver = IsoTpNode::with_clock_and_storage(
        tx_b,
        rx_b,
        cfg(0x101, 0x100, 0),
        StdClock,
        RxStorage::Owned(vec![0u8; 256]),
    )
    .unwrap();

    let payload = [1u8, 2, 3, 4];
    sender
        .send(&payload, Duration::from_millis(200))
        .expect("send");

    let mut delivered = Vec::new();
    receiver
        .recv(Duration::from_millis(200), &mut |data| {
            delivered = data.to_vec()
        })
        .expect("recv");

    assert_eq!(payload.to_vec(), delivered);
}

#[test]
fn extended_addressing_roundtrip_payload_len_7() {
    let bus = BusHandle::new();
    let can_a = MockCan::new_with_bus(&bus, vec![]).unwrap();
    let can_b = MockCan::new_with_bus(&bus, vec![]).unwrap();

    let (tx_a, rx_a) = can_a.split();
    let (tx_b, rx_b) = can_b.split();

    let mut cfg_a = cfg(0x120, 0x121, 0);
    cfg_a.tx_addr = Some(0xAA);
    cfg_a.rx_addr = Some(0x55);
    let mut cfg_b = cfg(0x121, 0x120, 0);
    cfg_b.tx_addr = Some(0x55);
    cfg_b.rx_addr = Some(0xAA);

    let mut sender = IsoTpNode::with_clock_and_storage(
        tx_a,
        rx_a,
        cfg_a,
        StdClock,
        RxStorage::Owned(vec![0u8; 256]),
    )
    .unwrap();
    let mut receiver = IsoTpNode::with_clock_and_storage(
        tx_b,
        rx_b,
        cfg_b,
        StdClock,
        RxStorage::Owned(vec![0u8; 256]),
    )
    .unwrap();

    let payload = [1u8, 2, 3, 4, 5, 6, 7];
    let mut delivered = Vec::new();
    let start = Instant::now();
    let mut send_done = false;
    let mut recv_done = false;
    let mut iterations = 0;

    while !(send_done && recv_done) {
        iterations += 1;
        assert!(iterations < 1000, "state machine stuck");
        let now = Instant::now();

        if !send_done
            && matches!(
                sender.poll_send(&payload, now).expect("send progress"),
                Progress::Completed
            )
        {
            send_done = true;
        }

        match receiver.poll_recv(now, &mut |data| delivered = data.to_vec()) {
            Ok(Progress::Completed) => recv_done = true,
            Ok(_) => {}
            Err(err) => panic!("recv error: {:?}", err),
        }

        assert!(
            start.elapsed() < Duration::from_secs(2),
            "timeout waiting for completion"
        );
    }

    assert_eq!(payload.to_vec(), delivered);
}

#[test]
fn multi_frame_flow_control_path() {
    let bus = BusHandle::new();
    let can_a = MockCan::new_with_bus(&bus, vec![]).unwrap();
    let can_b = MockCan::new_with_bus(&bus, vec![]).unwrap();

    let (tx_a, rx_a) = can_a.split();
    let (tx_b, rx_b) = can_b.split();

    let mut sender = IsoTpNode::with_clock_and_storage(
        tx_a,
        rx_a,
        cfg(0x200, 0x201, 4),
        StdClock,
        RxStorage::Owned(vec![0u8; 256]),
    )
    .unwrap();
    let mut receiver = IsoTpNode::with_clock_and_storage(
        tx_b,
        rx_b,
        cfg(0x201, 0x200, 4),
        StdClock,
        RxStorage::Owned(vec![0u8; 256]),
    )
    .unwrap();

    let payload: Vec<u8> = (0u8..20).collect();
    let mut delivered = Vec::new();
    let start = Instant::now();

    let mut send_done = false;
    let mut recv_done = false;
    let mut iterations = 0;

    while !(send_done && recv_done) {
        iterations += 1;
        assert!(iterations < 1000, "state machine stuck");
        let now = Instant::now();
        if !send_done
            && matches!(
                sender.poll_send(&payload, now).expect("send progress"),
                Progress::Completed
            )
        {
            send_done = true;
        }
        match receiver.poll_recv(now, &mut |data| delivered = data.to_vec()) {
            Ok(Progress::Completed) => recv_done = true,
            Ok(_) => {}
            Err(err) => panic!("recv error: {:?}", err),
        }
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "timeout waiting for completion"
        );
    }

    assert_eq!(payload, delivered);
}

#[test]
fn long_payload_over_multiple_nodes() {
    let bus = BusHandle::new();
    let can_a = MockCan::new_with_bus(&bus, vec![]).unwrap();
    let can_b = MockCan::new_with_bus(&bus, vec![]).unwrap();
    let can_c = MockCan::new_with_bus(&bus, vec![]).unwrap();

    let (tx_a, rx_a) = can_a.split();
    let (tx_b, rx_b) = can_b.split();
    let (tx_c, rx_c) = can_c.split();

    let mut sender = IsoTpNode::with_clock_and_storage(
        tx_a,
        rx_a,
        cfg(0x300, 0x301, 8),
        StdClock,
        RxStorage::Owned(vec![0u8; 256]),
    )
    .unwrap();
    let mut receiver_b = IsoTpNode::with_clock_and_storage(
        tx_b,
        rx_b,
        cfg(0x301, 0x300, 8),
        StdClock,
        RxStorage::Owned(vec![0u8; 256]),
    )
    .unwrap();
    let mut receiver_c = IsoTpNode::with_clock_and_storage(
        tx_c,
        rx_c,
        cfg(0x301, 0x300, 8),
        StdClock,
        RxStorage::Owned(vec![0u8; 256]),
    )
    .unwrap();

    let payload: Vec<u8> = (0..200u16).map(|v| (v & 0xFF) as u8).collect();
    let mut delivered_b = Vec::new();
    let mut delivered_c = Vec::new();

    // Multi-frame transfers require interleaving sender/receiver polling so FlowControl can be
    // generated and consumed.
    let start = Instant::now();
    let mut send_done = false;
    let mut recv_b_done = false;
    let mut recv_c_done = false;
    let mut iterations = 0;

    while !(send_done && recv_b_done && recv_c_done) {
        iterations += 1;
        assert!(iterations < 5000, "state machine stuck");
        let now = Instant::now();

        if !send_done
            && matches!(
                sender.poll_send(&payload, now).expect("send long progress"),
                Progress::Completed
            )
        {
            send_done = true;
        }
        if !recv_b_done {
            match receiver_b.poll_recv(now, &mut |data| delivered_b = data.to_vec()) {
                Ok(Progress::Completed) => recv_b_done = true,
                Ok(_) => {}
                Err(err) => panic!("recv b error: {:?}", err),
            }
        }
        if !recv_c_done {
            match receiver_c.poll_recv(now, &mut |data| delivered_c = data.to_vec()) {
                Ok(Progress::Completed) => recv_c_done = true,
                Ok(_) => {}
                Err(err) => panic!("recv c error: {:?}", err),
            }
        }

        assert!(
            start.elapsed() < Duration::from_secs(5),
            "timeout waiting for completion"
        );
    }

    assert_eq!(payload, delivered_b);
    assert_eq!(payload, delivered_c);
}

#[test]
fn back_to_back_long_messages() {
    let bus = BusHandle::new();
    let can_a = MockCan::new_with_bus(&bus, vec![]).unwrap();
    let can_b = MockCan::new_with_bus(&bus, vec![]).unwrap();

    let (tx_a, rx_a) = can_a.split();
    let (tx_b, rx_b) = can_b.split();

    let mut sender = IsoTpNode::with_clock_and_storage(
        tx_a,
        rx_a,
        cfg(0x400, 0x401, 4),
        StdClock,
        RxStorage::Owned(vec![0u8; 256]),
    )
    .unwrap();
    let mut receiver = IsoTpNode::with_clock_and_storage(
        tx_b,
        rx_b,
        cfg(0x401, 0x400, 4),
        StdClock,
        RxStorage::Owned(vec![0u8; 256]),
    )
    .unwrap();

    let payload1: Vec<u8> = (0..120u16).map(|v| (v & 0xFF) as u8).collect();
    let payload2: Vec<u8> = (120..260u16).map(|v| (v & 0xFF) as u8).collect();

    let start = Instant::now();
    let mut send1_done = false;
    let mut send2_done = false;
    let mut delivered = Vec::new();
    let mut iterations = 0;

    while !(send1_done && send2_done && delivered.len() == 2) {
        iterations += 1;
        assert!(iterations < 5000, "state machine stuck");
        let now = Instant::now();

        if !send1_done {
            if matches!(
                sender.poll_send(&payload1, now).expect("send1 poll"),
                Progress::Completed
            ) {
                send1_done = true;
            }
        } else if !send2_done
            && matches!(
                sender.poll_send(&payload2, now).expect("send2 poll"),
                Progress::Completed
            )
        {
            send2_done = true;
        }

        match receiver.poll_recv(now, &mut |data| delivered.push(data.to_vec())) {
            Ok(_) => {}
            Err(err) => panic!("recv poll: {:?}", err),
        }

        assert!(
            start.elapsed() < Duration::from_secs(5),
            "timeout waiting for completion"
        );
    }

    assert_eq!(payload1, delivered[0]);
    assert_eq!(payload2, delivered[1]);
}

#[test]
fn bus_contention_with_noise_frames() {
    let bus = BusHandle::new();
    let can_a = MockCan::new_with_bus(&bus, vec![]).unwrap();
    let can_b = MockCan::new_with_bus(&bus, vec![]).unwrap();
    let noise = bus.add_interface(vec![]).unwrap();

    let (tx_a, rx_a) = can_a.split();
    let (tx_b, rx_b) = can_b.split();

    let mut sender = IsoTpNode::with_clock_and_storage(
        tx_a,
        rx_a,
        cfg(0x500, 0x501, 4),
        StdClock,
        RxStorage::Owned(vec![0u8; 256]),
    )
    .unwrap();
    let mut receiver = IsoTpNode::with_clock_and_storage(
        tx_b,
        rx_b,
        cfg(0x501, 0x500, 4),
        StdClock,
        RxStorage::Owned(vec![0u8; 256]),
    )
    .unwrap();

    let payload: Vec<u8> = (0..96u16).map(|v| (v & 0xFF) as u8).collect();
    let mut delivered = Vec::new();
    let mut send_done = false;
    let mut recv_done = false;
    let mut iterations = 0;

    while !(send_done && recv_done) {
        iterations += 1;
        assert!(iterations < 2000, "stuck during contention test");
        let now = Instant::now();

        // Inject unrelated noise periodically.
        if iterations % 3 == 0 {
            let noise_frame = MockFrame::new(
                embedded_can::Id::Standard(StandardId::new(0x777).unwrap()),
                &[0xDE, 0xAD, 0xBE, 0xEF],
            )
            .unwrap();
            noise.transmit(noise_frame).unwrap();
        }

        if !send_done
            && matches!(
                sender.poll_send(&payload, now).expect("send poll"),
                Progress::Completed
            )
        {
            send_done = true;
        }

        match receiver.poll_recv(now, &mut |data| delivered = data.to_vec()) {
            Ok(Progress::Completed) => recv_done = true,
            Ok(_) => {}
            Err(err) => panic!("recv failed under contention: {:?}", err),
        }
    }

    assert_eq!(payload, delivered);
}

#[test]
fn blocking_multi_frame_roundtrip() {
    let bus = BusHandle::new();
    let can_a = MockCan::new_with_bus(&bus, vec![]).unwrap();
    let can_b = MockCan::new_with_bus(&bus, vec![]).unwrap();

    let (tx_a, rx_a) = can_a.split();
    let (tx_b, rx_b) = can_b.split();

    let mut sender = IsoTpNode::with_clock_and_storage(
        tx_a,
        rx_a,
        cfg(0x680, 0x681, 4),
        StdClock,
        RxStorage::Owned(vec![0u8; 256]),
    )
    .unwrap();
    let mut receiver: IsoTpNode<'static, _, _, _, _> = IsoTpNode::with_clock_and_storage(
        tx_b,
        rx_b,
        cfg(0x681, 0x680, 4),
        StdClock,
        RxStorage::Owned(vec![0u8; 256]),
    )
    .unwrap();

    let payload: Vec<u8> = (0..96u16).map(|v| (v & 0xFF) as u8).collect();
    let (done_tx, done_rx) = mpsc::channel();

    let recv_thread = thread::spawn(move || {
        let mut delivered = Vec::new();
        let res = receiver.recv(Duration::from_secs(2), &mut |data| {
            delivered = data.to_vec()
        });
        done_tx.send((res, delivered)).unwrap();
    });

    sender
        .send(&payload, Duration::from_secs(2))
        .expect("blocking send");

    let (recv_res, delivered) = done_rx.recv_timeout(Duration::from_secs(3)).unwrap();
    recv_res.expect("blocking recv");
    assert_eq!(payload, delivered);

    recv_thread.join().unwrap();
}

#[test]
fn blocking_recv_times_out_without_sender() {
    let bus = BusHandle::new();
    let can = MockCan::new_with_bus(&bus, vec![]).unwrap();
    let (tx, rx) = can.split();
    let mut receiver = IsoTpNode::with_clock_and_storage(
        tx,
        rx,
        cfg(0x682, 0x683, 0),
        StdClock,
        RxStorage::Owned(vec![0u8; 256]),
    )
    .unwrap();

    let err = receiver.recv(Duration::from_millis(50), &mut |_| {});
    assert!(matches!(
        err,
        Err(can_iso_tp::IsoTpError::Timeout(
            can_iso_tp::TimeoutKind::NAr
        ))
    ));
}

#[test]
fn blocking_send_times_out_without_receiver_polling() {
    let bus = BusHandle::new();
    let can_a = MockCan::new_with_bus(&bus, vec![]).unwrap();
    let can_b = MockCan::new_with_bus(&bus, vec![]).unwrap();

    let (tx_a, rx_a) = can_a.split();
    let (_tx_b, _rx_b) = can_b.split();
    let mut sender = IsoTpNode::with_clock_and_storage(
        tx_a,
        rx_a,
        cfg(0x684, 0x685, 4),
        StdClock,
        RxStorage::Owned(vec![0u8; 256]),
    )
    .unwrap();

    let payload: Vec<u8> = (0..64u16).map(|v| (v & 0xFF) as u8).collect();
    let err = sender.send(&payload, Duration::from_millis(50));
    assert!(matches!(
        err,
        Err(can_iso_tp::IsoTpError::Timeout(
            can_iso_tp::TimeoutKind::NAs
        ))
    ));
}

#[test]
fn blocking_send_and_recv_report_overflow() {
    let bus = BusHandle::new();
    let can_a = MockCan::new_with_bus(&bus, vec![]).unwrap();
    let can_b = MockCan::new_with_bus(&bus, vec![]).unwrap();

    let (tx_a, rx_a) = can_a.split();
    let (tx_b, rx_b) = can_b.split();

    let mut sender = IsoTpNode::with_clock_and_storage(
        tx_a,
        rx_a,
        cfg(0x686, 0x687, 4),
        StdClock,
        RxStorage::Owned(vec![0u8; 256]),
    )
    .unwrap();
    let mut receiver: IsoTpNode<'static, _, _, _, _> = {
        let mut cfg = cfg(0x687, 0x686, 4);
        cfg.max_payload_len = 8;
        IsoTpNode::with_clock_and_storage(tx_b, rx_b, cfg, StdClock, RxStorage::Owned(vec![0u8; 8]))
            .unwrap()
    };

    let payload: Vec<u8> = (0..32u16).map(|v| (v & 0xFF) as u8).collect();
    let (done_tx, done_rx) = mpsc::channel();

    let recv_thread = thread::spawn(move || {
        let res = receiver.recv(Duration::from_secs(1), &mut |_| {});
        done_tx.send(res).unwrap();
    });

    let send_err = sender.send(&payload, Duration::from_secs(1));
    assert!(matches!(send_err, Err(can_iso_tp::IsoTpError::Overflow)));

    let recv_err = done_rx.recv_timeout(Duration::from_secs(2)).unwrap();
    assert!(matches!(recv_err, Err(can_iso_tp::IsoTpError::RxOverflow)));

    recv_thread.join().unwrap();
}

#[test]
fn two_parallel_transfers_share_bus() {
    let bus = BusHandle::new();
    let can_a = MockCan::new_with_bus(&bus, vec![]).unwrap();
    let can_b = MockCan::new_with_bus(&bus, vec![]).unwrap();
    let can_c = MockCan::new_with_bus(&bus, vec![]).unwrap();
    let can_d = MockCan::new_with_bus(&bus, vec![]).unwrap();

    let (tx_a, rx_a) = can_a.split();
    let (tx_b, rx_b) = can_b.split();
    let (tx_c, rx_c) = can_c.split();
    let (tx_d, rx_d) = can_d.split();

    let mut sender1 = IsoTpNode::with_clock_and_storage(
        tx_a,
        rx_a,
        cfg(0x600, 0x601, 4),
        StdClock,
        RxStorage::Owned(vec![0u8; 256]),
    )
    .unwrap();
    let mut receiver1 = IsoTpNode::with_clock_and_storage(
        tx_b,
        rx_b,
        cfg(0x601, 0x600, 4),
        StdClock,
        RxStorage::Owned(vec![0u8; 256]),
    )
    .unwrap();
    let mut sender2 = IsoTpNode::with_clock_and_storage(
        tx_c,
        rx_c,
        cfg(0x700, 0x701, 4),
        StdClock,
        RxStorage::Owned(vec![0u8; 256]),
    )
    .unwrap();
    let mut receiver2 = IsoTpNode::with_clock_and_storage(
        tx_d,
        rx_d,
        cfg(0x701, 0x700, 4),
        StdClock,
        RxStorage::Owned(vec![0u8; 256]),
    )
    .unwrap();

    let payload1: Vec<u8> = (0..80u16).map(|v| (v & 0xFF) as u8).collect();
    let payload2: Vec<u8> = (0..140u16).map(|v| (v & 0xFF) as u8).collect();

    let mut delivered1 = Vec::new();
    let mut delivered2 = Vec::new();
    let mut send1_done = false;
    let mut recv1_done = false;
    let mut send2_done = false;
    let mut recv2_done = false;
    let mut iterations = 0;

    while !(send1_done && recv1_done && send2_done && recv2_done) {
        iterations += 1;
        assert!(iterations < 5000, "parallel transfers stuck");
        let now = Instant::now();

        if !send1_done
            && matches!(
                sender1.poll_send(&payload1, now).expect("sender1 poll"),
                Progress::Completed
            )
        {
            send1_done = true;
        }
        if !send2_done
            && matches!(
                sender2.poll_send(&payload2, now).expect("sender2 poll"),
                Progress::Completed
            )
        {
            send2_done = true;
        }

        match receiver1.poll_recv(now, &mut |data| delivered1 = data.to_vec()) {
            Ok(Progress::Completed) => recv1_done = true,
            Ok(_) => {}
            Err(err) => panic!("receiver1 failed: {:?}", err),
        }
        match receiver2.poll_recv(now, &mut |data| delivered2 = data.to_vec()) {
            Ok(Progress::Completed) => recv2_done = true,
            Ok(_) => {}
            Err(err) => panic!("receiver2 failed: {:?}", err),
        }
    }

    assert_eq!(payload1, delivered1);
    assert_eq!(payload2, delivered2);
}
