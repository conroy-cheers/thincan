#![cfg(feature = "heapless")]

use capnp::message::ReaderOptions;
use heapless::Vec;

use crate::{
    Atlas, capnp_scratch_words_for_bytes, file_ack_max_encoded_len, file_chunk_max_encoded_len,
    file_offer_max_encoded_len, schema,
};

/// Fixed-capacity Cap'n Proto scratch (no-alloc).
pub struct CapnpScratch<const WORDS: usize> {
    words: Vec<capnp::Word, WORDS>,
}

impl<const WORDS: usize> Default for CapnpScratch<WORDS> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const WORDS: usize> CapnpScratch<WORDS> {
    pub fn new() -> Self {
        Self { words: Vec::new() }
    }

    fn with_bytes<R>(
        &mut self,
        min_len: usize,
        f: impl FnOnce(&mut [u8]) -> R,
    ) -> Result<R, thincan::Error> {
        let min_words = capnp_scratch_words_for_bytes(min_len);
        if min_words > WORDS {
            return Err(thincan::Error {
                kind: thincan::ErrorKind::Other,
            });
        }
        while self.words.len() < min_words {
            self.words
                .push(capnp::word(0, 0, 0, 0, 0, 0, 0, 0))
                .map_err(|_| thincan::Error {
                    kind: thincan::ErrorKind::Other,
                })?;
        }
        let bytes = unsafe {
            core::slice::from_raw_parts_mut(
                self.words.as_mut_ptr() as *mut u8,
                self.words.len() * 8,
            )
        };
        Ok(f(&mut bytes[..min_len]))
    }
}

/// Encode a `FileReq` offer Cap'n Proto body into `out` using fixed scratch.
pub fn encode_file_offer_into<A: Atlas, const WORDS: usize>(
    scratch: &mut CapnpScratch<WORDS>,
    transfer_id: u32,
    total_len: u32,
    sender_max_chunk_size: u32,
    metadata: &[u8],
    out: &mut [u8],
) -> Result<usize, thincan::Error> {
    let max_len = file_offer_max_encoded_len(metadata.len());
    if out.len() < max_len {
        return Err(thincan::Error {
            kind: thincan::ErrorKind::Other,
        });
    }
    scratch.with_bytes(max_len, |scratch_bytes| {
        let mut message = capnp::message::Builder::new(
            capnp::message::SingleSegmentAllocator::new(scratch_bytes),
        );
        let mut root: schema::file_req::Builder = message.init_root();
        root.set_transfer_id(transfer_id);
        root.set_total_len(total_len);
        root.set_file_metadata(metadata);
        root.set_sender_max_chunk_size(sender_max_chunk_size);

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
    })?
}

/// Encode a `FileChunk` Cap'n Proto body into `out` using fixed scratch.
pub fn encode_file_chunk_into<const WORDS: usize>(
    scratch: &mut CapnpScratch<WORDS>,
    transfer_id: u32,
    offset: u32,
    data: &[u8],
    out: &mut [u8],
) -> Result<usize, thincan::Error> {
    let max_len = file_chunk_max_encoded_len(data.len());
    if out.len() < max_len {
        return Err(thincan::Error {
            kind: thincan::ErrorKind::Other,
        });
    }
    scratch.with_bytes(max_len, |scratch_bytes| {
        let mut message = capnp::message::Builder::new(
            capnp::message::SingleSegmentAllocator::new(scratch_bytes),
        );
        let mut root: schema::file_chunk::Builder = message.init_root();
        root.set_transfer_id(transfer_id);
        root.set_offset(offset);
        root.set_data(data);

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
    })?
}

/// Encode a `FileAck` Cap'n Proto body into `out` using fixed scratch.
pub fn encode_file_ack_into<const WORDS: usize>(
    scratch: &mut CapnpScratch<WORDS>,
    transfer_id: u32,
    kind: schema::FileAckKind,
    next_offset: u32,
    chunk_size: u32,
    error: schema::FileAckError,
    out: &mut [u8],
) -> Result<usize, thincan::Error> {
    let max_len = file_ack_max_encoded_len();
    if out.len() < max_len {
        return Err(thincan::Error {
            kind: thincan::ErrorKind::Other,
        });
    }
    scratch.with_bytes(max_len, |scratch_bytes| {
        let mut message = capnp::message::Builder::new(
            capnp::message::SingleSegmentAllocator::new(scratch_bytes),
        );
        let mut root: schema::file_ack::Builder = message.init_root();
        root.set_transfer_id(transfer_id);
        root.set_kind(kind);
        root.set_next_offset(next_offset);
        root.set_chunk_size(chunk_size);
        root.set_error(error);

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
    })?
}

/// Decode a `FileAck` body into `(transfer_id, kind, next_offset, chunk_size, error)`.
pub fn decode_file_ack_fields<'a>(
    msg: thincan::CapnpTyped<'a, schema::file_ack::Owned>,
) -> Result<(u32, schema::FileAckKind, u32, u32, schema::FileAckError), thincan::Error> {
    let (transfer_id, kind, next_offset, chunk_size, error) = msg
        .with_root(ReaderOptions::default(), |root| {
            (
                root.get_transfer_id(),
                root.get_kind(),
                root.get_next_offset(),
                root.get_chunk_size(),
                root.get_error(),
            )
        })
        .map_err(|_| thincan::Error {
            kind: thincan::ErrorKind::Other,
        })?;

    let kind = kind.map_err(|_| thincan::Error {
        kind: thincan::ErrorKind::Other,
    })?;
    let error = error.map_err(|_| thincan::Error {
        kind: thincan::ErrorKind::Other,
    })?;
    Ok((transfer_id, kind, next_offset, chunk_size, error))
}
