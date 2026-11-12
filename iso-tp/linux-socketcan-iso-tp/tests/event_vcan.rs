#![cfg(all(target_os = "linux", feature = "tokio"))]

use can_isotp_interface::{
    Destination, IsoTpAsyncReadSend, IsoTpAsyncWriteSend, ReceiveError, TransmitError,
};
use embedded_can::{Id, StandardId};
use linux_socketcan_iso_tp::{
    Error, IsoTpKernelOptions, SocketCloser, SocketRoute, TokioSocketCanIsoTp,
};
use std::ffi::CString;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::time::Duration;

fn test_iface() -> String {
    std::env::var("THINCAN_TEST_VCAN_IFACE")
        .expect("set THINCAN_TEST_VCAN_IFACE to an isolated vcan interface")
}

fn open(
    iface: &str,
    rx_id: Id,
    tx_id: Id,
    options: &IsoTpKernelOptions,
) -> linux_socketcan_iso_tp::SocketParts {
    let closer = SocketCloser::new(8, 2).unwrap();
    TokioSocketCanIsoTp::open_with_ids(
        &closer,
        iface,
        rx_id,
        tx_id,
        SocketRoute {
            receive_from: 0x22,
            transmit_to: Destination::Physical(0),
        },
        options,
    )
    .unwrap()
    .into_split()
}
fn deadline(duration: Duration) -> tokio::time::Instant {
    tokio::time::Instant::now().checked_add(duration).unwrap()
}
fn id(value: u16) -> Id {
    Id::Standard(StandardId::new(value).unwrap())
}

struct Injector(OwnedFd);
impl Injector {
    fn open(iface: &str) -> Self {
        // SAFETY: no pointers are supplied; a successful fd is owned below.
        let fd = unsafe {
            libc::socket(
                libc::PF_CAN,
                libc::SOCK_RAW | libc::SOCK_NONBLOCK,
                libc::CAN_RAW,
            )
        };
        assert!(
            fd >= 0,
            "raw CAN socket requires CAP_NET_RAW: {}",
            std::io::Error::last_os_error()
        );
        // SAFETY: fd is newly created and not otherwise owned.
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        let iface = CString::new(iface).unwrap();
        // SAFETY: sockaddr_can permits zero-initialization; iface is NUL-terminated.
        let mut address: libc::sockaddr_can = unsafe { std::mem::zeroed() };
        address.can_family = libc::AF_CAN as _;
        address.can_ifindex = unsafe { libc::if_nametoindex(iface.as_ptr()) } as _;
        assert!(address.can_ifindex > 0);
        // SAFETY: address points to a correctly sized sockaddr_can and fd is alive.
        let result = unsafe {
            libc::bind(
                fd.as_raw_fd(),
                (&address as *const libc::sockaddr_can).cast(),
                std::mem::size_of_val(&address) as _,
            )
        };
        assert_eq!(result, 0, "{}", std::io::Error::last_os_error());
        Self(fd)
    }

    fn send(&self, id: u32, data: &[u8]) {
        assert!(data.len() <= 8);
        // Linux classic CAN frame layout: native u32 ID, length, padding, data.
        let mut frame = [0u8; 16];
        frame[..4].copy_from_slice(&id.to_ne_bytes());
        frame[4] = data.len() as u8;
        frame[8..8 + data.len()].copy_from_slice(data);
        // SAFETY: self owns fd and frame is readable for its full length.
        let sent = unsafe {
            libc::send(
                self.0.as_raw_fd(),
                frame.as_ptr().cast(),
                frame.len(),
                libc::MSG_DONTWAIT,
            )
        };
        assert_eq!(sent, 16, "{}", std::io::Error::last_os_error());
    }
}

#[tokio::test]
#[ignore = "requires an isolated vcan interface and CAP_NET_RAW"]
async fn sequence_error_wakes_without_data_and_next_payload_succeeds() {
    let iface = test_iface();
    let mut node = open(&iface, id(0x351), id(0x352), &IsoTpKernelOptions::default());
    let injector = Injector::open(&iface);
    injector.send(0x351, &[0x10, 20, 1, 2, 3, 4, 5, 6]);
    // Sequence 2 is invalid here; the first consecutive frame must have SN 1.
    injector.send(0x351, &[0x22, 7, 8, 9, 10, 11, 12, 13]);
    let mut out = [0; 64];
    let error = tokio::time::timeout(Duration::from_millis(500), node.reader.recv_into(&mut out))
        .await
        .unwrap();
    assert!(matches!(error, Err(ReceiveError::Rejected(error))
        if match error.as_ref() { Error::Io(e)=>e.raw_os_error(), _=>None } == Some(libc::EILSEQ)));
    injector.send(0x351, &[0x02, 9, 8]);
    let received = tokio::time::timeout(Duration::from_secs(1), node.reader.recv_into(&mut out))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(&out[..received.len], &[9, 8]);
}

#[tokio::test]
#[ignore = "requires an isolated vcan interface and CAP_NET_RAW"]
async fn incomplete_receive_reports_timeout_without_data_then_recovers() {
    let iface = test_iface();
    let mut node = open(&iface, id(0x353), id(0x354), &IsoTpKernelOptions::default());
    let injector = Injector::open(&iface);
    injector.send(0x353, &[0x10, 20, 1, 2, 3, 4, 5, 6]);
    let mut out = [0; 64];
    let error = tokio::time::timeout(Duration::from_secs(2), node.reader.recv_into(&mut out))
        .await
        .unwrap();
    assert!(matches!(error, Err(ReceiveError::Rejected(error))
        if match error.as_ref() { Error::Io(e)=>e.raw_os_error(), _=>None } == Some(libc::ETIMEDOUT)));
    injector.send(0x353, &[0x01, 5]);
    let received = tokio::time::timeout(Duration::from_secs(1), node.reader.recv_into(&mut out))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(&out[..received.len], &[5]);
}

#[tokio::test]
#[ignore = "requires an isolated vcan interface and CAP_NET_RAW"]
async fn invalid_padding_reports_rejection_then_recovers() {
    let iface = test_iface();
    let mut options = IsoTpKernelOptions::default();
    options.socket.rx_padding = Some(0xAA);
    options.socket.flags = linux_socketcan_iso_tp::flags::CAN_ISOTP_CHK_PAD_LEN
        | linux_socketcan_iso_tp::flags::CAN_ISOTP_CHK_PAD_DATA;
    let mut node = open(&iface, id(0x359), id(0x35A), &options);
    let injector = Injector::open(&iface);
    injector.send(0x359, &[0x01, 7, 0, 0, 0, 0, 0, 0]);
    let mut out = [0; 64];
    let error = tokio::time::timeout(Duration::from_secs(1), node.reader.recv_into(&mut out))
        .await
        .unwrap();
    assert!(matches!(error, Err(ReceiveError::Rejected(error))
        if match error.as_ref() { Error::Io(e)=>e.raw_os_error(), _=>None } == Some(libc::EBADMSG)));
    injector.send(0x359, &[0x01, 8, 0xAA, 0xAA, 0xAA, 0xAA, 0xAA, 0xAA]);
    let received = tokio::time::timeout(Duration::from_secs(1), node.reader.recv_into(&mut out))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(&out[..received.len], &[8]);
}

#[tokio::test]
#[ignore = "requires an isolated vcan interface and CAP_NET_RAW"]
async fn flow_control_overflow_reports_rejection_then_recovers() {
    let iface = test_iface();
    let mut node = open(&iface, id(0x35B), id(0x35C), &IsoTpKernelOptions::default());
    let injector = Injector::open(&iface);
    node.writer
        .send(
            Destination::Physical(0),
            &[7; 64],
            deadline(Duration::from_secs(1)),
        )
        .await
        .unwrap();
    injector.send(0x35B, &[0x32, 0, 0]);
    let error = tokio::time::timeout(Duration::from_secs(1), node.reader.recv_into(&mut [0; 64]))
        .await
        .unwrap();
    assert!(matches!(error, Err(ReceiveError::Rejected(error))
        if match error.as_ref() { Error::Io(e)=>e.raw_os_error(), _=>None } == Some(libc::EMSGSIZE)));
    node.writer
        .send(
            Destination::Physical(0),
            &[8],
            deadline(Duration::from_secs(1)),
        )
        .await
        .unwrap();
    injector.send(0x35B, &[0x01, 9]);
    let mut out = [0; 64];
    let received = tokio::time::timeout(Duration::from_secs(1), node.reader.recv_into(&mut out))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(&out[..received.len], &[9]);
}

#[tokio::test]
#[ignore = "requires an isolated vcan interface"]
async fn rejected_send_and_flow_control_timeout_leave_the_socket_usable() {
    let iface = test_iface();
    // No peer: a multiframe send will time out waiting for flow control.
    let node = open(&iface, id(0x355), id(0x356), &IsoTpKernelOptions::default());
    let (mut reader, mut writer) = (node.reader, node.writer);
    let rejected = writer
        .send(
            Destination::Physical(0),
            &[],
            deadline(Duration::from_secs(1)),
        )
        .await;
    assert!(matches!(rejected, Err(TransmitError::Rejected(_))));
    writer
        .send(
            Destination::Physical(0),
            &[7; 64],
            deadline(Duration::from_secs(1)),
        )
        .await
        .unwrap();
    let error = tokio::time::timeout(Duration::from_secs(2), reader.recv_into(&mut [0; 64]))
        .await
        .unwrap();
    assert!(matches!(error, Err(ReceiveError::Rejected(error))
        if match error.as_ref() { Error::Io(e)=>e.raw_os_error(), _=>None } == Some(libc::ECOMM)));
    writer
        .send(
            Destination::Physical(0),
            &[1],
            deadline(Duration::from_secs(1)),
        )
        .await
        .unwrap();
}

#[tokio::test]
#[ignore = "requires an isolated vcan interface"]
async fn send_wait_observes_and_consumes_a_pending_socket_error() {
    let iface = test_iface();
    let node = open(&iface, id(0x357), id(0x358), &IsoTpKernelOptions::default());
    let mut writer = node.writer;
    writer
        .send(
            Destination::Physical(0),
            &[7; 64],
            deadline(Duration::from_secs(1)),
        )
        .await
        .unwrap();
    // With no peer, this send must wait for the previous transaction's flow
    // control timeout. The pending socket error belongs to that transaction.
    let error = writer
        .send(
            Destination::Physical(0),
            &[1],
            deadline(Duration::from_secs(2)),
        )
        .await;
    assert!(matches!(error, Err(TransmitError::Rejected(error))
        if match error.as_ref() { Error::Io(e)=>e.raw_os_error(), _=>None } == Some(libc::ECOMM)));
    writer
        .send(
            Destination::Physical(0),
            &[2],
            deadline(Duration::from_secs(1)),
        )
        .await
        .unwrap();
}

#[tokio::test]
#[ignore = "requires an isolated vcan interface in THINCAN_TEST_VCAN_IFACE"]
async fn split_kernel_isotp_transfers_multiframe_payloads_in_both_directions() {
    let iface = std::env::var("THINCAN_TEST_VCAN_IFACE")
        .expect("set THINCAN_TEST_VCAN_IFACE to an isolated vcan interface");
    let a = Id::Standard(StandardId::new(0x321).unwrap());
    let b = Id::Standard(StandardId::new(0x322).unwrap());
    let options = IsoTpKernelOptions::default();
    let local = open(&iface, a, b, &options);
    let peer = open(&iface, b, a, &options);
    let (mut local_rx, mut local_tx) = (local.reader, local.writer);
    let (mut peer_rx, mut peer_tx) = (peer.reader, peer.writer);
    let outgoing = [7; 64];
    let incoming = [9; 96];
    let mut received_local = [0; 128];
    let mut received_peer = [0; 128];
    let exchange = async {
        let (local_send, peer_send, local_read, peer_read) = tokio::join!(
            local_tx.send(
                Destination::Physical(0),
                &outgoing,
                deadline(Duration::from_secs(2))
            ),
            peer_tx.send(
                Destination::Physical(0),
                &incoming,
                deadline(Duration::from_secs(2))
            ),
            local_rx.recv_into(&mut received_local),
            peer_rx.recv_into(&mut received_peer),
        );
        local_send.unwrap();
        peer_send.unwrap();
        assert_eq!(&received_local[..local_read.unwrap().len], &incoming);
        assert_eq!(&received_peer[..peer_read.unwrap().len], &outgoing);
    };
    tokio::time::timeout(Duration::from_secs(5), exchange)
        .await
        .unwrap();
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires an isolated vcan interface"]
async fn close_during_flow_control_wait_preserves_executor_heartbeat_and_capacity() {
    let iface = test_iface();
    let closer = SocketCloser::new(1, 1).unwrap();
    let parts = TokioSocketCanIsoTp::open_with_ids(
        &closer,
        &iface,
        id(0x371),
        id(0x372),
        SocketRoute {
            receive_from: 0x22,
            transmit_to: Destination::Physical(0),
        },
        &IsoTpKernelOptions::default(),
    )
    .unwrap()
    .into_split();
    let linux_socketcan_iso_tp::SocketParts {
        reader,
        mut writer,
        control,
    } = parts;
    writer
        .send(
            Destination::Physical(0),
            &[7; 64],
            deadline(Duration::from_secs(1)),
        )
        .await
        .unwrap();
    // Permit must remain occupied through the kernel's outstanding TX wait.
    let start = tokio::time::Instant::now();
    drop(reader);
    drop(writer);
    tokio::time::sleep(Duration::from_millis(10)).await;
    assert!(
        start.elapsed() < Duration::from_millis(200),
        "socket drop blocked the executor"
    );
    assert_eq!(closer.occupied(), 1);
    assert!(!control.is_closed());
    assert!(matches!(
        TokioSocketCanIsoTp::open_peer(&closer, &iface, 1, 2, &IsoTpKernelOptions::default()),
        Err(Error::CloseCapacity)
    ));
    tokio::time::timeout(Duration::from_secs(3), control.closed())
        .await
        .unwrap();
    assert_eq!(closer.occupied(), 0);
    let next =
        TokioSocketCanIsoTp::open_peer(&closer, &iface, 1, 2, &IsoTpKernelOptions::default())
            .unwrap()
            .into_split();
    drop(next.reader);
    drop(next.writer);
    next.control.closed().await;
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires an isolated vcan interface"]
async fn repeated_open_reject_close_cycles_return_every_permit() {
    let iface = test_iface();
    let closer = SocketCloser::new(1, 1).unwrap();
    for _ in 0..1_000 {
        let mut parts = TokioSocketCanIsoTp::open_peer(
            &closer,
            &iface,
            0x11,
            0x22,
            &IsoTpKernelOptions::default(),
        )
        .unwrap()
        .into_split();
        assert_eq!(closer.occupied(), 1);
        assert!(matches!(
            parts
                .writer
                .send(
                    Destination::Functional(0x22),
                    &[1],
                    deadline(Duration::from_secs(1))
                )
                .await,
            Err(TransmitError::Rejected(_))
        ));
        drop(parts.reader);
        drop(parts.writer);
        tokio::time::timeout(Duration::from_secs(1), parts.control.closed())
            .await
            .unwrap();
        assert_eq!(closer.occupied(), 0);
    }
}

#[test]
#[ignore = "requires an isolated vcan interface"]
fn demux_flow_control_change_applies_to_registered_peers() {
    use can_isotp_interface::{IsoTpRxFlowControlConfig, RxFlowControl};
    let mut demux = linux_socketcan_iso_tp::KernelUdsDemux::new(
        test_iface(),
        0x51,
        IsoTpKernelOptions::default(),
    );
    demux.register_peer(0x52).unwrap();
    // Linux rejects option changes on a bound socket (`EISCONN`), so the demux
    // must reopen the peer's socket rather than change it in place.
    demux
        .set_rx_flow_control(RxFlowControl {
            block_size: 4,
            st_min: Duration::from_millis(1),
        })
        .expect("flow control change applies to an open peer");
}
