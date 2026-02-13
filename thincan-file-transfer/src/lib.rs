//! File-transfer protocol helpers for `thincan`.
//!
//! This crate provides Cap'n Proto message types and helpers for building a simple file transfer
//! protocol on top of `thincan`.
//!
//! ## Integration steps (embassy-style, no-alloc)
//! 1. Define the messages in your bus atlas (usually in your firmware/app crate):
//!    - `FileReq` / `FileChunk` / `FileAck` using the Cap'n Proto owned types from
//!      [`schema`].
//! 2. Implement [`Atlas`] for your generated marker type `your_atlas::Atlas`.
//! 3. Create two bounded channels:
//!    - an inbound queue for `FileReq`/`FileChunk` events, and
//!    - an outbound queue for `FileAck` responses to send.
//! 4. In your ISO-TP receive loop, decode the `thincan` wire payload and enqueue file-transfer
//!    messages via [`ReceiverIngress`].
//! 5. In a separate async task, call [`ReceiverNoAlloc::process_one`] to perform storage I/O and
//!    emit acks after writes complete (this provides natural backpressure).
//!
//! ### Sketch (enable `--features embassy`)
//! ```rust,ignore
//! use thincan_file_transfer as ft;
//! use embassy_sync::blocking_mutex::raw::NoopRawMutex;
//! use embassy_sync::channel::Channel;
//!
//! // Your bus atlas defines FileReq/FileChunk/FileAck message IDs (capnp types from `ft::schema`).
//! // `ReplyTo` is the addressing type provided by your ISO-TP demux (for UDS, typically `u8`).
//! type ReplyTo = u8;
//! const MAX_METADATA: usize = 64;
//! const MAX_CHUNK: usize = 1024;
//! const IN_DEPTH: usize = 8;
//! const ACK_DEPTH: usize = 8;
//!
//! static INBOUND: ft::InboundQueue<NoopRawMutex, ReplyTo, MAX_METADATA, MAX_CHUNK, IN_DEPTH> =
//!     Channel::new();
//! static OUTBOUND_ACKS: ft::OutboundAckQueue<NoopRawMutex, ReplyTo, ACK_DEPTH> = Channel::new();
//!
//! // Receive loop: enqueue file-transfer messages (no storage I/O here).
//! let ingestor = ft::ReceiverIngress::<MyAtlas, NoopRawMutex, ReplyTo, MAX_METADATA, MAX_CHUNK, IN_DEPTH>::new(&INBOUND);
//! # let reply_to: ReplyTo = 0;
//! # let payload: &[u8] = &[];
//! if let Ok(raw) = thincan::decode_wire(payload) {
//!     let _handled = ingestor.try_ingest_thincan_raw(reply_to, raw);
//! }
//!
//! // Worker task: do async storage I/O and emit acks after writes complete.
//! let mut rx = ft::ReceiverNoAlloc::<MyAtlas, MyStore, NoopRawMutex, ReplyTo, MAX_METADATA, 32, MAX_CHUNK, IN_DEPTH, ACK_DEPTH>::new(
//!     MyStore::new(),
//!     &INBOUND,
//!     &OUTBOUND_ACKS,
//! );
//! loop {
//!     rx.process_one().await?;
//! }
//! ```
//!
//! ## Backpressure / throttling
//! This crate is designed so that **slow storage naturally throttles the transfer**:
//! - [`ReceiverNoAlloc::process_one`] only emits an [`AckToSend`] after the corresponding
//!   [`AsyncFileStore::write_at`] has completed.
//! - The sender side is expected to be **ack-driven** and only send a bounded "window" of chunks
//!   ahead of the most recent cumulative ack.
//!
//! The `tokio` feature includes [`AsyncSender`], which implements this "windowed, ack-driven"
//! behavior.
//!
//! ## Sending acks back (embassy-style)
//! [`ReceiverNoAlloc`] does not transmit on the bus. It pushes `AckToSend<ReplyTo>` items into your
//! outbound ack queue, and your application is responsible for encoding and sending those acks
//! to the correct `reply_to` address.
//!
//! With `--features heapless` you can encode Cap'n Proto bodies without allocation:
//! ```rust,ignore
//! # use thincan_file_transfer as ft;
//! # let mut scratch = ft::CapnpScratch::<8>::new();
//! # let mut out = [0u8; ft::file_ack_max_encoded_len()];
//! # let ack: ft::AckToSend<u8> = todo!();
//! let used = ft::encode_file_ack_into(
//!     &mut scratch,
//!     ack.ack.transfer_id,
//!     ack.ack.kind,
//!     ack.ack.next_offset,
//!     ack.ack.chunk_size,
//!     ack.ack.error,
//!     &mut out,
//! )?;
//! // Send `&out[..used]` as the body of your bus's `FileAck` message to `ack.reply_to`.
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
    type FileReq: thincan::Message;
    type FileChunk: thincan::Message;
    type FileAck: thincan::Message;
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

#[cfg(feature = "tokio")]
mod tokio_impl;
#[cfg(feature = "tokio")]
pub use tokio_impl::{
    Ack, AckInbox, AckStream, AsyncSendResult, AsyncSender, ack_channel, decode_file_ack,
};

#[cfg(feature = "embassy")]
mod embassy_receiver;
#[cfg(feature = "embassy")]
pub use embassy_receiver::{
    AckToSend, InboundEvent, InboundQueue, OutboundAckQueue, ReceiverIngress, ReceiverNoAlloc,
    ReceiverNoAllocConfig,
};
