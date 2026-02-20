//! File-transfer protocol helpers for `thincan`.
//!
//! This crate provides Cap'n Proto message types and helpers for building a simple file transfer
//! protocol on top of `thincan`.
//!
//! ## Integration steps
//! 1. Define `FileReq` / `FileChunk` / `FileAck` in your bus atlas using [`schema`] owned types.
//! 2. Implement [`Atlas`] for your atlas marker type.
//! 3. Include `FileTransferBundle` in a `thincan::maplet!` bundle list.
//! 4. Build `maplet::Bundles::new(&iface)` and call methods on the singleton bundle instance:
//!    - sender: `bundles.file_transfer.send_file(...)` / `send_file_with_id(...)`
//!    - receiver (`alloc`): `bundles.file_transfer.recv_file(...)`
//!    - receiver (`heapless`): `bundles.file_transfer.recv_file_no_alloc(...)`
//!
//! ## Backpressure / throttling
//! Sender flow is ack-driven and windowed. Receiver flow only sends progress/completion acks after
//! storage writes complete, so slow storage naturally throttles transfer rate.
//!
//! ## Runtime note
//! These methods rely on your external demux pump to feed incoming frames into the doodad mailbox
//! via `DoodadHandle::ingest`.
//!
//! With `--features heapless` you can encode Cap'n Proto bodies without allocation:
//! ```rust,ignore
//! # use thincan_file_transfer as ft;
//! # let mut scratch = ft::CapnpScratch::<8>::new();
//! # let mut out = [0u8; ft::file_ack_max_encoded_len()];
//! # let transfer_id = 1u32;
//! # let kind = ft::schema::FileAckKind::Ack;
//! # let next_offset = 64u32;
//! # let chunk_size = 0u32;
//! # let error = ft::schema::FileAckError::None;
//! let used = ft::encode_file_ack_into(
//!     &mut scratch,
//!     transfer_id,
//!     kind,
//!     next_offset,
//!     chunk_size,
//!     error,
//!     &mut out,
//! )?;
//! // Send `&out[..used]` as the body of your bus's `FileAck` message.
//! # Ok::<(), thincan::Error>(())
//! ```
//!
//! This crate is async-first and is designed for runtimes like embassy where storage I/O must not
//! block the executor.
#![cfg_attr(not(feature = "std"), no_std)]
#![allow(async_fn_in_trait)]

#[cfg(not(any(feature = "alloc", feature = "heapless")))]
compile_error!(
    "thincan-file-transfer requires either `alloc` (or `std`) or `heapless` (for no-alloc)."
);

#[cfg(feature = "alloc")]
extern crate alloc;

/// Re-exported for integrations that need Cap'n Proto helpers without depending on `capnp`
/// directly.
#[doc(hidden)]
pub use capnp;

use core::marker::PhantomData;

capnp::generated_code!(pub mod file_transfer_capnp);
pub use file_transfer_capnp as schema;

/// Default chunk size (bytes) used by senders.
pub const DEFAULT_CHUNK_SIZE: usize = 64;

/// Cap'n Proto "word padding" (Cap'n Proto Data blobs are padded to a word boundary).
pub const fn capnp_padded_len(len: usize) -> usize {
    (len + 7) & !7
}

/// Upper bound for an encoded `FileReq` body (bytes) without metadata.
pub const fn file_req_max_encoded_len() -> usize {
    32
}

/// Upper bound for an encoded `FileReq` body (bytes) with metadata.
pub const fn file_offer_max_encoded_len(metadata_len: usize) -> usize {
    32 + capnp_padded_len(metadata_len)
}

/// Upper bound for an encoded `FileChunk` body (bytes).
pub const fn file_chunk_max_encoded_len(data_len: usize) -> usize {
    32 + capnp_padded_len(data_len)
}

/// Upper bound for an encoded `FileAck` body (bytes).
pub const fn file_ack_max_encoded_len() -> usize {
    24
}

/// Convert a byte count to a conservative Cap'n Proto scratch requirement (words).
pub const fn capnp_scratch_words_for_bytes(bytes: usize) -> usize {
    (bytes + 7) / 8
}

/// Receiver-side configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReceiverConfig {
    /// Maximum accepted `FileChunk.data` length (bytes).
    ///
    /// - `0` means "no limit / accept any size" (back-compat default).
    /// - Non-zero values cause oversize chunks to be rejected as a protocol error.
    pub max_chunk_size: u32,
}

impl Default for ReceiverConfig {
    fn default() -> Self {
        Self { max_chunk_size: 0 }
    }
}

/// Atlas contract for file-transfer message types.
pub trait Atlas {
    type FileReq: thincan::CapnpMessage;
    type FileChunk: thincan::CapnpMessage;
    type FileAck: thincan::CapnpMessage;
}

/// Number of message types required by [`FileTransferBundle`].
pub const FILE_TRANSFER_MESSAGE_COUNT: usize = 3;

/// Bundle type for file-transfer protocol operations.
#[derive(Clone, Copy, Debug, Default)]
pub struct FileTransferBundle<A>(PhantomData<A>);

impl<A> thincan::BundleSpec<FILE_TRANSFER_MESSAGE_COUNT> for FileTransferBundle<A>
where
    A: Atlas,
{
    const MESSAGE_IDS: [u16; FILE_TRANSFER_MESSAGE_COUNT] = [
        <A::FileReq as thincan::Message>::ID,
        <A::FileChunk as thincan::Message>::ID,
        <A::FileAck as thincan::Message>::ID,
    ];
}

/// Async dependency required by the file-transfer bundle: a byte-addressable file store.
///
/// This is intended for async runtimes such as embassy, where storage I/O (flash, SD, etc.) should
/// not block the entire executor.
pub trait AsyncFileStore {
    type Error;
    type WriteHandle;

    async fn begin_write(
        &mut self,
        transfer_id: u32,
        total_len: u32,
    ) -> Result<Self::WriteHandle, Self::Error>;

    async fn write_at(
        &mut self,
        handle: &mut Self::WriteHandle,
        offset: u32,
        bytes: &[u8],
    ) -> Result<(), Self::Error>;

    async fn commit(&mut self, handle: Self::WriteHandle) -> Result<(), Self::Error>;

    async fn abort(&mut self, handle: Self::WriteHandle);
}

/// Errors produced by the file-transfer state machine.
#[derive(Debug)]
pub enum Error<E> {
    Store(E),
    Protocol,
    Capnp(capnp::Error),
}

/// Ack message that the receiver side would like to emit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PendingAck {
    pub transfer_id: u32,
    pub kind: schema::FileAckKind,
    pub next_offset: u32,
    pub chunk_size: u32,
    pub error: schema::FileAckError,
}

/// Value type used to encode a `FileReq` message for a specific atlas marker type `A`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileReqValue<A> {
    pub transfer_id: u32,
    pub total_len: u32,
    _atlas: PhantomData<A>,
}

/// Value type used to encode a `FileReq` message including metadata and chunk negotiation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileOfferValue<'a, A> {
    pub transfer_id: u32,
    pub total_len: u32,
    pub file_metadata: &'a [u8],
    pub sender_max_chunk_size: u32,
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

impl<'a, A> FileOfferValue<'a, A> {
    /// Construct a `FileReq` offer value.
    pub fn new(
        transfer_id: u32,
        total_len: u32,
        sender_max_chunk_size: u32,
        file_metadata: &'a [u8],
    ) -> Self {
        Self {
            transfer_id,
            total_len,
            file_metadata,
            sender_max_chunk_size,
            _atlas: PhantomData,
        }
    }
}

/// Convenience constructor for [`FileOfferValue`].
pub fn file_offer<'a, A>(
    transfer_id: u32,
    total_len: u32,
    sender_max_chunk_size: u32,
    file_metadata: &'a [u8],
) -> FileOfferValue<'a, A> {
    FileOfferValue::new(transfer_id, total_len, sender_max_chunk_size, file_metadata)
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

/// Value type used to encode a `FileAck` message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileAckValue<A> {
    pub transfer_id: u32,
    pub kind: schema::FileAckKind,
    pub next_offset: u32,
    pub chunk_size: u32,
    pub error: schema::FileAckError,
    _atlas: PhantomData<A>,
}

impl<A> FileAckValue<A> {
    pub fn new(
        transfer_id: u32,
        kind: schema::FileAckKind,
        next_offset: u32,
        chunk_size: u32,
        error: schema::FileAckError,
    ) -> Self {
        Self {
            transfer_id,
            kind,
            next_offset,
            chunk_size,
            error,
            _atlas: PhantomData,
        }
    }
}

pub fn file_ack_accept<A>(transfer_id: u32, chunk_size: u32) -> FileAckValue<A> {
    FileAckValue::new(
        transfer_id,
        schema::FileAckKind::Accept,
        0,
        chunk_size,
        schema::FileAckError::None,
    )
}

pub fn file_ack_progress<A>(transfer_id: u32, next_offset: u32) -> FileAckValue<A> {
    FileAckValue::new(
        transfer_id,
        schema::FileAckKind::Ack,
        next_offset,
        0,
        schema::FileAckError::None,
    )
}

pub fn file_ack_reject<A>(transfer_id: u32, error: schema::FileAckError) -> FileAckValue<A> {
    FileAckValue::new(transfer_id, schema::FileAckKind::Reject, 0, 0, error)
}

#[cfg(feature = "alloc")]
mod alloc_encode;

#[cfg(feature = "heapless")]
mod heapless_encode;
#[cfg(feature = "heapless")]
pub use heapless_encode::{
    CapnpScratch, decode_file_ack_fields, encode_file_ack_into, encode_file_chunk_into,
    encode_file_offer_into,
};

#[cfg(any(feature = "tokio", feature = "embassy"))]
mod tokio_impl;
#[cfg(any(feature = "tokio", feature = "embassy"))]
pub use tokio_impl::{
    Ack, FileTransferBundleInstance, SendConfig, SendFileResult, SendState, decode_file_ack,
};

#[cfg(feature = "alloc")]
#[cfg(any(feature = "tokio", feature = "embassy"))]
pub use tokio_impl::RecvFileResult;

#[cfg(feature = "heapless")]
#[cfg(any(feature = "tokio", feature = "embassy"))]
pub use tokio_impl::RecvFileResultNoAlloc;

// File transfer integrations should expose the maplet-generated bundle singleton and call its
// async methods (`send_file`, `recv_file`, etc.) from bespoke async protocol code.
