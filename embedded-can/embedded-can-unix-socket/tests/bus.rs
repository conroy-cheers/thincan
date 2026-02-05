use embedded_can::Frame as _;
use embedded_can::{Id, StandardId};
use embedded_can_interface::{FilterConfig, IdMask, IdMaskFilter, RxFrameIo, TxFrameIo};
use embedded_can_unix_socket::{BusServer, UnixCan, UnixCanError, UnixFrame};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

static SOCKET_COUNTER: AtomicU64 = AtomicU64::new(0);

fn unique_socket_path() -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let counter = SOCKET_COUNTER.fetch_add(1, Ordering::Relaxed);
    PathBuf::from("/tmp").join(format!(
        "can-uds-{}-{}-{}.sock",
        std::process::id(),
        nanos,
        counter
    ))
}

fn start_server() -> (BusServer, PathBuf) {
    let path = unique_socket_path();
    let server = BusServer::start(&path).expect("server start");
    (server, path)
}

fn connect_with_retry(path: &PathBuf) -> UnixCan {
    for _ in 0..50 {
        if let Ok(client) = UnixCan::connect(path) {
            return client;
        }
        thread::sleep(Duration::from_millis(5));
    }
    UnixCan::connect(path).expect("connect")
}

#[test]
fn broadcasts_to_all_clients() {
    let (mut server, path) = start_server();

    let mut a = connect_with_retry(&path);
    let mut b = connect_with_retry(&path);

    let frame = UnixFrame::new(Id::Standard(StandardId::new(0x100).unwrap()), &[1, 2]).unwrap();
    a.send(&frame).unwrap();

    let recv_b = b.recv().unwrap();
    assert_eq!(recv_b, frame);

    let recv_a = a.recv().unwrap();
    assert_eq!(recv_a, frame);

    server.shutdown().unwrap();
}

#[test]
fn filters_drop_non_matching_frames() {
    let (mut server, path) = start_server();

    let mut sender = connect_with_retry(&path);
    let mut receiver = connect_with_retry(&path);

    let filter = IdMaskFilter {
        id: embedded_can_interface::Id::Standard(StandardId::new(0x123).unwrap()),
        mask: IdMask::Standard(0x7FF),
    };
    FilterConfig::set_filters(&mut receiver, &[filter]).unwrap();

    let other = UnixFrame::new(Id::Standard(StandardId::new(0x124).unwrap()), &[9]).unwrap();
    sender.send(&other).unwrap();

    let err = receiver
        .recv_timeout(Duration::from_millis(20))
        .unwrap_err();
    assert!(matches!(err, UnixCanError::Timeout));

    let match_frame = UnixFrame::new(Id::Standard(StandardId::new(0x123).unwrap()), &[7]).unwrap();
    sender.send(&match_frame).unwrap();

    let recv = receiver.recv().unwrap();
    assert_eq!(recv, match_frame);

    server.shutdown().unwrap();
}

#[test]
fn send_requires_ack_from_another_client() {
    let (mut server, path) = start_server();

    let mut solo = connect_with_retry(&path);
    let frame = UnixFrame::new(Id::Standard(StandardId::new(0x200).unwrap()), &[1]).unwrap();
    let err = solo.send(&frame).unwrap_err();
    assert!(matches!(err, UnixCanError::NoAck));

    server.shutdown().unwrap();
}

#[test]
fn ack_ignores_filters() {
    let (mut server, path) = start_server();

    let mut sender = connect_with_retry(&path);
    let mut filtered = connect_with_retry(&path);

    let filter = IdMaskFilter {
        id: embedded_can_interface::Id::Standard(StandardId::new(0x7FF).unwrap()),
        mask: IdMask::Standard(0x7FF),
    };
    FilterConfig::set_filters(&mut filtered, &[filter]).unwrap();

    let frame = UnixFrame::new(Id::Standard(StandardId::new(0x100).unwrap()), &[3]).unwrap();
    sender.send(&frame).unwrap();

    let err = filtered
        .recv_timeout(Duration::from_millis(20))
        .unwrap_err();
    assert!(matches!(err, UnixCanError::Timeout));

    server.shutdown().unwrap();
}

#[test]
fn delivers_remote_frame() {
    let (mut server, path) = start_server();

    let mut sender = connect_with_retry(&path);
    let mut receiver = connect_with_retry(&path);

    let frame = UnixFrame::new_remote(Id::Standard(StandardId::new(0x123).unwrap()), 1).unwrap();
    sender.send(&frame).unwrap();

    let got = receiver.recv().unwrap();
    assert!(got.is_remote());
    assert_eq!(got.id(), frame.id());
    assert_eq!(got.dlc(), frame.dlc());

    server.shutdown().unwrap();
}

#[test]
fn try_recv_and_timeout_behaviors() {
    let (mut server, path) = start_server();

    let mut a = connect_with_retry(&path);
    let mut b = connect_with_retry(&path);

    let err = b.try_recv().unwrap_err();
    assert!(matches!(err, UnixCanError::WouldBlock));

    let err = b.recv_timeout(Duration::from_millis(10)).unwrap_err();
    assert!(matches!(err, UnixCanError::Timeout));

    let frame = UnixFrame::new(Id::Standard(StandardId::new(0x55).unwrap()), &[5]).unwrap();
    a.try_send(&frame).unwrap();

    let recv = b.recv().unwrap();
    assert_eq!(recv, frame);

    server.shutdown().unwrap();
}
