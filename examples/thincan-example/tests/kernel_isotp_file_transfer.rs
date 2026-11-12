//! File transfer between two nodes on Linux kernel ISO-TP sockets, end to end
//! over a vcan interface: `thincan` and `thincan-file-transfer` on the Tokio
//! socket halves, with receive flow control configured when opening.
//!
//! Run with an isolated vcan interface:
//! `THINCAN_TEST_VCAN_IFACE=vcan0 cargo test -p thincan-example --features socketcan-isotp -- --ignored`
#![cfg(all(target_os = "linux", feature = "socketcan-isotp"))]

use linux_socketcan_iso_tp::{
    IsoTpFlowControlOptions, IsoTpKernelOptions, SocketCloser, TokioSocketCanIsoTp,
};
use std::time::{Duration, Instant};
use tokio::sync::Barrier;

thincan::bus_atlas! {
    pub mod atlas {
        0x1001 => FileReq(capnp = thincan_file_transfer::schema::file_req::Owned);
        0x1002 => FileChunk(capnp = thincan_file_transfer::schema::file_chunk::Owned);
        0x1003 => FileAck(capnp = thincan_file_transfer::schema::file_ack::Owned);
    }
}

pub mod file_transfer_bundle {
    pub type Bundle = thincan_file_transfer::FileTransferBundle<super::atlas::Atlas>;
    pub const MESSAGE_COUNT: usize = thincan_file_transfer::FILE_TRANSFER_MESSAGE_COUNT;
}

thincan::maplet! {
    pub mod maplet: atlas {
        bundles [file_transfer = file_transfer_bundle];
    }
}

impl thincan_file_transfer::Atlas for atlas::Atlas {
    type FileReq = atlas::FileReq;
    type FileChunk = atlas::FileChunk;
    type FileAck = atlas::FileAck;
}

/// UDS addresses distinct from the other vcan tests' IDs.
const A: u8 = 0x31;
const B: u8 = 0x32;
const CHUNK: usize = 256;
/// Receive flow control each node asks for: two frames per block, 2 ms apart
/// (STmin 0x02 is 2 ms in ISO-TP's encoding).
const FLOW_CONTROL: IsoTpFlowControlOptions = IsoTpFlowControlOptions {
    block_size: 2,
    st_min: 0x02,
    wft_max: 0,
};

#[derive(Debug, Default)]
struct MemoryStore {
    bytes: Vec<u8>,
}

impl thincan_file_transfer::AsyncFileStore for MemoryStore {
    type Error = ();
    type WriteHandle = ();
    async fn begin_write(&mut self, _: u32, total_len: u32) -> Result<(), ()> {
        self.bytes = vec![0; total_len as usize];
        Ok(())
    }
    async fn write_at(&mut self, _: &mut (), offset: u32, bytes: &[u8]) -> Result<(), ()> {
        let offset = offset as usize;
        self.bytes[offset..offset + bytes.len()].copy_from_slice(bytes);
        Ok(())
    }
    async fn commit(&mut self, _: ()) -> Result<(), ()> {
        Ok(())
    }
    async fn abort(&mut self, _: ()) {}
}

type Writer = linux_socketcan_iso_tp::TokioIsoTpWriteHalf;
type Bundles<'a> = maplet::Bundles<'a, thincan::NoopRawMutex, Writer, Vec<u8>, 8, 512, 4>;

fn deadline(duration: Duration) -> tokio::time::Instant {
    tokio::time::Instant::now() + duration
}

fn file(len: usize, seed: u8) -> Vec<u8> {
    (0..len)
        .map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed))
        .collect()
}

/// Run `app` on a node at `local` talking to `peer`, with its receive pump.
async fn node<T>(
    closer: &SocketCloser,
    local: u8,
    peer: u8,
    app: impl AsyncFnOnce(&Bundles<'_>) -> T,
) -> T {
    let iface = std::env::var("THINCAN_TEST_VCAN_IFACE")
        .expect("set THINCAN_TEST_VCAN_IFACE to an isolated vcan interface");
    let options = IsoTpKernelOptions {
        flow_control: Some(FLOW_CONTROL),
        ..Default::default()
    };
    let parts = TokioSocketCanIsoTp::open_peer(closer, &iface, local, peer, &options)
        .expect("open kernel ISO-TP socket")
        .into_split();
    let interface = maplet::Interface::<thincan::NoopRawMutex, _, _, 8, 512, 4>::new(
        parts.writer,
        vec![0; 1024],
    );
    let bundles = Bundles::new(&interface);
    let mut scratch = [0; 4095];
    let mut pump = thincan::ReceivePump::new(parts.reader, &interface, &mut scratch);
    let pump = async {
        loop {
            if let Err(error) = pump.next().await
                && error.is_terminal()
            {
                return format!("receive pump failed: {error:?}");
            }
        }
    };
    tokio::select! {
        error = pump => panic!("{error}"),
        out = app(&bundles) => out,
    }
}

async fn send(bundles: &Bundles<'_>, to: u8, bytes: &[u8]) {
    let config = thincan_file_transfer::SendConfig {
        sender_max_chunk_size: CHUNK,
        ..Default::default()
    };
    bundles
        .file_transfer
        .send_file(to, bytes, deadline(Duration::from_secs(10)), config)
        .await
        .expect("send over kernel ISO-TP");
}

async fn receive(bundles: &Bundles<'_>, from: u8) -> Vec<u8> {
    let config = thincan_file_transfer::ReceiverConfig {
        max_chunk_size: CHUNK as u32,
        ..Default::default()
    };
    let mut store = MemoryStore::default();
    bundles
        .file_transfer
        .recv_file(from, &mut store, deadline(Duration::from_secs(10)), config)
        .await
        .expect("receive over kernel ISO-TP");
    store.bytes
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires an isolated vcan interface in THINCAN_TEST_VCAN_IFACE"]
async fn file_transfer_both_ways_over_kernel_isotp_with_rx_flow_control() {
    let closer = SocketCloser::new(8, 2).expect("socket closer");
    let (start, done) = (Barrier::new(2), Barrier::new(2));
    let to_b = file(2048, 1);
    let to_a = file(1500, 2);
    let started = Instant::now();
    let scenario = async {
        tokio::join!(
            node(&closer, A, B, async |a| {
                start.wait().await;
                send(a, B, &to_b).await;
                assert_eq!(receive(a, B).await, to_a);
                done.wait().await;
            }),
            node(&closer, B, A, async |b| {
                start.wait().await;
                assert_eq!(receive(b, A).await, to_b);
                send(b, A, &to_a).await;
                done.wait().await;
            }),
        )
    };
    tokio::time::timeout(Duration::from_secs(30), scenario)
        .await
        .expect("transfers finish");
    // Each frame carries 7 data bytes, so the two files take about 500
    // consecutive frames. With a block size of 2, Linux sends the first frame
    // after each flow-control grant at once and waits STmin (2 ms) before the
    // second, so the transfers take about 0.6 s; without flow control vcan
    // finishes in about 30 ms.
    let elapsed = started.elapsed();
    assert!(
        elapsed >= Duration::from_millis(300),
        "flow control not applied: {elapsed:?}"
    );
}
