use can_iso_tp::pdu::Pdu;
use can_iso_tp::rx::{RxMachine, RxOutcome, RxState, RxStorage};
use can_iso_tp::{IsoTpConfig, IsoTpError, RxFlowControl};

fn cfg_with_limits(max_payload_len: usize) -> IsoTpConfig {
    IsoTpConfig {
        max_payload_len,
        ..IsoTpConfig::default()
    }
}

#[test]
fn rx_machine_borrowed_buffer_happy_path_and_helpers() {
    let cfg = cfg_with_limits(64);
    let rx_fc = RxFlowControl::from_config(&cfg);
    let mut buf = [0u8; 8];
    let mut rx = RxMachine::new(RxStorage::Borrowed(&mut buf));

    let out = rx
        .on_pdu(
            &cfg,
            &rx_fc,
            Pdu::SingleFrame {
                len: 2,
                data: &[0xAA, 0xBB],
            },
        )
        .unwrap();
    assert!(matches!(out, RxOutcome::Completed(2)));
    assert_eq!(rx.take_completed(), &[0xAA, 0xBB]);

    // Dead-code helpers should still behave.
    assert_eq!(rx.buffer(), &[0xAA, 0xBB]);
    assert_eq!(rx.buffer_mut().len(), 8);

    rx.reset();
    assert_eq!(rx.state, RxState::Idle);
}

#[test]
fn rx_machine_reports_errors_for_unexpected_or_overflow_pdus() {
    let cfg = cfg_with_limits(4);
    let rx_fc = RxFlowControl::from_config(&cfg);
    let mut buf = [0u8; 2];
    let mut rx = RxMachine::new(RxStorage::Borrowed(&mut buf));

    // SingleFrame while not idle should resync and accept the new single frame.
    rx.state = RxState::Receiving;
    let out = rx
        .on_pdu(
            &cfg,
            &rx_fc,
            Pdu::SingleFrame {
                len: 1,
                data: &[0x00],
            },
        )
        .unwrap();
    assert!(matches!(out, RxOutcome::Completed(1)));
    assert_eq!(rx.take_completed(), &[0x00]);

    // SingleFrame overflow (len > capacity).
    rx.state = RxState::Idle;
    assert!(matches!(
        rx.on_pdu(
            &cfg,
            &rx_fc,
            Pdu::SingleFrame {
                len: 3,
                data: &[0x00, 0x01, 0x02]
            }
        ),
        Err(IsoTpError::Overflow)
    ));

    // FirstFrame while not idle should resync and accept the new transfer.
    rx.state = RxState::Receiving;
    let out = rx
        .on_pdu(
            &cfg,
            &rx_fc,
            Pdu::FirstFrame {
                len: 2,
                data: &[0x00; 6],
            },
        )
        .unwrap();
    assert!(matches!(out, RxOutcome::SendFlowControl { .. }));

    // ConsecutiveFrame while not receiving.
    rx.state = RxState::Idle;
    assert!(matches!(
        rx.on_pdu(
            &cfg,
            &rx_fc,
            Pdu::ConsecutiveFrame {
                sn: 1,
                data: &[0x00]
            }
        ),
        Err(IsoTpError::UnexpectedPdu)
    ));

    // Bad sequence.
    rx.reset();
    let out = rx
        .on_pdu(
            &cfg,
            &rx_fc,
            Pdu::FirstFrame {
                len: 2,
                data: &[0xAA; 6],
            },
        )
        .unwrap();
    assert!(matches!(out, RxOutcome::SendFlowControl { .. }));
    assert!(matches!(
        rx.on_pdu(
            &cfg,
            &rx_fc,
            Pdu::ConsecutiveFrame {
                sn: 2,
                data: &[0x00]
            }
        ),
        Err(IsoTpError::BadSequence)
    ));
}

#[test]
fn rx_buffer_owned_as_ref_and_as_mut_paths_are_exercised() {
    let cfg = cfg_with_limits(64);
    let rx_fc = RxFlowControl::from_config(&cfg);
    let mut rx = RxMachine::new(RxStorage::Owned(vec![0u8; 8]));
    let out = rx
        .on_pdu(
            &cfg,
            &rx_fc,
            Pdu::SingleFrame {
                len: 2,
                data: &[0xAA, 0xBB],
            },
        )
        .unwrap();
    assert!(matches!(out, RxOutcome::Completed(2)));
    assert_eq!(rx.take_completed(), &[0xAA, 0xBB]);
    assert_eq!(rx.buffer(), &[0xAA, 0xBB]);
    assert_eq!(rx.buffer_mut().len(), 8);
}

#[test]
fn rx_machine_flow_control_uses_runtime_rx_flow_control() {
    let cfg = cfg_with_limits(64);
    let rx_fc = RxFlowControl {
        block_size: 0x22,
        st_min: core::time::Duration::from_millis(50),
    };
    let mut buf = [0u8; 64];
    let mut rx = RxMachine::new(RxStorage::Borrowed(&mut buf));

    let out = rx
        .on_pdu(
            &cfg,
            &rx_fc,
            Pdu::FirstFrame {
                len: 20,
                data: &[0x11; 6],
            },
        )
        .unwrap();
    match out {
        RxOutcome::SendFlowControl {
            status,
            block_size,
            st_min,
        } => {
            assert_eq!(status, can_iso_tp::pdu::FlowStatus::ClearToSend);
            assert_eq!(block_size, 0x22);
            assert_eq!(st_min, 50);
        }
        _ => panic!("expected flow control on first frame"),
    }
}

#[test]
fn rx_machine_flow_control_picks_up_updates_at_block_boundaries() {
    let mut cfg = cfg_with_limits(64);
    cfg.block_size = 2;
    cfg.st_min = core::time::Duration::from_millis(0);

    let mut rx_fc = RxFlowControl::from_config(&cfg);
    let mut buf = [0u8; 64];
    let mut rx = RxMachine::new(RxStorage::Borrowed(&mut buf));

    // Start a segmented transfer (len > 7 so FF is valid for classic CAN).
    let out = rx
        .on_pdu(
            &cfg,
            &rx_fc,
            Pdu::FirstFrame {
                len: 30,
                data: &[0xAA; 6],
            },
        )
        .unwrap();
    assert!(matches!(out, RxOutcome::SendFlowControl { .. }));

    // Consume one CF (block_remaining becomes 1).
    let out = rx
        .on_pdu(
            &cfg,
            &rx_fc,
            Pdu::ConsecutiveFrame {
                sn: 1,
                data: &[0xBB; 7],
            },
        )
        .unwrap();
    assert!(matches!(out, RxOutcome::None));

    // Update runtime FC before the next block boundary.
    rx_fc.block_size = 1;
    rx_fc.st_min = core::time::Duration::from_millis(10);

    // Next CF hits the block boundary and should request the updated FC parameters.
    let out = rx
        .on_pdu(
            &cfg,
            &rx_fc,
            Pdu::ConsecutiveFrame {
                sn: 2,
                data: &[0xCC; 7],
            },
        )
        .unwrap();
    match out {
        RxOutcome::SendFlowControl {
            status,
            block_size,
            st_min,
        } => {
            assert_eq!(status, can_iso_tp::pdu::FlowStatus::ClearToSend);
            assert_eq!(block_size, 1);
            assert_eq!(st_min, 10);
        }
        _ => panic!("expected flow control at block boundary"),
    }
}
