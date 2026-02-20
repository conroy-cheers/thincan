#![cfg(all(feature = "tokio", feature = "embassy"))]

use core::time::Duration;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use can_isotp_interface::{
    IsoTpAsyncEndpoint, RecvControl, RecvError, RecvMeta, RecvStatus, SendError,
};

thincan::bus_atlas! {
    pub mod atlas {
        0x1001 => FileReq(capnp = thincan_file_transfer::schema::file_req::Owned);
        0x1002 => FileChunk(capnp = thincan_file_transfer::schema::file_chunk::Owned);
        0x1003 => FileAck(capnp = thincan_file_transfer::schema::file_ack::Owned);
    }
}

pub mod protocol_bundle {
    pub type Bundle = thincan_file_transfer::FileTransferBundle<super::atlas::Atlas>;
    pub const MESSAGE_COUNT: usize = thincan_file_transfer::FILE_TRANSFER_MESSAGE_COUNT;
}

thincan::maplet! {
    pub mod maplet: atlas {
        bundles [file_transfer = protocol_bundle];
    }
}

impl thincan_file_transfer::Atlas for atlas::Atlas {
    type FileReq = atlas::FileReq;
    type FileChunk = atlas::FileChunk;
    type FileAck = atlas::FileAck;
}

#[derive(Default)]
struct SharedPipe {
    a_to_b: VecDeque<(u8, Vec<u8>)>,
    b_to_a: VecDeque<(u8, Vec<u8>)>,
}

#[derive(Clone, Copy)]
enum Direction {
    A,
    B,
}

#[derive(Clone)]
struct PipeEnd {
    shared: Arc<Mutex<SharedPipe>>,
    dir: Direction,
    from_addr: u8,
}

impl PipeEnd {
    fn pair(a_addr: u8, b_addr: u8) -> (Self, Self) {
        let shared = Arc::new(Mutex::new(SharedPipe::default()));
        (
            Self {
                shared: shared.clone(),
                dir: Direction::A,
                from_addr: a_addr,
            },
            Self {
                shared,
                dir: Direction::B,
                from_addr: b_addr,
            },
        )
    }

    fn drain_incoming(&self) -> Vec<(u8, Vec<u8>)> {
        let mut shared = self.shared.lock().unwrap();
        let queue = match self.dir {
            Direction::A => &mut shared.b_to_a,
            Direction::B => &mut shared.a_to_b,
        };
        queue.drain(..).collect()
    }
}

impl IsoTpAsyncEndpoint for PipeEnd {
    type Error = ();

    async fn send_to(
        &mut self,
        _to: u8,
        payload: &[u8],
        _timeout: Duration,
    ) -> Result<(), SendError<Self::Error>> {
        let mut shared = self.shared.lock().unwrap();
        match self.dir {
            Direction::A => shared.a_to_b.push_back((self.from_addr, payload.to_vec())),
            Direction::B => shared.b_to_a.push_back((self.from_addr, payload.to_vec())),
        }
        Ok(())
    }

    async fn send_functional_to(
        &mut self,
        _functional_to: u8,
        payload: &[u8],
        timeout: Duration,
    ) -> Result<(), SendError<Self::Error>> {
        self.send_to(0, payload, timeout).await
    }

    async fn recv_one<Cb>(
        &mut self,
        _timeout: Duration,
        _on_payload: Cb,
    ) -> Result<RecvStatus, RecvError<Self::Error>>
    where
        Cb: FnMut(RecvMeta, &[u8]) -> Result<RecvControl, Self::Error>,
    {
        Ok(RecvStatus::TimedOut)
    }
}

#[derive(Debug, Default)]
struct MemoryStore {
    bytes: Vec<u8>,
}

impl thincan_file_transfer::AsyncFileStore for MemoryStore {
    type Error = ();
    type WriteHandle = ();

    async fn begin_write(
        &mut self,
        _transfer_id: u32,
        total_len: u32,
    ) -> Result<Self::WriteHandle, Self::Error> {
        self.bytes.clear();
        self.bytes.resize(total_len as usize, 0);
        Ok(())
    }

    async fn write_at(
        &mut self,
        _handle: &mut Self::WriteHandle,
        offset: u32,
        bytes: &[u8],
    ) -> Result<(), Self::Error> {
        let offset = offset as usize;
        let end = offset + bytes.len();
        self.bytes[offset..end].copy_from_slice(bytes);
        Ok(())
    }

    async fn commit(&mut self, _handle: Self::WriteHandle) -> Result<(), Self::Error> {
        Ok(())
    }

    async fn abort(&mut self, _handle: Self::WriteHandle) {}
}

#[tokio::test(flavor = "current_thread")]
async fn application_dx_end_to_end_embassy_no_alloc_bundle_api() {
    const MAX_METADATA: usize = 16;
    const MAX_CHUNK: usize = 32;

    let (a_node, b_node) = PipeEnd::pair(0x31, 0x32);
    let a_pump = a_node.clone();
    let b_pump = b_node.clone();

    let mut a_tx = [0u8; 512];
    let mut b_tx = [0u8; 512];
    let a_iface =
        maplet::Interface::<thincan::NoopRawMutex, _, _, 16, 512, 4>::new(a_node, &mut a_tx);
    let b_iface =
        maplet::Interface::<thincan::NoopRawMutex, _, _, 16, 512, 4>::new(b_node, &mut b_tx);

    let mut a_bundles = maplet::Bundles::new(&a_iface);
    let b_bundles = maplet::Bundles::new(&b_iface);
    let a_ingest = a_iface.handle();
    let b_ingest = b_iface.handle();

    let stop = Arc::new(AtomicBool::new(false));

    let stop_a = stop.clone();
    let pump_a = async move {
        while !stop_a.load(Ordering::Relaxed) {
            for (from, payload) in a_pump.drain_incoming() {
                a_ingest.ingest(from, &payload).await.unwrap();
            }
            tokio::task::yield_now().await;
        }
        for (from, payload) in a_pump.drain_incoming() {
            a_ingest.ingest(from, &payload).await.unwrap();
        }
    };

    let stop_b = stop.clone();
    let pump_b = async move {
        while !stop_b.load(Ordering::Relaxed) {
            for (from, payload) in b_pump.drain_incoming() {
                b_ingest.ingest(from, &payload).await.unwrap();
            }
            tokio::task::yield_now().await;
        }
        for (from, payload) in b_pump.drain_incoming() {
            b_ingest.ingest(from, &payload).await.unwrap();
        }
    };

    let payload = b"embassy-no-alloc-path".to_vec();

    let recv_task = async move {
        let mut store = MemoryStore::default();
        let result = b_bundles
            .file_transfer
            .recv_file_no_alloc::<_, MAX_METADATA, MAX_CHUNK>(
                0x31,
                &mut store,
                Duration::from_millis(100),
                thincan_file_transfer::ReceiverConfig {
                    max_chunk_size: MAX_CHUNK as u32,
                },
            )
            .await
            .unwrap();
        (result, store.bytes)
    };

    let send_task = async move {
        a_bundles
            .file_transfer
            .send_file(
                0x32,
                &payload,
                Duration::from_millis(100),
                thincan_file_transfer::SendConfig {
                    sender_max_chunk_size: 8,
                    window_chunks: 2,
                    max_retries: 3,
                },
            )
            .await
            .unwrap()
    };

    let transfer = async {
        let (send_out, (recv_out, recv_bytes)) = tokio::join!(send_task, recv_task);
        stop.store(true, Ordering::Relaxed);
        (send_out, recv_out, recv_bytes)
    };

    let (_, _, (send_out, recv_out, recv_bytes)) = tokio::join!(pump_a, pump_b, transfer);

    assert_eq!(send_out.total_len, recv_bytes.len());
    assert_eq!(recv_out.received_len as usize, recv_bytes.len());
    assert!(recv_out.metadata.is_empty());
    assert_eq!(recv_bytes.as_slice(), b"embassy-no-alloc-path");
}
