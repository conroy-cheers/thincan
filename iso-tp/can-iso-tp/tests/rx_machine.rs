use can_iso_tp::pdu::Pdu;
use can_iso_tp::rx::{RxMachine, RxOutcome, RxState, RxStorage};
use can_iso_tp::{IsoTpConfig, IsoTpError};

fn cfg_with_limits(max_payload_len: usize) -> IsoTpConfig {
    IsoTpConfig {
        max_payload_len,
        ..IsoTpConfig::default()
    }
}

#[test]
fn rx_machine_borrowed_buffer_happy_path_and_helpers() {
    let cfg = cfg_with_limits(64);
    let mut buf = [0u8; 8];
    let mut rx = RxMachine::new(RxStorage::Borrowed(&mut buf));

    let out = rx
        .on_pdu(
            &cfg,
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
    let mut buf = [0u8; 2];
    let mut rx = RxMachine::new(RxStorage::Borrowed(&mut buf));

    // SingleFrame while not idle.
    rx.state = RxState::Receiving;
    assert!(matches!(
        rx.on_pdu(
            &cfg,
            Pdu::SingleFrame {
                len: 1,
                data: &[0x00]
            }
        ),
        Err(IsoTpError::UnexpectedPdu)
    ));

    // SingleFrame overflow (len > capacity).
    rx.state = RxState::Idle;
    assert!(matches!(
        rx.on_pdu(
            &cfg,
            Pdu::SingleFrame {
                len: 3,
                data: &[0x00, 0x01, 0x02]
            }
        ),
        Err(IsoTpError::Overflow)
    ));

    // FirstFrame while not idle.
    rx.state = RxState::Receiving;
    assert!(matches!(
        rx.on_pdu(
            &cfg,
            Pdu::FirstFrame {
                len: 2,
                data: &[0x00; 6]
            }
        ),
        Err(IsoTpError::UnexpectedPdu)
    ));

    // ConsecutiveFrame while not receiving.
    rx.state = RxState::Idle;
    assert!(matches!(
        rx.on_pdu(
            &cfg,
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
    let mut rx = RxMachine::new(RxStorage::Owned(vec![0u8; 8]));
    let out = rx
        .on_pdu(
            &cfg,
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
