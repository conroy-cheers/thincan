//! Implementations of the `can-isotp-interface` traits for this crate.

#[cfg(feature = "isotp-interface")]
use can_isotp_interface::{
    IsoTpAsyncEndpoint, IsoTpAsyncEndpointRecvInto, IsoTpEndpoint, IsoTpRxFlowControlConfig,
    RecvControl, RecvError, RecvMeta, RecvMetaIntoStatus, RecvStatus,
    RxFlowControl as IfaceRxFlowControl, SendError,
};

#[cfg(feature = "isotp-interface")]
use crate::{AsyncRuntime, IsoTpAsyncNode, IsoTpError, IsoTpNode, TimeoutKind};

#[cfg(feature = "isotp-interface")]
use core::time::Duration;

#[cfg(feature = "isotp-interface")]
use embedded_can::Frame;

#[cfg(feature = "isotp-interface")]
use embedded_can_interface::{AsyncRxFrameIo, AsyncTxFrameIo, RxFrameIo, TxFrameIo};

#[cfg(feature = "isotp-interface")]
use crate::timer::Clock;

/// Adapter that captures an [`AsyncRuntime`] reference alongside an async node/demux.
#[cfg(feature = "isotp-interface")]
#[derive(Debug)]
pub struct AsyncWithRt<'rt, Rt, Inner> {
    rt: &'rt Rt,
    inner: Inner,
}

#[cfg(feature = "isotp-interface")]
impl<'rt, Rt, Inner> AsyncWithRt<'rt, Rt, Inner> {
    pub fn new(rt: &'rt Rt, inner: Inner) -> Self {
        Self { rt, inner }
    }

    pub fn rt(&self) -> &'rt Rt {
        self.rt
    }

    pub fn inner(&self) -> &Inner {
        &self.inner
    }

    pub fn inner_mut(&mut self) -> &mut Inner {
        &mut self.inner
    }

    pub fn into_inner(self) -> Inner {
        self.inner
    }
}

#[cfg(feature = "isotp-interface")]
impl<'a, Tx, Rx, F, C> IsoTpRxFlowControlConfig for IsoTpNode<'a, Tx, Rx, F, C>
where
    Tx: TxFrameIo<Frame = F>,
    Rx: RxFrameIo<Frame = F, Error = Tx::Error>,
    F: Frame,
    C: Clock,
{
    type Error = IsoTpError<Tx::Error>;

    fn set_rx_flow_control(&mut self, fc: IfaceRxFlowControl) -> Result<(), Self::Error> {
        self.set_rx_flow_control(crate::RxFlowControl {
            block_size: fc.block_size,
            st_min: fc.st_min,
        });
        Ok(())
    }
}

#[cfg(feature = "isotp-interface")]
impl<'a, Tx, Rx, F, C> IsoTpRxFlowControlConfig for IsoTpAsyncNode<'a, Tx, Rx, F, C>
where
    Tx: AsyncTxFrameIo<Frame = F>,
    Rx: AsyncRxFrameIo<Frame = F, Error = Tx::Error>,
    F: Frame,
    C: Clock,
{
    type Error = IsoTpError<Tx::Error>;

    fn set_rx_flow_control(&mut self, fc: IfaceRxFlowControl) -> Result<(), Self::Error> {
        self.set_rx_flow_control(crate::RxFlowControl {
            block_size: fc.block_size,
            st_min: fc.st_min,
        });
        Ok(())
    }
}

#[cfg(feature = "isotp-interface")]
impl<'a, Tx, Rx, F, C> IsoTpEndpoint for IsoTpNode<'a, Tx, Rx, F, C>
where
    Tx: TxFrameIo<Frame = F>,
    Rx: RxFrameIo<Frame = F, Error = Tx::Error>,
    F: Frame,
    C: Clock,
{
    type Error = IsoTpError<Tx::Error>;

    fn send_to(
        &mut self,
        _to: u8,
        payload: &[u8],
        timeout: Duration,
    ) -> Result<(), SendError<Self::Error>> {
        IsoTpNode::send(self, payload, timeout).map_err(|e| match e {
            IsoTpError::Timeout(TimeoutKind::NAs) => SendError::Timeout,
            other => SendError::Backend(other),
        })
    }

    fn send_functional_to(
        &mut self,
        _functional_to: u8,
        payload: &[u8],
        timeout: Duration,
    ) -> Result<(), SendError<Self::Error>> {
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
        Cb: FnMut(RecvMeta, &[u8]) -> Result<RecvControl, Self::Error>,
    {
        let mut delivered = false;
        let mut cb_err: Option<Self::Error> = None;

        let res = IsoTpNode::recv(self, timeout, &mut |payload| {
            delivered = true;
            if cb_err.is_none() {
                if let Err(e) = on_payload(RecvMeta { reply_to: 0 }, payload) {
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

#[cfg(feature = "isotp-interface")]
impl<'rt, 'buf, Rt, Tx, Rx, F, C> IsoTpAsyncEndpoint
    for AsyncWithRt<'rt, Rt, IsoTpAsyncNode<'buf, Tx, Rx, F, C>>
where
    Rt: AsyncRuntime,
    Tx: AsyncTxFrameIo<Frame = F>,
    Rx: AsyncRxFrameIo<Frame = F, Error = Tx::Error>,
    F: Frame,
    C: Clock,
{
    type Error = IsoTpError<Tx::Error>;

    async fn send_to(
        &mut self,
        _to: u8,
        payload: &[u8],
        timeout: Duration,
    ) -> Result<(), SendError<Self::Error>> {
        self.inner
            .send(self.rt, payload, timeout)
            .await
            .map_err(|e| {
                if matches!(e, IsoTpError::Timeout(TimeoutKind::NAs)) {
                    SendError::Timeout
                } else {
                    SendError::Backend(e)
                }
            })
    }

    async fn send_functional_to(
        &mut self,
        _functional_to: u8,
        payload: &[u8],
        timeout: Duration,
    ) -> Result<(), SendError<Self::Error>> {
        self.inner
            .send(self.rt, payload, timeout)
            .await
            .map_err(|e| {
                if matches!(e, IsoTpError::Timeout(TimeoutKind::NAs)) {
                    SendError::Timeout
                } else {
                    SendError::Backend(e)
                }
            })
    }

    async fn recv_one<Cb>(
        &mut self,
        timeout: Duration,
        mut on_payload: Cb,
    ) -> Result<RecvStatus, RecvError<Self::Error>>
    where
        Cb: FnMut(RecvMeta, &[u8]) -> Result<RecvControl, Self::Error>,
    {
        let mut cb_err: Option<Self::Error> = None;

        let res = self
            .inner
            .recv(self.rt, timeout, &mut |payload| {
                if cb_err.is_some() {
                    return;
                }
                if let Err(e) = on_payload(RecvMeta { reply_to: 0 }, payload) {
                    cb_err = Some(e);
                }
            })
            .await;

        if let Some(e) = cb_err {
            return Err(RecvError::Backend(e));
        }

        match res {
            Ok(()) => Ok(RecvStatus::DeliveredOne),
            Err(IsoTpError::Timeout(_)) => Ok(RecvStatus::TimedOut),
            Err(e) => Err(RecvError::Backend(e)),
        }
    }
}

#[cfg(feature = "isotp-interface")]
impl<'rt, 'buf, Rt, Tx, Rx, F, C> IsoTpAsyncEndpointRecvInto
    for AsyncWithRt<'rt, Rt, IsoTpAsyncNode<'buf, Tx, Rx, F, C>>
where
    Rt: AsyncRuntime,
    Tx: AsyncTxFrameIo<Frame = F>,
    Rx: AsyncRxFrameIo<Frame = F, Error = Tx::Error>,
    F: Frame,
    C: Clock,
{
    type Error = IsoTpError<Tx::Error>;

    async fn recv_one_into(
        &mut self,
        timeout: Duration,
        out: &mut [u8],
    ) -> Result<RecvMetaIntoStatus, RecvError<Self::Error>> {
        let mut copied_len: Option<usize> = None;
        let mut cb_err: Option<RecvError<Self::Error>> = None;

        let res = self
            .inner
            .recv(self.rt, timeout, &mut |payload| {
                if copied_len.is_some() || cb_err.is_some() {
                    return;
                }
                if payload.len() > out.len() {
                    cb_err = Some(RecvError::BufferTooSmall {
                        needed: payload.len(),
                        got: out.len(),
                    });
                    return;
                }
                out[..payload.len()].copy_from_slice(payload);
                copied_len = Some(payload.len());
            })
            .await;

        if let Some(e) = cb_err {
            return Err(e);
        }

        match res {
            Ok(()) => Ok(RecvMetaIntoStatus::DeliveredOne {
                meta: RecvMeta { reply_to: 0 },
                len: copied_len.unwrap_or(0),
            }),
            Err(IsoTpError::Timeout(_)) => Ok(RecvMetaIntoStatus::TimedOut),
            Err(e) => Err(RecvError::Backend(e)),
        }
    }
}

#[cfg(all(feature = "isotp-interface", feature = "uds"))]
impl<'a, Tx, Rx, F, C, const MAX: usize> IsoTpEndpoint
    for crate::demux::IsoTpDemux<'a, Tx, Rx, F, C, MAX>
where
    Tx: TxFrameIo<Frame = F>,
    Rx: RxFrameIo<Frame = F, Error = Tx::Error>,
    F: Frame,
    C: Clock,
{
    type Error = IsoTpError<Tx::Error>;

    fn send_to(
        &mut self,
        to: u8,
        payload: &[u8],
        timeout: Duration,
    ) -> Result<(), SendError<Self::Error>> {
        crate::demux::IsoTpDemux::send_to(self, to, payload, timeout).map_err(|e| match e {
            IsoTpError::Timeout(TimeoutKind::NAs) => SendError::Timeout,
            other => SendError::Backend(other),
        })
    }

    fn send_functional_to(
        &mut self,
        functional_to: u8,
        payload: &[u8],
        timeout: Duration,
    ) -> Result<(), SendError<Self::Error>> {
        crate::demux::IsoTpDemux::send_functional_to(self, functional_to, payload, timeout).map_err(
            |e| match e {
                IsoTpError::Timeout(TimeoutKind::NAs) => SendError::Timeout,
                other => SendError::Backend(other),
            },
        )
    }

    fn recv_one<Cb>(
        &mut self,
        timeout: Duration,
        mut on_payload: Cb,
    ) -> Result<RecvStatus, RecvError<Self::Error>>
    where
        Cb: FnMut(RecvMeta, &[u8]) -> Result<RecvControl, Self::Error>,
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

#[cfg(all(feature = "isotp-interface", feature = "uds"))]
impl<'a, Tx, Rx, F, C, const MAX: usize> IsoTpRxFlowControlConfig
    for crate::demux::IsoTpDemux<'a, Tx, Rx, F, C, MAX>
where
    Tx: TxFrameIo<Frame = F>,
    Rx: RxFrameIo<Frame = F, Error = Tx::Error>,
    F: Frame,
    C: Clock,
{
    type Error = IsoTpError<Tx::Error>;

    fn set_rx_flow_control(&mut self, fc: IfaceRxFlowControl) -> Result<(), Self::Error> {
        self.set_rx_flow_control(crate::RxFlowControl {
            block_size: fc.block_size,
            st_min: fc.st_min,
        });
        Ok(())
    }
}

#[cfg(all(feature = "isotp-interface", feature = "uds"))]
impl<'rt, 'buf, Rt, Tx, Rx, F, C, const MAX_PEERS: usize> IsoTpAsyncEndpoint
    for AsyncWithRt<'rt, Rt, crate::async_demux::IsoTpAsyncDemux<'buf, Tx, Rx, F, C, MAX_PEERS>>
where
    Rt: AsyncRuntime,
    Tx: AsyncTxFrameIo<Frame = F>,
    Rx: AsyncRxFrameIo<Frame = F, Error = Tx::Error>,
    F: Frame,
    C: Clock,
{
    type Error = IsoTpError<Tx::Error>;

    async fn send_to(
        &mut self,
        to: u8,
        payload: &[u8],
        timeout: Duration,
    ) -> Result<(), SendError<Self::Error>> {
        let mut app = self.inner.app();
        app.send_to(self.rt, to, payload, timeout)
            .await
            .map_err(|e| {
                if matches!(e, IsoTpError::Timeout(TimeoutKind::NAs)) {
                    SendError::Timeout
                } else {
                    SendError::Backend(e)
                }
            })
    }

    async fn send_functional_to(
        &mut self,
        functional_to: u8,
        payload: &[u8],
        timeout: Duration,
    ) -> Result<(), SendError<Self::Error>> {
        let mut app = self.inner.app();
        app.send_functional_to(self.rt, functional_to, payload, timeout)
            .await
            .map_err(|e| {
                if matches!(e, IsoTpError::Timeout(TimeoutKind::NAs)) {
                    SendError::Timeout
                } else {
                    SendError::Backend(e)
                }
            })
    }

    async fn recv_one<Cb>(
        &mut self,
        timeout: Duration,
        mut on_payload: Cb,
    ) -> Result<RecvStatus, RecvError<Self::Error>>
    where
        Cb: FnMut(RecvMeta, &[u8]) -> Result<RecvControl, Self::Error>,
    {
        let mut app = self.inner.app();
        let mut tmp = [0u8; 4095];
        match app.recv_next_into(self.rt, timeout, &mut tmp).await {
            Ok(None) => Ok(RecvStatus::TimedOut),
            Ok(Some((reply_to, len))) => {
                on_payload(RecvMeta { reply_to }, &tmp[..len]).map_err(RecvError::Backend)?;
                Ok(RecvStatus::DeliveredOne)
            }
            Err(crate::async_demux::AppRecvIntoError::BufferTooSmall { needed, got }) => {
                Err(RecvError::BufferTooSmall { needed, got })
            }
            Err(crate::async_demux::AppRecvIntoError::IsoTp(e)) => Err(RecvError::Backend(e)),
        }
    }
}

#[cfg(all(feature = "isotp-interface", feature = "uds"))]
impl<'rt, 'buf, Rt, Tx, Rx, F, C, const MAX_PEERS: usize> IsoTpAsyncEndpointRecvInto
    for AsyncWithRt<'rt, Rt, crate::async_demux::IsoTpAsyncDemux<'buf, Tx, Rx, F, C, MAX_PEERS>>
where
    Rt: AsyncRuntime,
    Tx: AsyncTxFrameIo<Frame = F>,
    Rx: AsyncRxFrameIo<Frame = F, Error = Tx::Error>,
    F: Frame,
    C: Clock,
{
    type Error = IsoTpError<Tx::Error>;

    async fn recv_one_into(
        &mut self,
        timeout: Duration,
        out: &mut [u8],
    ) -> Result<RecvMetaIntoStatus, RecvError<Self::Error>> {
        let mut app = self.inner.app();
        match app.recv_next_into(self.rt, timeout, out).await {
            Ok(None) => Ok(RecvMetaIntoStatus::TimedOut),
            Ok(Some((reply_to, len))) => Ok(RecvMetaIntoStatus::DeliveredOne {
                meta: RecvMeta { reply_to },
                len,
            }),
            Err(crate::async_demux::AppRecvIntoError::BufferTooSmall { needed, got }) => {
                Err(RecvError::BufferTooSmall { needed, got })
            }
            Err(crate::async_demux::AppRecvIntoError::IsoTp(e)) => Err(RecvError::Backend(e)),
        }
    }
}
