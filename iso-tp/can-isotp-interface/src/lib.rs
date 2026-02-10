//! Shared ISO-TP transport traits.
//!
//! This crate defines thin, dependency-light interfaces so applications can
//! use either a userspace ISO-TP implementation or the Linux kernel ISO-TP
//! sockets without changing higher-level code.

#![no_std]

use core::time::Duration;

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
    /// Backend-specific error.
    Backend(E),
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
    use std::vec::Vec;

    use super::*;

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
}
