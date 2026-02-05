#[cfg(target_os = "linux")]
use can_iso_tp::{IsoTpConfig, IsoTpNode, StdClock};
#[cfg(target_os = "linux")]
use can_isotp_interface::{IsoTpEndpoint, RecvControl, RecvStatus};
#[cfg(target_os = "linux")]
use embedded_can::ExtendedId;
#[cfg(target_os = "linux")]
use embedded_can_interface::{FilterConfig, Id as IfaceId, IdMask, IdMaskFilter};
#[cfg(target_os = "linux")]
use embedded_can_socketcan::SocketCan;
#[cfg(target_os = "linux")]
use linux_socketcan_iso_tp::{IsoTpFlowControlOptions, IsoTpKernelOptions, SocketCanIsoTp};
#[cfg(target_os = "linux")]
use std::time::Duration;

#[cfg(target_os = "linux")]
const IO_TIMEOUT: Duration = Duration::from_secs(5);

#[cfg(target_os = "linux")]
fn iface_name() -> Option<String> {
    std::env::var("THINCAN_TEST_CAN_IFACE")
        .or_else(|_| std::env::var("THINCAN_BENCH_IFACE"))
        .ok()
}

#[cfg(target_os = "linux")]
fn uds_phys_id_raw(target: u8, source: u8) -> u32 {
    0x18DA_0000 | ((target as u32) << 8) | (source as u32)
}

#[cfg(target_os = "linux")]
fn uds_phys_id_iface(target: u8, source: u8) -> IfaceId {
    let raw = uds_phys_id_raw(target, source);
    let ext = ExtendedId::new(raw).expect("UDS 29-bit ID must fit in 29 bits");
    IfaceId::Extended(ext)
}

#[cfg(target_os = "linux")]
fn uds_phys_id_can(target: u8, source: u8) -> embedded_can::Id {
    let raw = uds_phys_id_raw(target, source);
    let ext = ExtendedId::new(raw).expect("UDS 29-bit ID must fit in 29 bits");
    embedded_can::Id::Extended(ext)
}

#[cfg(target_os = "linux")]
fn make_payload(seed: u8, len: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(len);
    for i in 0..len {
        out.push(seed.wrapping_add(i as u8).wrapping_mul(31).wrapping_add(17));
    }
    out
}

#[cfg(target_os = "linux")]
#[test]
fn raw_socketcan_talks_to_kernel_isotp() {
    let iface = match iface_name() {
        Some(name) => name,
        None => {
            eprintln!("skipping: set THINCAN_TEST_CAN_IFACE or THINCAN_BENCH_IFACE");
            return;
        }
    };

    let server = 0xF1;
    let client = 0xF2;

    let mut options = IsoTpKernelOptions::default();
    options.max_rx_payload = 4095;
    options.flow_control = Some(IsoTpFlowControlOptions::new(4, 5, 0));

    let mut kernel = match SocketCanIsoTp::open(
        &iface,
        uds_phys_id_can(server, client),
        uds_phys_id_can(client, server),
        &options,
    ) {
        Ok(sock) => sock,
        Err(err) => {
            eprintln!("skipping: failed to open kernel ISO-TP socket: {err:?}");
            return;
        }
    };

    let tx_can = match SocketCan::open(&iface) {
        Ok(sock) => sock,
        Err(err) => {
            eprintln!("skipping: failed to open raw SocketCAN TX: {err:?}");
            return;
        }
    };
    let mut rx_can = match SocketCan::open(&iface) {
        Ok(sock) => sock,
        Err(err) => {
            eprintln!("skipping: failed to open raw SocketCAN RX: {err:?}");
            return;
        }
    };

    let rx_filter = IdMaskFilter {
        id: uds_phys_id_iface(client, server),
        mask: IdMask::Extended(0x1FFF_FFFF),
    };
    if let Err(err) = FilterConfig::set_filters(&mut rx_can, &[rx_filter]) {
        eprintln!("skipping: failed to set acceptance filter: {err:?}");
        return;
    }

    let mut cfg = IsoTpConfig::default();
    cfg.tx_id = uds_phys_id_iface(server, client);
    cfg.rx_id = uds_phys_id_iface(client, server);
    cfg.block_size = 4;
    cfg.st_min = Duration::from_millis(1);
    cfg.max_payload_len = 4095;
    cfg.frame_len = 8;

    let mut rx_buf = vec![0u8; cfg.max_payload_len];
    let mut node = IsoTpNode::with_clock(tx_can, rx_can, cfg, StdClock, rx_buf.as_mut_slice())
        .expect("failed to build ISO-TP node");

    // Note: some backends reject zero-length ISO-TP payloads, so we start at 1 byte here.
    let sizes = [
        1usize, 7, 8, 9, 15, 31, 63, 64, 65, 127, 255, 512, 1024, 2048, 4095,
    ];

    for (idx, &size) in sizes.iter().enumerate() {
        let payload = make_payload(idx as u8, size);
        node.send(&payload, IO_TIMEOUT)
            .expect("raw ISO-TP send failed");

        let mut got = Vec::new();
        let status = kernel
            .recv_one(IO_TIMEOUT, |data| {
                got.extend_from_slice(data);
                Ok(RecvControl::Continue)
            })
            .expect("kernel ISO-TP recv failed");
        assert_eq!(status, RecvStatus::DeliveredOne);
        assert_eq!(got, payload, "mismatch raw->kernel at size {size}");

        let payload = make_payload((0xA5 ^ idx as u8).wrapping_add(3), size);
        kernel
            .send(&payload, IO_TIMEOUT)
            .expect("kernel ISO-TP send failed");

        let mut got = Vec::new();
        node.recv(IO_TIMEOUT, &mut |data| got.extend_from_slice(data))
            .expect("raw ISO-TP recv failed");
        assert_eq!(got, payload, "mismatch kernel->raw at size {size}");
    }
}

#[cfg(not(target_os = "linux"))]
#[test]
fn raw_socketcan_talks_to_kernel_isotp() {}
