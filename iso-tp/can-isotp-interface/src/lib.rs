//! Shared UDS-over-ISO-TP transport traits.
//!
//! This crate defines dependency-light interfaces for 29-bit UDS-style ISO-TP transports where
//! messages are always addressed to a peer (`u8` source/target addresses).

#![no_std]
#![allow(async_fn_in_trait)]

use core::time::Duration;

/// Receive-side ISO-TP flow-control parameters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RxFlowControl {
    /// Block size (BS) to advertise (0 = unlimited).
    pub block_size: u8,
    /// Minimum separation time (STmin) to advertise between consecutive frames.
    pub st_min: Duration,
}

/// Optional extension trait: runtime-configurable RX flow-control.
pub trait IsoTpRxFlowControlConfig {
    /// Backend-specific error type.
    type Error;

    /// Update receive-side FlowControl parameters.
    fn set_rx_flow_control(&mut self, fc: RxFlowControl) -> Result<(), Self::Error>;
}

/// Result of a receive attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecvStatus {
    /// No payload arrived before the timeout elapsed.
    TimedOut,
    /// One payload was delivered to the callback.
    DeliveredOne,
}

/// Control signal for receive callbacks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecvControl {
    /// Continue processing future payloads.
    Continue,
    /// Stop processing after the current payload.
    Stop,
}

/// Metadata accompanying a received payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RecvMeta {
    /// Address to reply to.
    pub reply_to: u8,
}

/// Error for a send attempt.
#[derive(Debug)]
pub enum SendError<E> {
    /// The send did not complete before the timeout elapsed.
    Timeout,
    /// Backend-specific error.
    Backend(E),
}

/// Error for a receive attempt.
#[derive(Debug)]
pub enum RecvError<E> {
    /// Caller-provided buffer was too small to hold the received payload.
    BufferTooSmall {
        /// Needed payload length in bytes.
        needed: usize,
        /// Provided buffer length in bytes.
        got: usize,
    },
    /// Backend-specific error.
    Backend(E),
}

/// Result of a receive-into attempt (multi-peer with reply-to metadata).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecvMetaIntoStatus {
    /// No payload arrived before the timeout elapsed.
    TimedOut,
    /// One payload was delivered into the provided buffer.
    DeliveredOne {
        /// Reply-to metadata (sender identity).
        meta: RecvMeta,
        /// Number of payload bytes written into the output buffer.
        len: usize,
    },
}

/// UDS-aware ISO-TP endpoint.
///
/// This trait is addressed: sends target a peer address and receives report `reply_to` metadata.
pub trait IsoTpEndpoint {
    /// Backend-specific error type.
    type Error;

    /// Send a payload to a specific peer address.
    fn send_to(
        &mut self,
        to: u8,
        payload: &[u8],
        timeout: Duration,
    ) -> Result<(), SendError<Self::Error>>;

    /// Receive at most one payload and deliver it with reply-to metadata.
    fn recv_one<Cb>(
        &mut self,
        timeout: Duration,
        on_payload: Cb,
    ) -> Result<RecvStatus, RecvError<Self::Error>>
    where
        Cb: FnMut(RecvMeta, &[u8]) -> Result<RecvControl, Self::Error>;
}

/// Async UDS-aware ISO-TP endpoint.
pub trait IsoTpAsyncEndpoint {
    /// Backend-specific error type.
    type Error;

    /// Send a payload to a specific peer address.
    async fn send_to(
        &mut self,
        to: u8,
        payload: &[u8],
        timeout: Duration,
    ) -> Result<(), SendError<Self::Error>>;

    /// Receive at most one payload and deliver it with reply-to metadata.
    async fn recv_one<Cb>(
        &mut self,
        timeout: Duration,
        on_payload: Cb,
    ) -> Result<RecvStatus, RecvError<Self::Error>>
    where
        Cb: FnMut(RecvMeta, &[u8]) -> Result<RecvControl, Self::Error>;
}

/// Async UDS-aware ISO-TP endpoint (recv-into API).
pub trait IsoTpAsyncEndpointRecvInto {
    /// Backend-specific error type.
    type Error;

    /// Receive at most one payload and copy it into `out`.
    async fn recv_one_into(
        &mut self,
        timeout: Duration,
        out: &mut [u8],
    ) -> Result<RecvMetaIntoStatus, RecvError<Self::Error>>;
}

/// Backward-compatible alias trait name for addressed sync endpoints.
pub trait IsoTpEndpointMeta: IsoTpEndpoint {}

impl<T> IsoTpEndpointMeta for T where T: IsoTpEndpoint {}

/// Backward-compatible alias trait name for addressed async endpoints.
pub trait IsoTpAsyncEndpointMeta: IsoTpAsyncEndpoint {}

impl<T> IsoTpAsyncEndpointMeta for T where T: IsoTpAsyncEndpoint {}

/// Backward-compatible alias trait name for addressed async recv-into endpoints.
pub trait IsoTpAsyncEndpointMetaRecvInto: IsoTpAsyncEndpointRecvInto {}

impl<T> IsoTpAsyncEndpointMetaRecvInto for T where T: IsoTpAsyncEndpointRecvInto {}

#[cfg(test)]
mod tests {
    extern crate std;

    use super::*;
    use core::future::Future;
    use core::pin::Pin;
    use core::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};
    use std::vec;
    use std::vec::Vec;

    fn block_on<F: Future>(mut fut: F) -> F::Output {
        fn raw_waker() -> RawWaker {
            fn clone(_: *const ()) -> RawWaker {
                raw_waker()
            }
            fn wake(_: *const ()) {}
            fn wake_by_ref(_: *const ()) {}
            fn drop(_: *const ()) {}
            let vtable = &RawWakerVTable::new(clone, wake, wake_by_ref, drop);
            RawWaker::new(core::ptr::null(), vtable)
        }

        // SAFETY: no-op waker is sufficient for these immediately-ready futures.
        let waker = unsafe { Waker::from_raw(raw_waker()) };
        let mut cx = Context::from_waker(&waker);
        // SAFETY: we do not move `fut` after pinning.
        let mut fut = unsafe { Pin::new_unchecked(&mut fut) };
        loop {
            match fut.as_mut().poll(&mut cx) {
                Poll::Ready(v) => return v,
                Poll::Pending => {}
            }
        }
    }

    #[derive(Default)]
    struct Dummy {
        sent_to: Vec<u8>,
        sent_payloads: Vec<Vec<u8>>,
        inbox: Vec<(u8, Vec<u8>)>,
    }

    impl IsoTpEndpoint for Dummy {
        type Error = ();

        fn send_to(
            &mut self,
            to: u8,
            payload: &[u8],
            _timeout: Duration,
        ) -> Result<(), SendError<Self::Error>> {
            self.sent_to.push(to);
            self.sent_payloads.push(payload.to_vec());
            Ok(())
        }

        fn recv_one<Cb>(
            &mut self,
            _timeout: Duration,
            mut on_payload: Cb,
        ) -> Result<RecvStatus, RecvError<Self::Error>>
        where
            Cb: FnMut(RecvMeta, &[u8]) -> Result<RecvControl, Self::Error>,
        {
            if let Some((reply_to, data)) = self.inbox.pop() {
                let _ = on_payload(RecvMeta { reply_to }, &data)
                    .map_err(RecvError::Backend)?;
                return Ok(RecvStatus::DeliveredOne);
            }
            Ok(RecvStatus::TimedOut)
        }
    }

    impl IsoTpAsyncEndpoint for Dummy {
        type Error = ();

        async fn send_to(
            &mut self,
            to: u8,
            payload: &[u8],
            timeout: Duration,
        ) -> Result<(), SendError<Self::Error>> {
            IsoTpEndpoint::send_to(self, to, payload, timeout)
        }

        async fn recv_one<Cb>(
            &mut self,
            timeout: Duration,
            on_payload: Cb,
        ) -> Result<RecvStatus, RecvError<Self::Error>>
        where
            Cb: FnMut(RecvMeta, &[u8]) -> Result<RecvControl, Self::Error>,
        {
            IsoTpEndpoint::recv_one(self, timeout, on_payload)
        }
    }

    impl IsoTpAsyncEndpointRecvInto for Dummy {
        type Error = ();

        async fn recv_one_into(
            &mut self,
            _timeout: Duration,
            out: &mut [u8],
        ) -> Result<RecvMetaIntoStatus, RecvError<Self::Error>> {
            if let Some((reply_to, data)) = self.inbox.pop() {
                if data.len() > out.len() {
                    return Err(RecvError::BufferTooSmall {
                        needed: data.len(),
                        got: out.len(),
                    });
                }
                out[..data.len()].copy_from_slice(&data);
                return Ok(RecvMetaIntoStatus::DeliveredOne {
                    meta: RecvMeta { reply_to },
                    len: data.len(),
                });
            }
            Ok(RecvMetaIntoStatus::TimedOut)
        }
    }

    #[test]
    fn sync_trait_is_addressed() {
        let mut d = Dummy::default();
        IsoTpEndpoint::send_to(
            &mut d,
            0x33,
            b"abc",
            Duration::from_millis(1),
        )
        .unwrap();
        assert_eq!(d.sent_to, vec![0x33]);
        assert_eq!(d.sent_payloads, vec![b"abc".to_vec()]);
    }

    #[test]
    fn async_trait_is_addressed() {
        let mut d = Dummy::default();
        block_on(IsoTpAsyncEndpoint::send_to(
            &mut d,
            0x44,
            b"xyz",
            Duration::from_millis(1),
        ))
        .unwrap();
        assert_eq!(d.sent_to, vec![0x44]);
        assert_eq!(d.sent_payloads, vec![b"xyz".to_vec()]);
    }
}
