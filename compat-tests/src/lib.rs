//! End-to-end compatibility harness: complete nodes from the published 0.1
//! release ([`old`]) and from this workspace ([`new`]) on one in-process
//! unix-socket CAN bus.
//!
//! Every exchange crosses real CAN frames, both ISO-TP implementations
//! (`can-iso-tp` 0.2 and 0.3), both `thincan` versions and both file-transfer
//! state machines. Faults are injected only at a node's `thincan` transport
//! edge, by dropping selected outgoing messages ([`Faults`]), so the protocol
//! code under test runs unmodified.

pub mod new;
pub mod old;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

pub const NODE_A: u8 = 0x11;
pub const NODE_B: u8 = 0x22;

/// Largest ISO-TP payload either node accepts.
pub const MAX_ISOTP_PAYLOAD: usize = 4095;
/// Chunk size both senders offer; receivers accept it.
pub const CHUNK_SIZE: usize = 1024;

/// thincan message ids used by both nodes' atlases.
pub const FILE_REQ: u16 = 0x1001;
pub const FILE_CHUNK: u16 = 0x1002;
pub const FILE_ACK: u16 = 0x1003;

/// A unix-socket CAN bus server, removed when dropped.
pub struct Bus {
    server: embedded_can_unix_socket::BusServer,
    path: PathBuf,
}

impl Bus {
    pub fn start() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let path = std::env::temp_dir().join(format!(
            "thincan-compat-{}-{}.sock",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_file(&path);
        let server = embedded_can_unix_socket::BusServer::start(&path).expect("start CAN bus");
        Self { server, path }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for Bus {
    fn drop(&mut self) {
        let _ = self.server.shutdown();
        let _ = std::fs::remove_file(&self.path);
    }
}

type Rule = dyn Fn(u16, &[u8]) -> bool + Send + Sync;

/// Which outgoing thincan messages a node drops before they reach ISO-TP.
#[derive(Clone)]
pub struct Faults(Option<Arc<Rule>>);

impl Faults {
    pub fn none() -> Self {
        Self(None)
    }

    /// Drop the first `n` messages matching `rule`; the counter counts all matches.
    pub fn drop_first(
        n: usize,
        rule: impl Fn(u16, &[u8]) -> bool + Send + Sync + 'static,
    ) -> (Self, Arc<AtomicUsize>) {
        let seen = Arc::new(AtomicUsize::new(0));
        let counter = seen.clone();
        let faults = Self(Some(Arc::new(move |id: u16, body: &[u8]| {
            rule(id, body) && counter.fetch_add(1, Ordering::SeqCst) < n
        })));
        (faults, seen)
    }

    /// Drop every message matching `rule`.
    pub fn drop_all(rule: impl Fn(u16, &[u8]) -> bool + Send + Sync + 'static) -> Self {
        Self(Some(Arc::new(rule)))
    }

    /// Whether to drop this encoded thincan payload (message id, then body).
    pub fn drops(&self, payload: &[u8]) -> bool {
        match (&self.0, payload) {
            (Some(rule), [lo, hi, body @ ..]) => rule(u16::from_le_bytes([*lo, *hi]), body),
            _ => false,
        }
    }
}

fn ack_kind(body: &[u8]) -> Option<thincan_file_transfer::schema::FileAckKind> {
    thincan_file_transfer::decode_file_ack(thincan::CapnpTyped::<
        thincan_file_transfer::schema::file_ack::Owned,
    >::new(body))
    .ok()
    .map(|ack| ack.kind)
}

/// `FileAck` of kind `Complete`.
pub fn is_complete(id: u16, body: &[u8]) -> bool {
    id == FILE_ACK && ack_kind(body) == Some(thincan_file_transfer::schema::FileAckKind::Complete)
}

/// `FileAck` of kind `Ack` (receiver progress).
pub fn is_progress(id: u16, body: &[u8]) -> bool {
    id == FILE_ACK && ack_kind(body) == Some(thincan_file_transfer::schema::FileAckKind::Ack)
}

/// `FileChunk` carrying data (not the empty confirmation of `Complete`).
pub fn is_data_chunk(id: u16, body: &[u8]) -> bool {
    id == FILE_CHUNK
        && thincan::CapnpTyped::<thincan_file_transfer::schema::file_chunk::Owned>::new(body)
            .with_root(Default::default(), |root| {
                root.get_data().map_or(0, |data| data.len()) > 0
            })
            .unwrap_or(false)
}

/// Deterministic test file contents.
pub fn file(len: usize, seed: u8) -> Vec<u8> {
    (0..len)
        .map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed))
        .collect()
}

/// A memory store that can take a while to prepare, like a flash erase.
#[derive(Debug, Default)]
pub struct Store {
    pub bytes: Vec<u8>,
    pub prepare_delay: Duration,
    /// How many transfers `begin_write` was called for.
    pub begun: usize,
}

impl Store {
    pub fn slow(prepare_delay: Duration) -> Self {
        Self {
            prepare_delay,
            ..Self::default()
        }
    }

    async fn begin(&mut self, total_len: u32) {
        self.begun += 1;
        tokio::time::sleep(self.prepare_delay).await;
        self.bytes = vec![0; total_len as usize];
    }

    fn write(&mut self, offset: u32, bytes: &[u8]) -> Result<(), String> {
        let offset = offset as usize;
        let end = offset + bytes.len();
        let len = self.bytes.len();
        self.bytes
            .get_mut(offset..end)
            .ok_or_else(|| format!("write past end: {end} > {len}"))?
            .copy_from_slice(bytes);
        Ok(())
    }
}

impl thincan_file_transfer::AsyncFileStore for Store {
    type Error = String;
    type WriteHandle = ();
    async fn begin_write(&mut self, _: u32, total_len: u32) -> Result<(), String> {
        self.begin(total_len).await;
        Ok(())
    }
    async fn write_at(&mut self, _: &mut (), offset: u32, bytes: &[u8]) -> Result<(), String> {
        self.write(offset, bytes)
    }
    async fn commit(&mut self, _: ()) -> Result<(), String> {
        Ok(())
    }
    async fn abort(&mut self, _: ()) {}
}

impl thincan_file_transfer_01::AsyncFileStore for Store {
    type Error = String;
    type WriteHandle = ();
    async fn begin_write(&mut self, _: u32, total_len: u32) -> Result<(), String> {
        self.begin(total_len).await;
        Ok(())
    }
    async fn write_at(&mut self, _: &mut (), offset: u32, bytes: &[u8]) -> Result<(), String> {
        self.write(offset, bytes)
    }
    async fn commit(&mut self, _: ()) -> Result<(), String> {
        Ok(())
    }
    async fn abort(&mut self, _: ()) {}
}

/// Connect to the bus twice (separate TX and RX connections), retrying while
/// the server comes up.
fn connect(
    path: &Path,
) -> (
    embedded_can_unix_socket::UnixCan,
    embedded_can_unix_socket::UnixCan,
) {
    let connect = || {
        for _ in 0..50 {
            if let Ok(can) = embedded_can_unix_socket::UnixCan::connect(path) {
                return can;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        panic!("connect to CAN bus at {}", path.display());
    };
    (connect(), connect())
}

/// Run an end-to-end scenario on its own current-thread runtime, failing it if
/// it takes longer than a minute.
///
/// The thread gets a large stack: each node's `Interface` holds its mailboxes
/// inline (hundreds of KiB), and debug builds copy them while constructing.
pub fn e2e<F, Fut>(scenario: F)
where
    F: FnOnce() -> Fut + Send + 'static,
    Fut: Future<Output = ()>,
{
    let thread = std::thread::Builder::new()
        .name("compat-e2e".into())
        .stack_size(64 << 20)
        .spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("tokio runtime");
            runtime.block_on(async {
                tokio::time::timeout(Duration::from_secs(60), scenario())
                    .await
                    .expect("scenario timed out")
            });
        })
        .expect("spawn scenario thread");
    if let Err(panic) = thread.join() {
        std::panic::resume_unwind(panic);
    }
}
