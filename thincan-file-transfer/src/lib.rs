//! File-transfer bundle for `thincan`.
//!
//! This crate provides a composable "file transfer" bundle built on top of `thincan` and Cap'n Proto.
//! It is designed to be used from downstream crates without any per-atlas glue macros.
//!
//! ## Integration steps
//! 1. Define the messages in your bus atlas (usually in your firmware/app crate):
//!    - `FileReq` / `FileChunk` / `FileAck` using the Cap'n Proto owned types from
//!      `thincan_file_transfer::schema`.
//! 2. Implement [`Atlas`] for your generated marker type `your_atlas::Atlas`.
//! 3. Compose the bundle into a `bus_maplet!` using `file_transfer = thincan_file_transfer::Bundle`.
//! 4. Provide storage by implementing [`FileStore`], and keep a [`State`] in your handlers.
//!
//! ## Minimal example (encoding only)
//! ```rust
//! use thincan::Encode;
//!
//! #[derive(Clone, Copy)]
//! struct FileReq;
//! #[derive(Clone, Copy)]
//! struct FileChunk;
//! #[derive(Clone, Copy)]
//! struct FileAck;
//!
//! impl thincan::Message for FileReq { const ID: u16 = 0x1001; }
//! impl thincan::Message for FileChunk { const ID: u16 = 0x1002; }
//! impl thincan::Message for FileAck { const ID: u16 = 0x1003; }
//!
//! struct MyAtlas;
//! impl thincan_file_transfer::Atlas for MyAtlas {
//!   type FileReq = FileReq;
//!   type FileChunk = FileChunk;
//!   type FileAck = FileAck;
//! }
//!
//! let req = thincan_file_transfer::file_req::<MyAtlas>(1, 16);
//! let mut buf = vec![0u8; req.max_encoded_len()];
//! let n = req.encode(&mut buf).unwrap();
//! # assert!(n > 0);
//! ```
//!
//! For a full routing example, see `examples/router_only.rs` and `examples/interface_two_threads.rs`.
//!
//! ## Doctest: end-to-end decode into a store
//! ```rust
//! use thincan::Encode;
//!
//! #[derive(Clone, Copy)]
//! struct FileReq;
//! #[derive(Clone, Copy)]
//! struct FileChunk;
//! #[derive(Clone, Copy)]
//! struct FileAck;
//!
//! impl thincan::Message for FileReq { const ID: u16 = 0x1001; }
//! impl thincan::Message for FileChunk { const ID: u16 = 0x1002; }
//! impl thincan::Message for FileAck { const ID: u16 = 0x1003; }
//!
//! struct MyAtlas;
//! impl thincan_file_transfer::Atlas for MyAtlas {
//!   type FileReq = FileReq;
//!   type FileChunk = FileChunk;
//!   type FileAck = FileAck;
//! }
//!
//! #[derive(Default)]
//! struct MemoryStore {
//!   committed: Vec<Vec<u8>>,
//! }
//!
//! impl thincan_file_transfer::FileStore for MemoryStore {
//!   type Error = core::convert::Infallible;
//!   type WriteHandle = Vec<u8>;
//!
//!   fn begin_write(&mut self, _transfer_id: u32, total_len: u32) -> Result<Self::WriteHandle, Self::Error> {
//!     Ok(vec![0u8; total_len as usize])
//!   }
//!
//!   fn write_at(&mut self, handle: &mut Self::WriteHandle, offset: u32, bytes: &[u8]) -> Result<(), Self::Error> {
//!     let start = offset as usize;
//!     let end = start + bytes.len();
//!     handle[start..end].copy_from_slice(bytes);
//!     Ok(())
//!   }
//!
//!   fn commit(&mut self, handle: Self::WriteHandle) -> Result<(), Self::Error> {
//!     self.committed.push(handle);
//!     Ok(())
//!   }
//!
//!   fn abort(&mut self, _handle: Self::WriteHandle) {}
//! }
//!
//! let mut state = thincan_file_transfer::State::new(MemoryStore::default());
//! let transfer_id = 42u32;
//! let file = b"hello file transfer".to_vec();
//!
//! // Encode + handle FileReq.
//! let req = thincan_file_transfer::file_req::<MyAtlas>(transfer_id, file.len() as u32);
//! let mut req_buf = vec![0u8; req.max_encoded_len()];
//! let req_len = req.encode(&mut req_buf).unwrap();
//! let req_typed = thincan::CapnpTyped::<thincan_file_transfer::schema::file_req::Owned>::new(&req_buf[..req_len]);
//! thincan_file_transfer::handle_file_req(&mut state, req_typed).unwrap();
//!
//! // Encode + handle FileChunk (single chunk).
//! let chunk = thincan_file_transfer::file_chunk::<MyAtlas>(transfer_id, 0, &file[..]);
//! let mut chunk_buf = vec![0u8; chunk.max_encoded_len()];
//! let chunk_len = chunk.encode(&mut chunk_buf).unwrap();
//! let chunk_typed = thincan::CapnpTyped::<thincan_file_transfer::schema::file_chunk::Owned>::new(&chunk_buf[..chunk_len]);
//! thincan_file_transfer::handle_file_chunk(&mut state, chunk_typed).unwrap();
//!
//! # assert_eq!(state.store.committed.len(), 1);
//! # assert_eq!(state.store.committed[0], file);
//! ```

use capnp::message::ReaderOptions;
use core::marker::PhantomData;
use std::cell::RefCell;
use std::time::Duration;

capnp::generated_code!(pub mod file_transfer_capnp);
pub use file_transfer_capnp as schema;

thread_local! {
    static CAPNP_SCRATCH: RefCell<Vec<capnp::Word>> = const { RefCell::new(Vec::new()) };
}

fn with_capnp_scratch_bytes<R>(min_len: usize, f: impl FnOnce(&mut [u8]) -> R) -> R {
    let min_words = (min_len + 7) / 8;
    CAPNP_SCRATCH.with(|cell| {
        let mut words = cell.borrow_mut();
        if words.len() < min_words {
            words.resize(min_words, capnp::word(0, 0, 0, 0, 0, 0, 0, 0));
        }
        let bytes = unsafe {
            std::slice::from_raw_parts_mut(words.as_mut_ptr() as *mut u8, words.len() * 8)
        };
        f(&mut bytes[..min_len])
    })
}

/// Default chunk size (bytes) used by [`Sender`].
pub const DEFAULT_CHUNK_SIZE: usize = 64;

/// Generic bundle spec (used by `bus_maplet!`).
///
/// Compose this bundle by writing (see the examples directory for a complete, compilable version):
/// ```ignore
/// thincan::bus_atlas! { pub mod atlas { /* ... */ } }
/// impl thincan_file_transfer::Atlas for atlas::Atlas { /* ... */ }
/// thincan::bus_maplet! {
///   pub mod maplet: atlas {
///     bundles [file_transfer = thincan_file_transfer::Bundle];
///     /* ... */
///   }
/// }
/// ```
pub struct Bundle;

/// Atlas contract for this bundle.
///
/// This trait is implemented by the application's atlas marker type (generated by `bus_atlas!` as
/// `your_atlas::Atlas`). It maps the bundle's required messages to the concrete message types in
/// the application's atlas.
///
/// ### Doctest: compose into a maplet
/// ```rust
/// use thincan::Encode;
/// use thincan::RouterDispatch;
///
/// mod demo {
///   use super::*;
///
///   thincan::bus_atlas! {
///     pub mod atlas {
///       0x1001 => FileReq(capnp = thincan_file_transfer::schema::file_req::Owned);
///       0x1002 => FileChunk(capnp = thincan_file_transfer::schema::file_chunk::Owned);
///       0x1003 => FileAck(capnp = thincan_file_transfer::schema::file_ack::Owned);
///     }
///   }
///
///   impl thincan_file_transfer::Atlas for atlas::Atlas {
///     type FileReq = atlas::FileReq;
///     type FileChunk = atlas::FileChunk;
///     type FileAck = atlas::FileAck;
///   }
///
///   thincan::bus_maplet! {
///     pub mod maplet: atlas {
///       bundles [file_transfer = thincan_file_transfer::Bundle];
///       parser: thincan::DefaultParser;
///       use msgs [FileReq, FileChunk, FileAck];
///       handles {}
///       unhandled_by_default = true;
///       ignore [];
///     }
///   }
///
///   #[derive(Default)]
///   pub struct NoApp;
///
///   impl<'a> maplet::Handlers<'a> for NoApp {
///     type Error = ();
///   }
///
///   #[derive(Default)]
///   pub struct MemoryStore {
///     pub committed: Vec<Vec<u8>>,
///   }
///
///   impl thincan_file_transfer::FileStore for MemoryStore {
///     type Error = core::convert::Infallible;
///     type WriteHandle = Vec<u8>;
///
///     fn begin_write(&mut self, _transfer_id: u32, total_len: u32) -> Result<Self::WriteHandle, Self::Error> {
///       Ok(vec![0u8; total_len as usize])
///     }
///
///     fn write_at(&mut self, handle: &mut Self::WriteHandle, offset: u32, bytes: &[u8]) -> Result<(), Self::Error> {
///       let start = offset as usize;
///       let end = start + bytes.len();
///       handle[start..end].copy_from_slice(bytes);
///       Ok(())
///     }
///
///     fn commit(&mut self, handle: Self::WriteHandle) -> Result<(), Self::Error> {
///       self.committed.push(handle);
///       Ok(())
///     }
///
///     fn abort(&mut self, _handle: Self::WriteHandle) {}
///   }
/// }
///
/// let file = b"hello world".to_vec();
/// let transfer_id = 1u32;
/// let mut router = demo::maplet::Router::new();
/// let mut handlers = demo::maplet::HandlersImpl {
///   app: demo::NoApp::default(),
///   file_transfer: thincan_file_transfer::State::new(demo::MemoryStore::default()),
/// };
///
/// let req = thincan_file_transfer::file_req::<demo::atlas::Atlas>(transfer_id, file.len() as u32);
/// let mut req_buf = vec![0u8; req.max_encoded_len()];
/// let req_len = req.encode(&mut req_buf).unwrap();
/// router.dispatch(
///   &mut handlers,
///   thincan::RawMessage { id: <demo::atlas::FileReq as thincan::Message>::ID, body: &req_buf[..req_len] },
/// ).unwrap();
///
/// let chunk = thincan_file_transfer::file_chunk::<demo::atlas::Atlas>(transfer_id, 0, &file[..]);
/// let mut chunk_buf = vec![0u8; chunk.max_encoded_len()];
/// let chunk_len = chunk.encode(&mut chunk_buf).unwrap();
/// router.dispatch(
///   &mut handlers,
///   thincan::RawMessage { id: <demo::atlas::FileChunk as thincan::Message>::ID, body: &chunk_buf[..chunk_len] },
/// ).unwrap();
///
/// # assert_eq!(handlers.file_transfer.store.committed.len(), 1);
/// # assert_eq!(handlers.file_transfer.store.committed[0], file);
/// ```
pub trait Atlas {
    type FileReq: thincan::Message;
    type FileChunk: thincan::Message;
    type FileAck: thincan::Message;
}

/// Dependency required by the file-transfer bundle: a byte-addressable file store.
pub trait FileStore {
    type Error;
    type WriteHandle;

    fn begin_write(
        &mut self,
        transfer_id: u32,
        total_len: u32,
    ) -> Result<Self::WriteHandle, Self::Error>;

    fn write_at(
        &mut self,
        handle: &mut Self::WriteHandle,
        offset: u32,
        bytes: &[u8],
    ) -> Result<(), Self::Error>;

    fn commit(&mut self, handle: Self::WriteHandle) -> Result<(), Self::Error>;

    fn abort(&mut self, handle: Self::WriteHandle);
}

/// Dependency "umbrella trait" for bundle handlers.
///
/// Most applications will simply use [`State`], which already implements this trait.
pub trait Deps {
    type Store: FileStore;
    fn state(&mut self) -> &mut State<Self::Store>;
}

/// Errors produced by the file-transfer state machine.
#[derive(Debug)]
pub enum Error<E> {
    Store(E),
    Protocol,
    Capnp(capnp::Error),
}

struct InProgress<S: FileStore> {
    transfer_id: u32,
    total_len: u32,
    handle: S::WriteHandle,
    received: Vec<(u32, u32)>,
}

/// Bundle state (store + in-progress transfer tracking).
#[derive(Default)]
pub struct State<S: FileStore> {
    pub store: S,
    in_progress: Option<InProgress<S>>,
}

impl<S: FileStore> State<S> {
    /// Create new bundle state around a concrete store implementation.
    pub fn new(store: S) -> Self {
        Self {
            store,
            in_progress: None,
        }
    }
}

impl<S: FileStore> Deps for State<S> {
    type Store = S;
    fn state(&mut self) -> &mut State<Self::Store> {
        self
    }
}

/// Handle an incoming `FileReq` message.
pub fn handle_file_req<'a, S: FileStore>(
    state: &mut State<S>,
    msg: thincan::CapnpTyped<'a, schema::file_req::Owned>,
) -> Result<(), Error<S::Error>> {
    let (transfer_id, total_len) = msg
        .with_root(ReaderOptions::default(), |root| {
            (root.get_transfer_id(), root.get_total_len())
        })
        .map_err(Error::Capnp)?;

    if let Some(in_progress) = state.in_progress.take() {
        state.store.abort(in_progress.handle);
    }
    let handle = state
        .store
        .begin_write(transfer_id, total_len)
        .map_err(Error::Store)?;
    if total_len == 0 {
        state.store.commit(handle).map_err(Error::Store)?;
        state.in_progress = None;
    } else {
        state.in_progress = Some(InProgress {
            transfer_id,
            total_len,
            handle,
            received: Vec::new(),
        });
    }
    Ok(())
}

fn insert_received_range(ranges: &mut Vec<(u32, u32)>, start: u32, end: u32) {
    if start >= end {
        return;
    }

    let mut i = 0usize;
    while i < ranges.len() && ranges[i].1 < start {
        i += 1;
    }

    let mut merged_start = start;
    let mut merged_end = end;
    while i < ranges.len() && ranges[i].0 <= merged_end {
        merged_start = merged_start.min(ranges[i].0);
        merged_end = merged_end.max(ranges[i].1);
        ranges.remove(i);
    }

    ranges.insert(i, (merged_start, merged_end));
}

/// Handle an incoming `FileChunk` message.
pub fn handle_file_chunk<'a, S: FileStore>(
    state: &mut State<S>,
    msg: thincan::CapnpTyped<'a, schema::file_chunk::Owned>,
) -> Result<(), Error<S::Error>> {
    msg.0.with_reader(ReaderOptions::default(), |reader| {
        let typed = capnp::message::TypedReader::<_, schema::file_chunk::Owned>::new(reader);
        let root = typed.get().map_err(Error::Capnp)?;

        let transfer_id = root.get_transfer_id();
        let offset = root.get_offset();
        let data = root.get_data().unwrap_or(&[]);

        let in_progress = state.in_progress.as_mut().ok_or(Error::Protocol)?;
        if in_progress.transfer_id != transfer_id {
            return Err(Error::Protocol);
        }

        let remaining = in_progress.total_len.saturating_sub(offset);
        if remaining == 0 {
            return Ok(());
        }

        let chunk_len = (data.len() as u32).min(remaining) as usize;
        state
            .store
            .write_at(&mut in_progress.handle, offset, &data[..chunk_len])
            .map_err(Error::Store)?;

        insert_received_range(
            &mut in_progress.received,
            offset,
            offset + (chunk_len as u32),
        );
        if in_progress.received.len() == 1
            && in_progress.received[0].0 == 0
            && in_progress.received[0].1 >= in_progress.total_len
        {
            let in_progress = state.in_progress.take().ok_or(Error::Protocol)?;
            state
                .store
                .commit(in_progress.handle)
                .map_err(Error::Store)?;
        }

        Ok(())
    })
}

/// Handle an incoming `FileAck` message.
pub fn handle_file_ack<'a, S: FileStore>(
    _state: &mut State<S>,
    _msg: thincan::CapnpTyped<'a, schema::file_ack::Owned>,
) -> Result<(), Error<S::Error>> {
    Ok(())
}

/// Value type used to encode a `FileReq` message for a specific atlas marker type `A`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileReqValue<A> {
    pub transfer_id: u32,
    pub total_len: u32,
    _atlas: PhantomData<A>,
}

/// Value type used to encode a `FileChunk` message for a specific atlas marker type `A`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileChunkValue<'a, A> {
    pub transfer_id: u32,
    pub offset: u32,
    pub data: &'a [u8],
    _atlas: PhantomData<A>,
}

impl<A> FileReqValue<A> {
    /// Construct a `FileReq` value.
    pub fn new(transfer_id: u32, total_len: u32) -> Self {
        Self {
            transfer_id,
            total_len,
            _atlas: PhantomData,
        }
    }
}

/// Convenience constructor for [`FileReqValue`].
pub fn file_req<A>(transfer_id: u32, total_len: u32) -> FileReqValue<A> {
    FileReqValue::new(transfer_id, total_len)
}

impl<'a, A> FileChunkValue<'a, A> {
    /// Construct a `FileChunk` value.
    pub fn new(transfer_id: u32, offset: u32, data: &'a [u8]) -> Self {
        Self {
            transfer_id,
            offset,
            data,
            _atlas: PhantomData,
        }
    }
}

/// Convenience constructor for [`FileChunkValue`].
pub fn file_chunk<'a, A>(transfer_id: u32, offset: u32, data: &'a [u8]) -> FileChunkValue<'a, A> {
    FileChunkValue::new(transfer_id, offset, data)
}

/// Transport abstraction for sending encoded messages.
pub trait SendEncoded {
    fn send_encoded<M: thincan::Message, V: thincan::Encode<M>>(
        &mut self,
        value: &V,
        timeout: Duration,
    ) -> Result<(), thincan::Error>;
}

impl<Node, Router, TxBuf> SendEncoded for thincan::Interface<Node, Router, TxBuf>
where
    Node: thincan::Transport,
    TxBuf: AsMut<[u8]>,
{
    fn send_encoded<M: thincan::Message, V: thincan::Encode<M>>(
        &mut self,
        value: &V,
        timeout: Duration,
    ) -> Result<(), thincan::Error> {
        thincan::Interface::send_encoded(self, value, timeout)
    }
}

/// Transport abstraction for sending encoded messages to a specific transport address.
pub trait SendEncodedTo<A> {
    fn send_encoded_to<M: thincan::Message, V: thincan::Encode<M>>(
        &mut self,
        to: A,
        value: &V,
        timeout: Duration,
    ) -> Result<(), thincan::Error>;
}

impl<Node, Router, TxBuf> SendEncodedTo<Node::ReplyTo> for thincan::Interface<Node, Router, TxBuf>
where
    Node: thincan::TransportMeta,
    TxBuf: AsMut<[u8]>,
{
    fn send_encoded_to<M: thincan::Message, V: thincan::Encode<M>>(
        &mut self,
        to: Node::ReplyTo,
        value: &V,
        timeout: Duration,
    ) -> Result<(), thincan::Error> {
        thincan::Interface::send_encoded_to(self, to, value, timeout)
    }
}

/// Summary of a completed send.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SendResult {
    pub transfer_id: u32,
    pub total_len: usize,
    pub chunk_size: usize,
    pub chunks: usize,
}

/// High-level sender that handles `FileReq` + chunking for you.
#[derive(Debug, Clone)]
pub struct Sender<A> {
    next_transfer_id: u32,
    chunk_size: usize,
    _atlas: PhantomData<A>,
}

impl<A> Default for Sender<A> {
    fn default() -> Self {
        Self::new()
    }
}

impl<A> Sender<A> {
    /// Create a sender with a default chunk size.
    pub fn new() -> Self {
        Self {
            next_transfer_id: 1,
            chunk_size: DEFAULT_CHUNK_SIZE,
            _atlas: PhantomData,
        }
    }

    /// Create a sender with a custom chunk size.
    pub fn with_chunk_size(chunk_size: usize) -> Result<Self, thincan::Error> {
        let mut sender = Self::new();
        sender.set_chunk_size(chunk_size)?;
        Ok(sender)
    }

    /// Current chunk size in bytes.
    pub fn chunk_size(&self) -> usize {
        self.chunk_size
    }

    /// Set the chunk size in bytes.
    pub fn set_chunk_size(&mut self, chunk_size: usize) -> Result<(), thincan::Error> {
        if chunk_size == 0 {
            return Err(thincan::Error {
                kind: thincan::ErrorKind::Other,
            });
        }
        self.chunk_size = chunk_size;
        Ok(())
    }

    /// Next transfer id that will be used by [`send_file`].
    pub fn next_transfer_id(&self) -> u32 {
        self.next_transfer_id
    }

    /// Send a full file using an auto-assigned transfer id.
    pub fn send_file<T: SendEncoded>(
        &mut self,
        tx: &mut T,
        bytes: &[u8],
        timeout: Duration,
    ) -> Result<SendResult, thincan::Error>
    where
        A: Atlas,
    {
        let transfer_id = self.next_transfer_id;
        self.next_transfer_id = self.next_transfer_id.wrapping_add(1);
        self.send_file_with_id(tx, transfer_id, bytes, timeout)
    }

    /// Send a full file using an explicit transfer id.
    pub fn send_file_with_id<T: SendEncoded>(
        &mut self,
        tx: &mut T,
        transfer_id: u32,
        bytes: &[u8],
        timeout: Duration,
    ) -> Result<SendResult, thincan::Error>
    where
        A: Atlas,
    {
        if transfer_id >= self.next_transfer_id {
            self.next_transfer_id = transfer_id.wrapping_add(1);
        }
        if self.chunk_size == 0 {
            return Err(thincan::Error {
                kind: thincan::ErrorKind::Other,
            });
        }

        let total_len = bytes.len();
        let total_len_u32 = u32::try_from(total_len).map_err(|_| thincan::Error {
            kind: thincan::ErrorKind::Other,
        })?;

        tx.send_encoded::<A::FileReq, _>(&file_req::<A>(transfer_id, total_len_u32), timeout)?;

        let mut chunks = 0usize;
        for (index, chunk) in bytes.chunks(self.chunk_size).enumerate() {
            let offset = index.checked_mul(self.chunk_size).ok_or(thincan::Error {
                kind: thincan::ErrorKind::Other,
            })?;
            let offset_u32 = u32::try_from(offset).map_err(|_| thincan::Error {
                kind: thincan::ErrorKind::Other,
            })?;
            tx.send_encoded::<A::FileChunk, _>(
                &file_chunk::<A>(transfer_id, offset_u32, chunk),
                timeout,
            )?;
            chunks += 1;
        }

        Ok(SendResult {
            transfer_id,
            total_len,
            chunk_size: self.chunk_size,
            chunks,
        })
    }

    /// Send a full file to a specific transport address using an auto-assigned transfer id.
    pub fn send_file_to<T, Addr>(
        &mut self,
        tx: &mut T,
        to: Addr,
        bytes: &[u8],
        timeout: Duration,
    ) -> Result<SendResult, thincan::Error>
    where
        A: Atlas,
        T: SendEncodedTo<Addr>,
        Addr: Copy,
    {
        let transfer_id = self.next_transfer_id;
        self.next_transfer_id = self.next_transfer_id.wrapping_add(1);
        self.send_file_with_id_to(tx, to, transfer_id, bytes, timeout)
    }

    /// Send a full file to a specific transport address using an explicit transfer id.
    pub fn send_file_with_id_to<T, Addr>(
        &mut self,
        tx: &mut T,
        to: Addr,
        transfer_id: u32,
        bytes: &[u8],
        timeout: Duration,
    ) -> Result<SendResult, thincan::Error>
    where
        A: Atlas,
        T: SendEncodedTo<Addr>,
        Addr: Copy,
    {
        if transfer_id >= self.next_transfer_id {
            self.next_transfer_id = transfer_id.wrapping_add(1);
        }
        if self.chunk_size == 0 {
            return Err(thincan::Error {
                kind: thincan::ErrorKind::Other,
            });
        }

        let total_len = bytes.len();
        let total_len_u32 = u32::try_from(total_len).map_err(|_| thincan::Error {
            kind: thincan::ErrorKind::Other,
        })?;

        tx.send_encoded_to::<A::FileReq, _>(
            to,
            &file_req::<A>(transfer_id, total_len_u32),
            timeout,
        )?;

        let mut chunks = 0usize;
        for (index, chunk) in bytes.chunks(self.chunk_size).enumerate() {
            let offset = index.checked_mul(self.chunk_size).ok_or(thincan::Error {
                kind: thincan::ErrorKind::Other,
            })?;
            let offset_u32 = u32::try_from(offset).map_err(|_| thincan::Error {
                kind: thincan::ErrorKind::Other,
            })?;
            tx.send_encoded_to::<A::FileChunk, _>(
                to,
                &file_chunk::<A>(transfer_id, offset_u32, chunk),
                timeout,
            )?;
            chunks += 1;
        }

        Ok(SendResult {
            transfer_id,
            total_len,
            chunk_size: self.chunk_size,
            chunks,
        })
    }
}

impl<A> thincan::BundleMeta<A> for Bundle
where
    A: Atlas,
{
    type Parser = thincan::DefaultParser;
    const HANDLED_IDS: &'static [u16] = &[
        <A::FileReq as thincan::Message>::ID,
        <A::FileChunk as thincan::Message>::ID,
        <A::FileAck as thincan::Message>::ID,
    ];
    const USED_IDS: &'static [u16] = <Self as thincan::BundleMeta<A>>::HANDLED_IDS;

    fn is_known(id: u16) -> bool {
        id == <A::FileReq as thincan::Message>::ID
            || id == <A::FileChunk as thincan::Message>::ID
            || id == <A::FileAck as thincan::Message>::ID
    }
}

impl<A, H> thincan::BundleDispatch<A, H> for Bundle
where
    A: Atlas,
    H: Deps,
{
    fn try_dispatch<'a>(
        _parser: &Self::Parser,
        handlers: &mut H,
        msg: thincan::RawMessage<'a>,
    ) -> Option<Result<(), thincan::Error>> {
        if msg.id == <A::FileReq as thincan::Message>::ID {
            let parsed = thincan::CapnpTyped::<schema::file_req::Owned>::new(msg.body);
            if handle_file_req(handlers.state(), parsed).is_err() {
                return Some(Err(thincan::Error {
                    kind: thincan::ErrorKind::Other,
                }));
            }
            return Some(Ok(()));
        }

        if msg.id == <A::FileChunk as thincan::Message>::ID {
            let parsed = thincan::CapnpTyped::<schema::file_chunk::Owned>::new(msg.body);
            if handle_file_chunk(handlers.state(), parsed).is_err() {
                return Some(Err(thincan::Error {
                    kind: thincan::ErrorKind::Other,
                }));
            }
            return Some(Ok(()));
        }

        if msg.id == <A::FileAck as thincan::Message>::ID {
            let parsed = thincan::CapnpTyped::<schema::file_ack::Owned>::new(msg.body);
            if handle_file_ack(handlers.state(), parsed).is_err() {
                return Some(Err(thincan::Error {
                    kind: thincan::ErrorKind::Other,
                }));
            }
            return Some(Ok(()));
        }
        None
    }
}

impl<A> thincan::Encode<<A as Atlas>::FileReq> for FileReqValue<A>
where
    A: Atlas,
{
    fn max_encoded_len(&self) -> usize {
        // Single-segment Cap'n Proto message (no framing) containing:
        // - 1 word: root pointer
        // - 1 word: struct data (2x u32)
        16
    }

    fn encode(&self, out: &mut [u8]) -> Result<usize, thincan::Error> {
        let max_len = self.max_encoded_len();
        if out.len() < max_len {
            return Err(thincan::Error {
                kind: thincan::ErrorKind::Other,
            });
        }
        with_capnp_scratch_bytes(max_len, |scratch| {
            let mut message =
                capnp::message::Builder::new(capnp::message::SingleSegmentAllocator::new(scratch));
            let mut root: schema::file_req::Builder = message.init_root();
            root.set_transfer_id(self.transfer_id);
            root.set_total_len(self.total_len);

            let segments = message.get_segments_for_output();
            if segments.len() != 1 {
                return Err(thincan::Error {
                    kind: thincan::ErrorKind::Other,
                });
            }
            let bytes = segments[0];
            if bytes.len() > max_len {
                return Err(thincan::Error {
                    kind: thincan::ErrorKind::Other,
                });
            }
            out[..bytes.len()].copy_from_slice(bytes);
            Ok(bytes.len())
        })
    }
}

impl<'a, A> thincan::Encode<<A as Atlas>::FileChunk> for FileChunkValue<'a, A>
where
    A: Atlas,
{
    fn max_encoded_len(&self) -> usize {
        // Single-segment Cap'n Proto message (no framing) containing:
        // - 1 word: root pointer
        // - 3 words: struct (data=2x u32, pointers=1x pointer) + alignment slack
        // - N bytes: the Data payload, padded to a word boundary
        32 + ((self.data.len() + 7) & !7)
    }

    fn encode(&self, out: &mut [u8]) -> Result<usize, thincan::Error> {
        let max_len = self.max_encoded_len();
        if out.len() < max_len {
            return Err(thincan::Error {
                kind: thincan::ErrorKind::Other,
            });
        }
        with_capnp_scratch_bytes(max_len, |scratch| {
            let mut message =
                capnp::message::Builder::new(capnp::message::SingleSegmentAllocator::new(scratch));
            let mut root: schema::file_chunk::Builder = message.init_root();
            root.set_transfer_id(self.transfer_id);
            root.set_offset(self.offset);
            root.set_data(self.data);

            let segments = message.get_segments_for_output();
            if segments.len() != 1 {
                return Err(thincan::Error {
                    kind: thincan::ErrorKind::Other,
                });
            }
            let bytes = segments[0];
            if bytes.len() > max_len {
                return Err(thincan::Error {
                    kind: thincan::ErrorKind::Other,
                });
            }
            out[..bytes.len()].copy_from_slice(bytes);
            Ok(bytes.len())
        })
    }
}
