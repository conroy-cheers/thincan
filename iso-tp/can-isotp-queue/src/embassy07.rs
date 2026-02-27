use core::marker::PhantomData;
use core::time::Duration;

use can_isotp_interface::{
    IsoTpAsyncEndpoint, IsoTpAsyncEndpointRecvInto, RecvControl, RecvError, RecvMeta,
    RecvMetaIntoStatus, RecvStatus, SendError,
};
use embassy_futures::yield_now;
use embassy_sync_07::blocking_mutex::raw::RawMutex;
use embassy_sync_07::channel::Channel;
use embassy_sync_07::mutex::Mutex;

use crate::common::{ActorConfig, ActorHooks, QueueIsoTpError, QueueKind};

const BACKPRESSURE_LOG_THRESHOLD: Duration = Duration::from_millis(1);

#[derive(Debug)]
pub struct IsoTpTxRequest<const MAX_PAYLOAD: usize> {
    pub to: u8,
    pub is_functional: bool,
    pub timeout: Duration,
    pub len: usize,
    pub payload: [u8; MAX_PAYLOAD],
}

#[derive(Debug)]
pub enum IsoTpTxResponse<E> {
    Ok,
    Timeout,
    Backend(E),
}

#[derive(Debug)]
pub enum IsoTpRxEvent<E, const MAX_PAYLOAD: usize> {
    Delivered {
        from: u8,
        len: usize,
        payload: [u8; MAX_PAYLOAD],
    },
    BufferTooSmall {
        needed: usize,
        got: usize,
    },
    Backend(E),
}

pub struct QueueResources<
    RM,
    E,
    const MAX_PAYLOAD: usize,
    const TX_REQ_DEPTH: usize,
    const TX_RESP_DEPTH: usize,
    const RX_EVT_DEPTH: usize,
> where
    RM: RawMutex + 'static,
    E: 'static,
{
    tx_req: Channel<RM, IsoTpTxRequest<MAX_PAYLOAD>, TX_REQ_DEPTH>,
    tx_resp: Channel<RM, IsoTpTxResponse<E>, TX_RESP_DEPTH>,
    rx_evt: Channel<RM, IsoTpRxEvent<E, MAX_PAYLOAD>, RX_EVT_DEPTH>,
    tx_rpc_lock: Mutex<RM, ()>,
}

impl<
        RM,
        E,
        const MAX_PAYLOAD: usize,
        const TX_REQ_DEPTH: usize,
        const TX_RESP_DEPTH: usize,
        const RX_EVT_DEPTH: usize,
    > QueueResources<RM, E, MAX_PAYLOAD, TX_REQ_DEPTH, TX_RESP_DEPTH, RX_EVT_DEPTH>
where
    RM: RawMutex + 'static,
    E: 'static,
{
    pub const fn new() -> Self {
        Self {
            tx_req: Channel::new(),
            tx_resp: Channel::new(),
            rx_evt: Channel::new(),
            tx_rpc_lock: Mutex::new(()),
        }
    }

    pub fn split_endpoints(
        &'static self,
    ) -> (
        QueueIsoTpEndpoint<RM, E, MAX_PAYLOAD, TX_REQ_DEPTH, TX_RESP_DEPTH, RX_EVT_DEPTH>,
        QueueIsoTpEndpoint<RM, E, MAX_PAYLOAD, TX_REQ_DEPTH, TX_RESP_DEPTH, RX_EVT_DEPTH>,
    ) {
        let tx = QueueIsoTpEndpoint::new(self);
        let rx = QueueIsoTpEndpoint::new(self);
        (tx, rx)
    }
}

impl<
        RM,
        E,
        const MAX_PAYLOAD: usize,
        const TX_REQ_DEPTH: usize,
        const TX_RESP_DEPTH: usize,
        const RX_EVT_DEPTH: usize,
    > Default for QueueResources<RM, E, MAX_PAYLOAD, TX_REQ_DEPTH, TX_RESP_DEPTH, RX_EVT_DEPTH>
where
    RM: RawMutex + 'static,
    E: 'static,
{
    fn default() -> Self {
        Self::new()
    }
}

pub struct QueueIsoTpEndpoint<
    RM,
    E,
    const MAX_PAYLOAD: usize,
    const TX_REQ_DEPTH: usize,
    const TX_RESP_DEPTH: usize,
    const RX_EVT_DEPTH: usize,
> where
    RM: RawMutex + 'static,
    E: 'static,
{
    resources: &'static QueueResources<RM, E, MAX_PAYLOAD, TX_REQ_DEPTH, TX_RESP_DEPTH, RX_EVT_DEPTH>,
    _marker: PhantomData<fn() -> E>,
}

impl<
        RM,
        E,
        const MAX_PAYLOAD: usize,
        const TX_REQ_DEPTH: usize,
        const TX_RESP_DEPTH: usize,
        const RX_EVT_DEPTH: usize,
    > Copy for QueueIsoTpEndpoint<RM, E, MAX_PAYLOAD, TX_REQ_DEPTH, TX_RESP_DEPTH, RX_EVT_DEPTH>
where
    RM: RawMutex + 'static,
    E: 'static,
{
}

impl<
        RM,
        E,
        const MAX_PAYLOAD: usize,
        const TX_REQ_DEPTH: usize,
        const TX_RESP_DEPTH: usize,
        const RX_EVT_DEPTH: usize,
    > Clone for QueueIsoTpEndpoint<RM, E, MAX_PAYLOAD, TX_REQ_DEPTH, TX_RESP_DEPTH, RX_EVT_DEPTH>
where
    RM: RawMutex + 'static,
    E: 'static,
{
    fn clone(&self) -> Self {
        *self
    }
}

impl<
        RM,
        E,
        const MAX_PAYLOAD: usize,
        const TX_REQ_DEPTH: usize,
        const TX_RESP_DEPTH: usize,
        const RX_EVT_DEPTH: usize,
    > QueueIsoTpEndpoint<RM, E, MAX_PAYLOAD, TX_REQ_DEPTH, TX_RESP_DEPTH, RX_EVT_DEPTH>
where
    RM: RawMutex + 'static,
    E: 'static,
{
    const fn new(
        resources: &'static QueueResources<
            RM,
            E,
            MAX_PAYLOAD,
            TX_REQ_DEPTH,
            TX_RESP_DEPTH,
            RX_EVT_DEPTH,
        >,
    ) -> Self {
        Self {
            resources,
            _marker: PhantomData,
        }
    }

    fn map_tx_response(resp: IsoTpTxResponse<E>) -> Result<(), SendError<QueueIsoTpError<E>>> {
        match resp {
            IsoTpTxResponse::Ok => Ok(()),
            IsoTpTxResponse::Timeout => Err(SendError::Timeout),
            IsoTpTxResponse::Backend(e) => Err(SendError::Backend(QueueIsoTpError::Transport(e))),
        }
    }
}

impl<
        RM,
        E: 'static,
        const MAX_PAYLOAD: usize,
        const TX_REQ_DEPTH: usize,
        const TX_RESP_DEPTH: usize,
        const RX_EVT_DEPTH: usize,
    > IsoTpAsyncEndpoint
    for QueueIsoTpEndpoint<RM, E, MAX_PAYLOAD, TX_REQ_DEPTH, TX_RESP_DEPTH, RX_EVT_DEPTH>
where
    RM: RawMutex + 'static,
{
    type Error = QueueIsoTpError<E>;

    async fn send_to(
        &mut self,
        to: u8,
        payload: &[u8],
        timeout: Duration,
    ) -> Result<(), SendError<Self::Error>> {
        if payload.len() > MAX_PAYLOAD {
            return Err(SendError::Backend(QueueIsoTpError::PayloadTooLarge {
                needed: payload.len(),
                capacity: MAX_PAYLOAD,
            }));
        }

        let mut req_payload = [0u8; MAX_PAYLOAD];
        req_payload[..payload.len()].copy_from_slice(payload);
        let req = IsoTpTxRequest {
            to,
            is_functional: false,
            timeout,
            len: payload.len(),
            payload: req_payload,
        };

        let _guard = self.resources.tx_rpc_lock.lock().await;
        while self.resources.tx_resp.try_receive().is_ok() {}
        self.resources.tx_req.send(req).await;
        let resp = self.resources.tx_resp.receive().await;
        Self::map_tx_response(resp)
    }

    async fn send_functional_to(
        &mut self,
        functional_to: u8,
        payload: &[u8],
        timeout: Duration,
    ) -> Result<(), SendError<Self::Error>> {
        if payload.len() > MAX_PAYLOAD {
            return Err(SendError::Backend(QueueIsoTpError::PayloadTooLarge {
                needed: payload.len(),
                capacity: MAX_PAYLOAD,
            }));
        }

        let mut req_payload = [0u8; MAX_PAYLOAD];
        req_payload[..payload.len()].copy_from_slice(payload);
        let req = IsoTpTxRequest {
            to: functional_to,
            is_functional: true,
            timeout,
            len: payload.len(),
            payload: req_payload,
        };

        let _guard = self.resources.tx_rpc_lock.lock().await;
        while self.resources.tx_resp.try_receive().is_ok() {}
        self.resources.tx_req.send(req).await;
        let resp = self.resources.tx_resp.receive().await;
        Self::map_tx_response(resp)
    }

    async fn recv_one<Cb>(
        &mut self,
        timeout: Duration,
        mut on_payload: Cb,
    ) -> Result<RecvStatus, RecvError<Self::Error>>
    where
        Cb: FnMut(RecvMeta, &[u8]) -> Result<RecvControl, Self::Error>,
    {
        let mut tmp = [0u8; MAX_PAYLOAD];
        match self.recv_one_into(timeout, &mut tmp).await? {
            RecvMetaIntoStatus::TimedOut => Ok(RecvStatus::TimedOut),
            RecvMetaIntoStatus::DeliveredOne { meta, len } => {
                let _ = on_payload(meta, &tmp[..len]).map_err(RecvError::Backend)?;
                Ok(RecvStatus::DeliveredOne)
            }
        }
    }
}

impl<
        RM,
        E: 'static,
        const MAX_PAYLOAD: usize,
        const TX_REQ_DEPTH: usize,
        const TX_RESP_DEPTH: usize,
        const RX_EVT_DEPTH: usize,
    > IsoTpAsyncEndpointRecvInto
    for QueueIsoTpEndpoint<RM, E, MAX_PAYLOAD, TX_REQ_DEPTH, TX_RESP_DEPTH, RX_EVT_DEPTH>
where
    RM: RawMutex,
{
    type Error = QueueIsoTpError<E>;

    async fn recv_one_into(
        &mut self,
        timeout: Duration,
        out: &mut [u8],
    ) -> Result<RecvMetaIntoStatus, RecvError<Self::Error>> {
        let wait =
            embassy_time::with_timeout(core_to_embassy_duration(timeout), self.resources.rx_evt.receive())
                .await;

        let evt = match wait {
            Ok(evt) => evt,
            Err(_) => return Ok(RecvMetaIntoStatus::TimedOut),
        };

        match evt {
            IsoTpRxEvent::Delivered { from, len, payload } => {
                if out.len() < len {
                    return Err(RecvError::BufferTooSmall {
                        needed: len,
                        got: out.len(),
                    });
                }
                out[..len].copy_from_slice(&payload[..len]);
                Ok(RecvMetaIntoStatus::DeliveredOne {
                    meta: RecvMeta { reply_to: from },
                    len,
                })
            }
            IsoTpRxEvent::BufferTooSmall { needed, got } => {
                Err(RecvError::BufferTooSmall { needed, got })
            }
            IsoTpRxEvent::Backend(e) => Err(RecvError::Backend(QueueIsoTpError::Transport(e))),
        }
    }
}

async fn send_rx_event<
    RM,
    E,
    H,
    const MAX_PAYLOAD: usize,
    const TX_REQ_DEPTH: usize,
    const TX_RESP_DEPTH: usize,
    const RX_EVT_DEPTH: usize,
>(
    hooks: &H,
    resources: &'static QueueResources<RM, E, MAX_PAYLOAD, TX_REQ_DEPTH, TX_RESP_DEPTH, RX_EVT_DEPTH>,
    evt: IsoTpRxEvent<E, MAX_PAYLOAD>,
) where
    RM: RawMutex + 'static,
    E: 'static,
    H: ActorHooks<E>,
{
    let started = embassy_time::Instant::now();
    resources.rx_evt.send(evt).await;
    if let Some(blocked) = embassy_time::Instant::now().checked_duration_since(started)
        && blocked >= core_to_embassy_duration(BACKPRESSURE_LOG_THRESHOLD)
    {
        hooks.on_queue_backpressure(QueueKind::RxEvt, embassy_to_core_duration(blocked));
    }
}

pub async fn run_actor<
    Node,
    RM,
    E: 'static,
    const MAX_PAYLOAD: usize,
    const TX_REQ_DEPTH: usize,
    const TX_RESP_DEPTH: usize,
    const RX_EVT_DEPTH: usize,
    H,
>(
    node: &'static mut Node,
    resources: &'static QueueResources<RM, E, MAX_PAYLOAD, TX_REQ_DEPTH, TX_RESP_DEPTH, RX_EVT_DEPTH>,
    cfg: ActorConfig,
    hooks: H,
) -> !
where
    RM: RawMutex + 'static,
    Node: IsoTpAsyncEndpoint<Error = E> + IsoTpAsyncEndpointRecvInto<Error = E>,
    H: ActorHooks<E>,
{
    hooks.on_actor_started();
    let tx_burst_limit = cfg.normalized_tx_burst_limit();
    let mut rx_buf = [0u8; MAX_PAYLOAD];

    loop {
        let mut tx_processed = 0;
        while tx_processed < tx_burst_limit {
            match resources.tx_req.try_receive() {
                Ok(req) => {
                    tx_processed += 1;
                    hooks.on_tx_request(req.to, req.is_functional, req.len, req.timeout);

                    let mapped = match if req.is_functional {
                        node.send_functional_to(req.to, &req.payload[..req.len], req.timeout)
                            .await
                    } else {
                        node.send_to(req.to, &req.payload[..req.len], req.timeout).await
                    } {
                        Ok(()) => Ok(()),
                        Err(SendError::Timeout) => Err(SendError::Timeout),
                        Err(SendError::Backend(err)) => {
                            Err(SendError::Backend(QueueIsoTpError::Transport(err)))
                        }
                    };
                    hooks.on_tx_result(&mapped);

                    let rsp = match mapped {
                        Ok(()) => IsoTpTxResponse::Ok,
                        Err(SendError::Timeout) => IsoTpTxResponse::Timeout,
                        Err(SendError::Backend(QueueIsoTpError::Transport(e))) => {
                            IsoTpTxResponse::Backend(e)
                        }
                        Err(SendError::Backend(QueueIsoTpError::PayloadTooLarge { .. })) => {
                            unreachable!("actor send path does not generate PayloadTooLarge")
                        }
                        Err(SendError::Backend(QueueIsoTpError::ChannelClosed)) => {
                            unreachable!("embassy channels are static and never close")
                        }
                    };

                    let send_started = embassy_time::Instant::now();
                    resources.tx_resp.send(rsp).await;
                    if let Some(blocked) =
                        embassy_time::Instant::now().checked_duration_since(send_started)
                        && blocked >= core_to_embassy_duration(BACKPRESSURE_LOG_THRESHOLD)
                    {
                        hooks.on_queue_backpressure(
                            QueueKind::TxResp,
                            embassy_to_core_duration(blocked),
                        );
                    }
                }
                Err(_) => break,
            }
        }

        match node.recv_one_into(cfg.recv_poll_timeout, &mut rx_buf).await {
            Ok(RecvMetaIntoStatus::TimedOut) => {
                yield_now().await;
            }
            Ok(RecvMetaIntoStatus::DeliveredOne { meta, len }) => {
                if let Some(expected_from) = cfg.allowed_reply_from
                    && meta.reply_to != expected_from
                {
                    hooks.on_rx_filtered_source(expected_from, meta.reply_to, len);
                    continue;
                }
                let from = cfg.fixed_reply_to.unwrap_or(meta.reply_to);
                hooks.on_rx_delivered(from, len);

                let mut payload = [0u8; MAX_PAYLOAD];
                payload[..len].copy_from_slice(&rx_buf[..len]);
                send_rx_event(
                    &hooks,
                    resources,
                    IsoTpRxEvent::Delivered { from, len, payload },
                )
                .await;
            }
            Err(RecvError::BufferTooSmall { needed, got }) => {
                hooks.on_rx_buffer_too_small(needed, got);
                send_rx_event(
                    &hooks,
                    resources,
                    IsoTpRxEvent::BufferTooSmall { needed, got },
                )
                .await;
            }
            Err(RecvError::Backend(err)) => {
                hooks.on_rx_backend_error(&err);
                send_rx_event(&hooks, resources, IsoTpRxEvent::Backend(err)).await;
            }
        }
    }
}

fn core_to_embassy_duration(dur: Duration) -> embassy_time::Duration {
    let micros = dur.as_micros().min(u128::from(u64::MAX)) as u64;
    embassy_time::Duration::from_micros(micros)
}

fn embassy_to_core_duration(dur: embassy_time::Duration) -> Duration {
    Duration::from_micros(dur.as_micros())
}

#[cfg(test)]
mod tests {
    use super::*;

    use embassy_sync_07::blocking_mutex::raw::CriticalSectionRawMutex;

    #[test]
    fn resources_construct_and_split() {
        static RES: QueueResources<CriticalSectionRawMutex, (), 64, 4, 1, 4> =
            QueueResources::new();
        let (_tx, _rx) = RES.split_endpoints();
    }
}
