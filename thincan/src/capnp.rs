//! Cap'n Proto encoding helpers.

use core::marker::PhantomData;

use crate::{Encode, Error, ErrorKind, Message};

/// Borrowed Cap'n Proto scratch space (no-alloc).
#[derive(Debug)]
pub struct CapnpScratchRef<'a> {
    buf: &'a mut [u8],
}

impl<'a> CapnpScratchRef<'a> {
    /// Wrap a caller-provided scratch buffer.
    pub fn new(buf: &'a mut [u8]) -> Self {
        Self { buf }
    }

    /// Scratch capacity in bytes.
    pub fn capacity(&self) -> usize {
        self.buf.len()
    }

    fn as_mut(&mut self) -> &mut [u8] {
        self.buf
    }

    /// Build a Cap'n Proto value that implements `thincan::Encode`.
    pub fn builder_value<M, S, F>(&mut self, f: F) -> CapnpBuilderValue<'_, M, S, F>
    where
        M: Message,
        S: ::capnp::traits::Owned,
        F: for<'b> Fn(
            &mut <S as ::capnp::traits::Owned>::Builder<'b>,
        ) -> Result<(), ::capnp::Error>,
    {
        let scratch = self.as_mut();
        CapnpBuilderValue {
            scratch: core::ptr::NonNull::from(scratch),
            f,
            _marker: PhantomData,
        }
    }
}

/// Value wrapper that encodes a Cap'n Proto root into a thincan message body.
pub struct CapnpBuilderValue<'a, M, S, F> {
    scratch: core::ptr::NonNull<[u8]>,
    f: F,
    _marker: PhantomData<(M, S, &'a mut [u8])>,
}

impl<'a, M, S, F> Encode<M> for CapnpBuilderValue<'a, M, S, F>
where
    M: Message,
    S: ::capnp::traits::Owned,
    F: for<'b> Fn(&mut <S as ::capnp::traits::Owned>::Builder<'b>) -> Result<(), ::capnp::Error>,
{
    fn max_encoded_len(&self) -> usize {
        unsafe { (&*self.scratch.as_ptr()).len() }
    }

    fn encode(&self, out: &mut [u8]) -> Result<usize, Error> {
        let scratch = unsafe { &mut *self.scratch.as_ptr() };
        let mut message =
            ::capnp::message::Builder::new(::capnp::message::SingleSegmentAllocator::new(scratch));
        let mut root: <S as ::capnp::traits::Owned>::Builder<'_> = message.init_root();
        (self.f)(&mut root).map_err(|_| Error {
            kind: ErrorKind::Other,
        })?;
        drop(root);

        let segments = message.get_segments_for_output();
        if segments.len() != 1 {
            return Err(Error {
                kind: ErrorKind::Other,
            });
        }
        let bytes = segments[0];
        if bytes.len() > out.len() {
            return Err(Error {
                kind: ErrorKind::BufferTooSmall {
                    needed: bytes.len(),
                    got: out.len(),
                },
            });
        }
        out[..bytes.len()].copy_from_slice(bytes);
        Ok(bytes.len())
    }
}
