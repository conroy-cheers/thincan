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
//! 4. Provide storage by implementing [`FileStore`], and keep a state in your handlers.
//!
//! For a full routing example, see `examples/router_only.rs` and `examples/interface_two_threads.rs`.
#![cfg_attr(not(feature = "std"), no_std)]

#[cfg(not(any(feature = "alloc", feature = "heapless")))]
compile_error!(
    "thincan-file-transfer requires either `alloc` (or `std`) or `heapless` (for no-alloc)."
);

#[cfg(feature = "alloc")]
extern crate alloc;

use core::marker::PhantomData;

capnp::generated_code!(pub mod file_transfer_capnp);
pub use file_transfer_capnp as schema;

/// Default chunk size (bytes) used by [`Sender`](crate::Sender) (alloc) and [`SenderNoAlloc`](crate::SenderNoAlloc).
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

/// Receiver-side configuration (used by [`State`](crate::State) and [`StateNoAlloc`](crate::StateNoAlloc)).
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

/// Generic bundle spec (used by `bus_maplet!`).
pub struct Bundle;

/// Atlas contract for this bundle.
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

/// Summary of a completed send.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SendResult {
    pub transfer_id: u32,
    pub total_len: usize,
    pub chunk_size: usize,
    pub chunks: usize,
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

#[cfg(feature = "alloc")]
mod alloc_impl;
#[cfg(feature = "alloc")]
pub use alloc_impl::{
    Deps, SendEncoded, SendEncodedTo, Sender, State, handle_file_ack, handle_file_chunk,
    handle_file_req,
};

#[cfg(feature = "heapless")]
mod heapless_impl;
#[cfg(feature = "heapless")]
pub use heapless_impl::{
    CapnpScratch, SendMsg, SendMsgTo, SenderNoAlloc, StateNoAlloc, handle_file_ack_no_alloc,
    handle_file_chunk_no_alloc, handle_file_req_no_alloc,
};

#[cfg(feature = "tokio")]
mod tokio_impl;
#[cfg(feature = "tokio")]
pub use tokio_impl::{
    Ack, AckInbox, AckStream, AsyncSendResult, AsyncSender, ack_channel, decode_file_ack,
};
