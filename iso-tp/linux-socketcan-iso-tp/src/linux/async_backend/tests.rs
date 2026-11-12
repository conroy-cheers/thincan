use super::*;
use can_isotp_interface::{IsoTpAsyncReadSend, IsoTpAsyncWriteSend, TransmitError};
use std::os::unix::net::UnixDatagram;

fn pair(closer: &SocketCloser) -> (SocketParts, UnixDatagram) {
    let (local, peer) = UnixDatagram::pair().unwrap();
    local.set_nonblocking(true).unwrap();
    peer.set_nonblocking(true).unwrap();
    let status = Arc::new(Status::new());
    let deferred = DeferredSocket {
        socket: Some(SocketCanIsoTp {
            fd: local.into(),
            rx_buf: Vec::new(),
        }),
        permit: Some(closer.acquire().unwrap()),
        jobs: closer.jobs.clone(),
        status: status.clone(),
    };
    let socket = TokioSocketCanIsoTp {
        shared: Arc::new(Shared {
            io: AsyncFd::new(deferred).unwrap(),
            status,
            route: SocketRoute {
                receive_from: 0,
                transmit_to: Destination::Physical(3),
            },
        }),
    };
    (socket.into_split(), peer)
}
fn deadline() -> tokio::time::Instant {
    tokio::time::Instant::now() + Duration::from_secs(1)
}
#[tokio::test]
async fn routing_truncation_and_empty_buffers_consume_exactly_one_datagram() {
    let closer = SocketCloser::new(1, 1).unwrap();
    let (mut parts, peer) = pair(&closer);
    peer.send(&[1, 2, 3]).unwrap();
    assert!(matches!(
        parts.reader.recv_into(&mut []).await,
        Err(ReceiveError::BufferTooSmall { needed: 3, got: 0 })
    ));
    peer.send(&[4, 5, 6]).unwrap();
    assert!(matches!(
        parts.reader.recv_into(&mut [0; 1]).await,
        Err(ReceiveError::BufferTooSmall { needed: 3, got: 1 })
    ));
    peer.send(&[7]).unwrap();
    let mut out = [0; 8];
    assert_eq!(
        parts
            .reader
            .recv_into(&mut out)
            .await
            .unwrap()
            .meta
            .reply_to,
        0
    );
    assert_eq!(out[0], 7);
    assert!(matches!(
        parts
            .writer
            .send(Destination::Functional(3), &[9], deadline())
            .await,
        Err(TransmitError::Rejected(_))
    ));
    assert!(peer.recv(&mut out).is_err());
    parts
        .writer
        .send(Destination::Physical(3), &[9], deadline())
        .await
        .unwrap();
    assert_eq!(peer.recv(&mut out).unwrap(), 1);
    assert_eq!(out[0], 9);
}
#[tokio::test]
async fn deadline_expiry_starts_no_syscall_and_cancelled_receive_keeps_data() {
    let closer = SocketCloser::new(1, 1).unwrap();
    let (mut parts, peer) = pair(&closer);
    assert!(matches!(
        parts
            .writer
            .send(Destination::Physical(3), &[9], tokio::time::Instant::now())
            .await,
        Err(TransmitError::Timeout)
    ));
    let mut out = [0; 8];
    assert!(peer.recv(&mut out).is_err());
    assert!(
        tokio::time::timeout(Duration::from_millis(5), parts.reader.recv_into(&mut out))
            .await
            .is_err()
    );
    peer.send(&[7]).unwrap();
    assert_eq!(parts.reader.recv_into(&mut out).await.unwrap().len, 1);
    assert_eq!(out[0], 7);
}
#[tokio::test]
async fn first_failure_is_latched_and_invalidates_both_halves() {
    let closer = SocketCloser::new(1, 1).unwrap();
    let (mut parts, peer) = pair(&closer);
    peer.send(&[]).unwrap();
    let error = match parts.reader.recv_into(&mut [0; 8]).await {
        Err(ReceiveError::Fatal(error)) => error,
        _ => panic!("zero datagram must be fatal"),
    };
    let failure = parts.control.failed().await.unwrap();
    assert_eq!(failure.operation, Operation::Receive);
    assert!(Arc::ptr_eq(&error, &failure.error));
    assert!(matches!(
        parts
            .writer
            .send(Destination::Physical(3), &[1], deadline())
            .await,
        Err(TransmitError::Closed)
    ));
    assert!(matches!(
        parts.reader.recv_into(&mut [0; 8]).await,
        Err(ReceiveError::Closed)
    ));
    let same = parts
        .reader
        .shared
        .status
        .fail(Operation::Send, Error::InvalidConfig("later error"));
    assert!(Arc::ptr_eq(&same, &error));
    let control = parts.control;
    drop(parts.reader);
    drop(parts.writer);
    tokio::time::timeout(Duration::from_secs(1), control.closed())
        .await
        .unwrap();
    assert_eq!(closer.occupied(), 0);
}
#[tokio::test]
async fn closer_admission_reserves_capacity_and_outlives_its_public_handle() {
    let closer = SocketCloser::new(1, 1).unwrap();
    let (parts, _peer) = pair(&closer);
    assert!(matches!(closer.acquire(), Err(Error::CloseCapacity)));
    assert_eq!(closer.occupied(), 1);
    let control = parts.control;
    drop(closer);
    drop(parts.reader);
    drop(parts.writer);
    tokio::time::timeout(Duration::from_secs(1), control.closed())
        .await
        .unwrap();
    assert!(control.is_closed());
}
#[test]
fn error_classification_is_operation_specific() {
    assert!(transaction_error(&io::Error::from_raw_os_error(
        libc::EILSEQ
    )));
    assert!(!transaction_error(&io::Error::from_raw_os_error(
        libc::EINVAL
    )));
    assert!(!transaction_error(&io::Error::from_raw_os_error(
        libc::ENODEV
    )));
    // Send also rejects (rather than failing the socket on) a bad request or a
    // full CAN transmit queue.
    for errno in [libc::EILSEQ, libc::EINVAL, libc::ENOBUFS] {
        assert!(send_rejection(&io::Error::from_raw_os_error(errno)));
    }
    assert!(!send_rejection(&io::Error::from_raw_os_error(libc::ENODEV)));
}
