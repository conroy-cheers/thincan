#![cfg(feature = "alloc")]

use crate::{Atlas, FileAckValue, FileChunkValue, FileOfferValue, FileReqValue, schema};
use alloc::vec::Vec;

#[cfg(feature = "std")]
use core::cell::RefCell;

#[cfg(feature = "std")]
thread_local! {
    static CAPNP_SCRATCH: RefCell<Vec<capnp::Word>> = const { RefCell::new(Vec::new()) };
}

fn with_capnp_scratch_bytes<R>(min_len: usize, f: impl FnOnce(&mut [u8]) -> R) -> R {
    let min_words = (min_len + 7) / 8;

    #[cfg(feature = "std")]
    {
        return CAPNP_SCRATCH.with(|cell| {
            let mut words = cell.borrow_mut();
            if words.len() < min_words {
                words.resize(min_words, capnp::word(0, 0, 0, 0, 0, 0, 0, 0));
            }
            let bytes = unsafe {
                core::slice::from_raw_parts_mut(words.as_mut_ptr() as *mut u8, words.len() * 8)
            };
            f(&mut bytes[..min_len])
        });
    }

    #[cfg(not(feature = "std"))]
    {
        let mut words: Vec<capnp::Word> = Vec::new();
        words.resize(min_words, capnp::word(0, 0, 0, 0, 0, 0, 0, 0));
        let bytes = unsafe {
            core::slice::from_raw_parts_mut(words.as_mut_ptr() as *mut u8, words.len() * 8)
        };
        f(&mut bytes[..min_len])
    }
}

impl<A> thincan::EncodeCapnp<<A as Atlas>::FileReq> for FileReqValue<A>
where
    A: Atlas,
{
    fn max_encoded_len(&self) -> usize {
        crate::file_req_max_encoded_len()
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

impl<'a, A> thincan::EncodeCapnp<<A as Atlas>::FileReq> for FileOfferValue<'a, A>
where
    A: Atlas,
{
    fn max_encoded_len(&self) -> usize {
        crate::file_offer_max_encoded_len(self.file_metadata.len())
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
            root.set_file_metadata(self.file_metadata);
            root.set_sender_max_chunk_size(self.sender_max_chunk_size);

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

impl<'a, A> thincan::EncodeCapnp<<A as Atlas>::FileChunk> for FileChunkValue<'a, A>
where
    A: Atlas,
{
    fn max_encoded_len(&self) -> usize {
        crate::file_chunk_max_encoded_len(self.data.len())
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

impl<A> thincan::EncodeCapnp<<A as Atlas>::FileAck> for FileAckValue<A>
where
    A: Atlas,
{
    fn max_encoded_len(&self) -> usize {
        crate::file_ack_max_encoded_len()
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
            let mut root: schema::file_ack::Builder = message.init_root();
            root.set_transfer_id(self.transfer_id);
            root.set_kind(self.kind);
            root.set_next_offset(self.next_offset);
            root.set_chunk_size(self.chunk_size);
            root.set_error(self.error);

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
