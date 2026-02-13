//! Shared ISO-TP transport traits.
//!
//! This crate defines thin, dependency-light interfaces so applications can
//! use either a userspace ISO-TP implementation or the Linux kernel ISO-TP
//! sockets without changing higher-level code.

#![no_std]
#![allow(async_fn_in_trait)]

use core::time::Duration;

/// Receive-side ISO-TP flow-control parameters.
///
/// These values are advertised to the remote sender via ISO-TP FlowControl (FC) frames. Updating
/// them at runtime allows an application to *shape* the sender's transmission rate based on
/// backpressure (e.g. queue depth, CPU load).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RxFlowControl {
    /// Block size (BS) to advertise (0 = unlimited).
    pub block_size: u8,
    /// Minimum separation time (STmin) to advertise between consecutive frames.
    pub st_min: Duration,
}

/// Optional extension trait: runtime-configurable RX flow-control.
///
/// Implementations should apply the provided parameters to subsequent FlowControl frames emitted
/// in response to segmented transfers.
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
pub struct RecvMeta<T> {
    /// Address to reply to, if applicable.
    pub reply_to: T,
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

/// Result of a receive-into attempt (point-to-point).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecvIntoStatus {
    /// No payload arrived before the timeout elapsed.
    TimedOut,
    /// One payload was delivered into the provided buffer.
    DeliveredOne {
        /// Number of payload bytes written into the output buffer.
        len: usize,
    },
}

/// Result of a receive-into attempt (multi-peer with reply-to metadata).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecvMetaIntoStatus<A>
where
    A: Copy,
{
    /// No payload arrived before the timeout elapsed.
    TimedOut,
    /// One payload was delivered into the provided buffer.
    DeliveredOne {
        /// Reply-to metadata (sender identity).
        meta: RecvMeta<A>,
        /// Number of payload bytes written into the output buffer.
        len: usize,
    },
}

/// Point-to-point ISO-TP endpoint.
///
/// This trait models a single ISO-TP channel with fixed addressing (e.g.
/// one Linux kernel ISO-TP socket bound to specific RX/TX CAN IDs).
pub trait IsoTpEndpoint {
    /// Backend-specific error type.
    type Error;

    /// Send a payload, blocking until completion or timeout.
    fn send(&mut self, payload: &[u8], timeout: Duration) -> Result<(), SendError<Self::Error>>;

    /// Receive at most one payload and deliver it to the callback.
    fn recv_one<Cb>(
        &mut self,
        timeout: Duration,
        on_payload: Cb,
    ) -> Result<RecvStatus, RecvError<Self::Error>>
    where
        Cb: FnMut(&[u8]) -> Result<RecvControl, Self::Error>;
}

/// Async point-to-point ISO-TP endpoint.
///
/// This is the async equivalent of [`IsoTpEndpoint`]. Implementations are expected to be
/// runtime-native (e.g. tokio) or use an adapter that captures whatever runtime handle is needed.
pub trait IsoTpAsyncEndpoint {
    /// Backend-specific error type.
    type Error;

    /// Send a payload, awaiting completion or timeout.
    async fn send(
        &mut self,
        payload: &[u8],
        timeout: Duration,
    ) -> Result<(), SendError<Self::Error>>;

    /// Receive at most one payload and deliver it to the callback.
    async fn recv_one<Cb>(
        &mut self,
        timeout: Duration,
        on_payload: Cb,
    ) -> Result<RecvStatus, RecvError<Self::Error>>
    where
        Cb: FnMut(&[u8]) -> Result<RecvControl, Self::Error>;
}

/// Async point-to-point ISO-TP endpoint (recv-into API).
///
/// This is an alternative to [`IsoTpAsyncEndpoint::recv_one`] that avoids callbacks by copying
/// the received payload into a caller-provided buffer. This shape is required for higher-level
/// protocols that need to `await` during dispatch/handling (e.g. async file I/O backpressure).
pub trait IsoTpAsyncEndpointRecvInto {
    /// Backend-specific error type.
    type Error;

    /// Receive at most one payload and copy it into `out`.
    async fn recv_one_into(
        &mut self,
        timeout: Duration,
        out: &mut [u8],
    ) -> Result<RecvIntoStatus, RecvError<Self::Error>>;
}

/// Async multi-peer ISO-TP endpoint with reply-to metadata.
///
/// This is the async equivalent of [`IsoTpEndpointMeta`].
pub trait IsoTpAsyncEndpointMeta {
    /// Backend-specific error type.
    type Error;
    /// Reply-to address type.
    type ReplyTo: Copy;

    /// Send a payload to a specific peer address.
    async fn send_to(
        &mut self,
        to: Self::ReplyTo,
        payload: &[u8],
        timeout: Duration,
    ) -> Result<(), SendError<Self::Error>>;

    /// Receive at most one payload and deliver it with reply-to metadata.
    async fn recv_one_meta<Cb>(
        &mut self,
        timeout: Duration,
        on_payload: Cb,
    ) -> Result<RecvStatus, RecvError<Self::Error>>
    where
        Cb: FnMut(RecvMeta<Self::ReplyTo>, &[u8]) -> Result<RecvControl, Self::Error>;
}

/// Async multi-peer ISO-TP endpoint with reply-to metadata (recv-into API).
///
/// This is an alternative to [`IsoTpAsyncEndpointMeta::recv_one_meta`] that avoids callbacks by
/// copying the received payload into a caller-provided buffer.
pub trait IsoTpAsyncEndpointMetaRecvInto {
    /// Backend-specific error type.
    type Error;
    /// Reply-to address type.
    type ReplyTo: Copy;

    /// Receive at most one payload and copy it into `out`.
    async fn recv_one_meta_into(
        &mut self,
        timeout: Duration,
        out: &mut [u8],
    ) -> Result<RecvMetaIntoStatus<Self::ReplyTo>, RecvError<Self::Error>>;
}

/// Multi-peer ISO-TP endpoint with reply-to metadata.
///
/// This is intended for UDS normal-fixed addressing, where multiple peers
/// share the same CAN IDs and the source address identifies the sender.
pub trait IsoTpEndpointMeta {
    /// Backend-specific error type.
    type Error;
    /// Reply-to address type.
    type ReplyTo: Copy;

    /// Send a payload to a specific peer address.
    fn send_to(
        &mut self,
        to: Self::ReplyTo,
        payload: &[u8],
        timeout: Duration,
    ) -> Result<(), SendError<Self::Error>>;

    /// Receive at most one payload and deliver it with reply-to metadata.
    fn recv_one_meta<Cb>(
        &mut self,
        timeout: Duration,
        on_payload: Cb,
    ) -> Result<RecvStatus, RecvError<Self::Error>>
    where
        Cb: FnMut(RecvMeta<Self::ReplyTo>, &[u8]) -> Result<RecvControl, Self::Error>;
}

#[cfg(test)]
mod tests {
    extern crate std;
    use core::future::Future;
    use core::pin::Pin;
    use core::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};
    use std::vec::Vec;

    use super::*;

    fn block_on<F: Future>(mut fut: F) -> F::Output {
        // A tiny single-threaded executor for tests (no timers).
        unsafe fn clone(_: *const ()) -> RawWaker {
            RawWaker::new(core::ptr::null(), &VTABLE)
        }
        unsafe fn wake(_: *const ()) {}
        unsafe fn wake_by_ref(_: *const ()) {}
        unsafe fn drop(_: *const ()) {}
        static VTABLE: RawWakerVTable = RawWakerVTable::new(clone, wake, wake_by_ref, drop);

        let waker = unsafe { Waker::from_raw(RawWaker::new(core::ptr::null(), &VTABLE)) };
        let mut cx = Context::from_waker(&waker);
        // SAFETY: `fut` lives on the stack for the duration of this function.
        let mut fut = unsafe { Pin::new_unchecked(&mut fut) };
        loop {
            match fut.as_mut().poll(&mut cx) {
                Poll::Ready(v) => return v,
                Poll::Pending => std::thread::yield_now(),
            }
        }
    }

    #[derive(Debug, Default)]
    struct Dummy {
        next_recv: Option<Vec<u8>>,
        last_sent: Option<Vec<u8>>,
    }

    impl IsoTpEndpoint for Dummy {
        type Error = ();

        fn send(
            &mut self,
            payload: &[u8],
            _timeout: Duration,
        ) -> Result<(), SendError<Self::Error>> {
            self.last_sent = Some(payload.to_vec());
            Ok(())
        }

        fn recv_one<Cb>(
            &mut self,
            _timeout: Duration,
            mut on_payload: Cb,
        ) -> Result<RecvStatus, RecvError<Self::Error>>
        where
            Cb: FnMut(&[u8]) -> Result<RecvControl, Self::Error>,
        {
            match self.next_recv.take() {
                Some(data) => {
                    let _ = on_payload(&data).map_err(RecvError::Backend)?;
                    Ok(RecvStatus::DeliveredOne)
                }
                None => Ok(RecvStatus::TimedOut),
            }
        }
    }

    #[test]
    fn dummy_endpoint_round_trips_payload() {
        let mut ep = Dummy::default();
        ep.send(b"hello", Duration::from_millis(1)).unwrap();
        assert_eq!(ep.last_sent.as_deref(), Some(b"hello".as_ref()));

        ep.next_recv = Some(b"world".to_vec());
        let mut got = Vec::new();
        let status = ep
            .recv_one(Duration::from_millis(1), |payload| {
                got.extend_from_slice(payload);
                Ok(RecvControl::Continue)
            })
            .unwrap();
        assert_eq!(status, RecvStatus::DeliveredOne);
        assert_eq!(got, b"world");
    }

    #[test]
    fn dummy_endpoint_timeout() {
        let mut ep = Dummy::default();
        let status = ep
            .recv_one(Duration::from_millis(1), |_payload| {
                Ok(RecvControl::Continue)
            })
            .unwrap();
        assert_eq!(status, RecvStatus::TimedOut);
    }

    #[derive(Debug, Default)]
    struct DummyAsync {
        next_recv: Option<Vec<u8>>,
        last_sent: Option<Vec<u8>>,
    }

    impl IsoTpAsyncEndpoint for DummyAsync {
        type Error = ();

        async fn send(
            &mut self,
            payload: &[u8],
            _timeout: Duration,
        ) -> Result<(), SendError<Self::Error>> {
            self.last_sent = Some(payload.to_vec());
            Ok(())
        }

        async fn recv_one<Cb>(
            &mut self,
            _timeout: Duration,
            mut on_payload: Cb,
        ) -> Result<RecvStatus, RecvError<Self::Error>>
        where
            Cb: FnMut(&[u8]) -> Result<RecvControl, Self::Error>,
        {
            match self.next_recv.take() {
                Some(data) => {
                    let _ = on_payload(&data).map_err(RecvError::Backend)?;
                    Ok(RecvStatus::DeliveredOne)
                }
                None => Ok(RecvStatus::TimedOut),
            }
        }
    }

    #[test]
    fn dummy_async_endpoint_round_trips_payload() {
        block_on(async {
            let mut ep = DummyAsync::default();
            ep.send(b"hello", Duration::from_millis(1)).await.unwrap();
            assert_eq!(ep.last_sent.as_deref(), Some(b"hello".as_ref()));

            ep.next_recv = Some(b"world".to_vec());
            let mut got = Vec::new();
            let status = ep
                .recv_one(Duration::from_millis(1), |payload| {
                    got.extend_from_slice(payload);
                    Ok(RecvControl::Continue)
                })
                .await
                .unwrap();
            assert_eq!(status, RecvStatus::DeliveredOne);
            assert_eq!(got, b"world");
        })
    }
}
