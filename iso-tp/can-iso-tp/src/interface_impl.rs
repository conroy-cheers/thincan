//! Implementations of the `can-isotp-interface` traits for this crate.

#[cfg(feature = "isotp-interface")]
use can_isotp_interface::{RecvControl, RecvError, RecvMeta, RecvStatus, SendError};

#[cfg(feature = "isotp-interface")]
use crate::{IsoTpError, IsoTpNode, TimeoutKind};

#[cfg(feature = "isotp-interface")]
use core::time::Duration;

#[cfg(feature = "isotp-interface")]
use embedded_can::Frame;

#[cfg(feature = "isotp-interface")]
use embedded_can_interface::{RxFrameIo, TxFrameIo};

#[cfg(feature = "isotp-interface")]
use crate::timer::Clock;

#[cfg(feature = "isotp-interface")]
impl<'a, Tx, Rx, F, C> can_isotp_interface::IsoTpEndpoint for IsoTpNode<'a, Tx, Rx, F, C>
where
    Tx: TxFrameIo<Frame = F>,
    Rx: RxFrameIo<Frame = F, Error = Tx::Error>,
    F: Frame,
    C: Clock,
{
    type Error = IsoTpError<Tx::Error>;

    fn send(&mut self, payload: &[u8], timeout: Duration) -> Result<(), SendError<Self::Error>> {
        IsoTpNode::send(self, payload, timeout).map_err(|e| match e {
            IsoTpError::Timeout(TimeoutKind::NAs) => SendError::Timeout,
            other => SendError::Backend(other),
        })
    }

    fn recv_one<Cb>(
        &mut self,
        timeout: Duration,
        mut on_payload: Cb,
    ) -> Result<RecvStatus, RecvError<Self::Error>>
    where
        Cb: FnMut(&[u8]) -> Result<RecvControl, Self::Error>,
    {
        let mut delivered = false;
        let mut cb_err: Option<Self::Error> = None;

        let res = IsoTpNode::recv(self, timeout, &mut |payload| {
            delivered = true;
            if cb_err.is_none() {
                if let Err(e) = on_payload(payload) {
                    cb_err = Some(e);
                }
            }
        });

        if let Some(e) = cb_err {
            return Err(RecvError::Backend(e));
        }

        match res {
            Ok(()) => Ok(if delivered {
                RecvStatus::DeliveredOne
            } else {
                RecvStatus::TimedOut
            }),
            Err(IsoTpError::Timeout(_)) => Ok(RecvStatus::TimedOut),
            Err(e) => Err(RecvError::Backend(e)),
        }
    }
}

#[cfg(all(feature = "isotp-interface", feature = "uds"))]
impl<'a, Tx, Rx, F, C, const MAX: usize> can_isotp_interface::IsoTpEndpointMeta
    for crate::demux::IsoTpDemux<'a, Tx, Rx, F, C, MAX>
where
    Tx: TxFrameIo<Frame = F>,
    Rx: RxFrameIo<Frame = F, Error = Tx::Error>,
    F: Frame,
    C: Clock,
{
    type Error = IsoTpError<Tx::Error>;
    type ReplyTo = u8;

    fn send_to(
        &mut self,
        to: Self::ReplyTo,
        payload: &[u8],
        timeout: Duration,
    ) -> Result<(), SendError<Self::Error>> {
        crate::demux::IsoTpDemux::send_to(self, to, payload, timeout).map_err(|e| match e {
            IsoTpError::Timeout(TimeoutKind::NAs) => SendError::Timeout,
            other => SendError::Backend(other),
        })
    }

    fn recv_one_meta<Cb>(
        &mut self,
        timeout: Duration,
        mut on_payload: Cb,
    ) -> Result<RecvStatus, RecvError<Self::Error>>
    where
        Cb: FnMut(RecvMeta<Self::ReplyTo>, &[u8]) -> Result<RecvControl, Self::Error>,
    {
        let mut delivered = false;
        let mut cb_err: Option<Self::Error> = None;

        let res = crate::demux::IsoTpDemux::recv(self, timeout, &mut |reply_to, payload| {
            delivered = true;
            if cb_err.is_none() {
                if let Err(e) = on_payload(RecvMeta { reply_to }, payload) {
                    cb_err = Some(e);
                }
            }
        });

        if let Some(e) = cb_err {
            return Err(RecvError::Backend(e));
        }

        match res {
            Ok(()) => Ok(if delivered {
                RecvStatus::DeliveredOne
            } else {
                RecvStatus::TimedOut
            }),
            Err(IsoTpError::Timeout(_)) => Ok(RecvStatus::TimedOut),
            Err(e) => Err(RecvError::Backend(e)),
        }
    }
}
