use core::time::Duration;
use std::sync::Arc;
use std::vec::Vec;

use can_isotp_interface::{
    IsoTpAsyncEndpoint, IsoTpAsyncEndpointRecvInto, RecvControl, RecvError, RecvMeta,
    RecvMetaIntoStatus, RecvStatus, SendError,
};
use tokio::sync::{Mutex, mpsc, oneshot};

use crate::common::{ActorConfig, ActorHooks, QueueIsoTpError, QueueKind};

const BACKPRESSURE_LOG_THRESHOLD: Duration = Duration::from_millis(1);

#[derive(Debug)]
struct IsoTpTxRequest<E> {
    to: u8,
    functional_to: bool,
    timeout: Duration,
    payload: Vec<u8>,
    done: oneshot::Sender<Result<(), SendError<QueueIsoTpError<E>>>>,
}

#[derive(Debug)]
enum IsoTpRxEvent<E> {
    Delivered { from: u8, payload: Vec<u8> },
    BufferTooSmall { needed: usize, got: usize },
    Backend(E),
}

#[derive(Debug)]
struct QueueIsoTpInner<E> {
    max_payload: usize,
    tx_req: mpsc::Sender<IsoTpTxRequest<E>>,
    rx_evt: Mutex<mpsc::Receiver<IsoTpRxEvent<E>>>,
}

pub struct QueueIsoTpEndpoint<E> {
    inner: Arc<QueueIsoTpInner<E>>,
}

impl<E> Clone for QueueIsoTpEndpoint<E> {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

impl<E> QueueIsoTpEndpoint<E> {
    fn new(
        max_payload: usize,
        tx_req: mpsc::Sender<IsoTpTxRequest<E>>,
        rx_evt: mpsc::Receiver<IsoTpRxEvent<E>>,
    ) -> Self {
        Self {
            inner: Arc::new(QueueIsoTpInner {
                max_payload,
                tx_req,
                rx_evt: Mutex::new(rx_evt),
            }),
        }
    }
}

pub struct ActorPorts<E> {
    tx_req_rx: mpsc::Receiver<IsoTpTxRequest<E>>,
    rx_evt_tx: mpsc::Sender<IsoTpRxEvent<E>>,
}

pub fn make_endpoints<E: Send + 'static>(
    max_payload: usize,
    tx_depth: usize,
    rx_depth: usize,
) -> (QueueIsoTpEndpoint<E>, QueueIsoTpEndpoint<E>, ActorPorts<E>) {
    let tx_depth = tx_depth.max(1);
    let rx_depth = rx_depth.max(1);

    let (tx_req_tx, tx_req_rx) = mpsc::channel(tx_depth);
    let (rx_evt_tx, rx_evt_rx) = mpsc::channel(rx_depth);
    let tx_node = QueueIsoTpEndpoint::new(max_payload, tx_req_tx.clone(), rx_evt_rx);
    let rx_node = tx_node.clone();
    let ports = ActorPorts { tx_req_rx, rx_evt_tx };
    (tx_node, rx_node, ports)
}

impl<E: Send + 'static> IsoTpAsyncEndpoint for QueueIsoTpEndpoint<E> {
    type Error = QueueIsoTpError<E>;

    async fn send_to(
        &mut self,
        to: u8,
        payload: &[u8],
        timeout: Duration,
    ) -> Result<(), SendError<Self::Error>> {
        if payload.len() > self.inner.max_payload {
            return Err(SendError::Backend(QueueIsoTpError::PayloadTooLarge {
                needed: payload.len(),
                capacity: self.inner.max_payload,
            }));
        }

        let (done_tx, done_rx) = oneshot::channel();
        let req = IsoTpTxRequest {
            to,
            functional_to: false,
            timeout,
            payload: payload.to_vec(),
            done: done_tx,
        };

        self.inner
            .tx_req
            .send(req)
            .await
            .map_err(|_| SendError::Backend(QueueIsoTpError::ChannelClosed))?;

        done_rx
            .await
            .map_err(|_| SendError::Backend(QueueIsoTpError::ChannelClosed))?
    }

    async fn send_functional_to(
        &mut self,
        functional_to: u8,
        payload: &[u8],
        timeout: Duration,
    ) -> Result<(), SendError<Self::Error>> {
        if payload.len() > self.inner.max_payload {
            return Err(SendError::Backend(QueueIsoTpError::PayloadTooLarge {
                needed: payload.len(),
                capacity: self.inner.max_payload,
            }));
        }

        let (done_tx, done_rx) = oneshot::channel();
        let req = IsoTpTxRequest {
            to: functional_to,
            functional_to: true,
            timeout,
            payload: payload.to_vec(),
            done: done_tx,
        };

        self.inner
            .tx_req
            .send(req)
            .await
            .map_err(|_| SendError::Backend(QueueIsoTpError::ChannelClosed))?;

        done_rx
            .await
            .map_err(|_| SendError::Backend(QueueIsoTpError::ChannelClosed))?
    }

    async fn recv_one<Cb>(
        &mut self,
        timeout: Duration,
        mut on_payload: Cb,
    ) -> Result<RecvStatus, RecvError<Self::Error>>
    where
        Cb: FnMut(RecvMeta, &[u8]) -> Result<RecvControl, Self::Error>,
    {
        let mut rx = self.inner.rx_evt.lock().await;
        match tokio::time::timeout(timeout, rx.recv()).await {
            Ok(Some(IsoTpRxEvent::Delivered { from, payload })) => {
                let _ = on_payload(RecvMeta { reply_to: from }, &payload)
                    .map_err(RecvError::Backend)?;
                Ok(RecvStatus::DeliveredOne)
            }
            Ok(Some(IsoTpRxEvent::BufferTooSmall { needed, got })) => {
                Err(RecvError::BufferTooSmall { needed, got })
            }
            Ok(Some(IsoTpRxEvent::Backend(err))) => {
                Err(RecvError::Backend(QueueIsoTpError::Transport(err)))
            }
            Ok(None) => Err(RecvError::Backend(QueueIsoTpError::ChannelClosed)),
            Err(_) => Ok(RecvStatus::TimedOut),
        }
    }
}

impl<E: Send + 'static> IsoTpAsyncEndpointRecvInto for QueueIsoTpEndpoint<E> {
    type Error = QueueIsoTpError<E>;

    async fn recv_one_into(
        &mut self,
        timeout: Duration,
        out: &mut [u8],
    ) -> Result<RecvMetaIntoStatus, RecvError<Self::Error>> {
        let mut rx = self.inner.rx_evt.lock().await;
        match tokio::time::timeout(timeout, rx.recv()).await {
            Ok(Some(IsoTpRxEvent::Delivered { from, payload })) => {
                if out.len() < payload.len() {
                    return Err(RecvError::BufferTooSmall {
                        needed: payload.len(),
                        got: out.len(),
                    });
                }
                out[..payload.len()].copy_from_slice(&payload);
                Ok(RecvMetaIntoStatus::DeliveredOne {
                    meta: RecvMeta { reply_to: from },
                    len: payload.len(),
                })
            }
            Ok(Some(IsoTpRxEvent::BufferTooSmall { needed, got })) => {
                Err(RecvError::BufferTooSmall { needed, got })
            }
            Ok(Some(IsoTpRxEvent::Backend(err))) => {
                Err(RecvError::Backend(QueueIsoTpError::Transport(err)))
            }
            Ok(None) => Err(RecvError::Backend(QueueIsoTpError::ChannelClosed)),
            Err(_) => Ok(RecvMetaIntoStatus::TimedOut),
        }
    }
}

async fn send_rx_event<E, H: ActorHooks<E>>(
    hooks: &H,
    tx: &mpsc::Sender<IsoTpRxEvent<E>>,
    evt: IsoTpRxEvent<E>,
) {
    let started = tokio::time::Instant::now();
    if tx.send(evt).await.is_ok() {
        let blocked_for = started.elapsed();
        if blocked_for >= BACKPRESSURE_LOG_THRESHOLD {
            hooks.on_queue_backpressure(QueueKind::RxEvt, blocked_for);
        }
    }
}

pub async fn run_actor<Node, E, H>(
    mut node: Node,
    mut ports: ActorPorts<E>,
    cfg: ActorConfig,
    hooks: H,
) -> !
where
    Node: IsoTpAsyncEndpoint<Error = E>,
    H: ActorHooks<E>,
{
    hooks.on_actor_started();
    let tx_burst_limit = cfg.normalized_tx_burst_limit();

    loop {
        let mut tx_processed = 0;
        while tx_processed < tx_burst_limit {
            match ports.tx_req_rx.try_recv() {
                Ok(req) => {
                    tx_processed += 1;
                    hooks.on_tx_request(req.to, req.functional_to, req.payload.len(), req.timeout);

                    let mapped = match if req.functional_to {
                        node.send_functional_to(req.to, &req.payload, req.timeout).await
                    } else {
                        node.send_to(req.to, &req.payload, req.timeout).await
                    } {
                        Ok(()) => Ok(()),
                        Err(SendError::Timeout) => Err(SendError::Timeout),
                        Err(SendError::Backend(err)) => {
                            Err(SendError::Backend(QueueIsoTpError::Transport(err)))
                        }
                    };

                    hooks.on_tx_result(&mapped);
                    let _ = req.done.send(mapped);
                }
                Err(mpsc::error::TryRecvError::Empty) => break,
                Err(mpsc::error::TryRecvError::Disconnected) => break,
            }
        }

        let mut delivered: Option<(u8, Vec<u8>)> = None;
        match node
            .recv_one(cfg.recv_poll_timeout, |meta, payload| {
                delivered = Some((meta.reply_to, payload.to_vec()));
                Ok(RecvControl::Stop)
            })
            .await
        {
            Ok(RecvStatus::TimedOut) => {
                tokio::task::yield_now().await;
            }
            Ok(RecvStatus::DeliveredOne) => {
                if let Some((from_meta, payload)) = delivered {
                    if let Some(expected_from) = cfg.allowed_reply_from
                        && from_meta != expected_from
                    {
                        hooks.on_rx_filtered_source(expected_from, from_meta, payload.len());
                        continue;
                    }
                    let from = cfg.fixed_reply_to.unwrap_or(from_meta);
                    hooks.on_rx_delivered(from, payload.len());
                    send_rx_event(&hooks, &ports.rx_evt_tx, IsoTpRxEvent::Delivered { from, payload })
                        .await;
                }
            }
            Err(RecvError::BufferTooSmall { needed, got }) => {
                hooks.on_rx_buffer_too_small(needed, got);
                send_rx_event(
                    &hooks,
                    &ports.rx_evt_tx,
                    IsoTpRxEvent::BufferTooSmall { needed, got },
                )
                .await;
            }
            Err(RecvError::Backend(err)) => {
                hooks.on_rx_backend_error(&err);
                send_rx_event(&hooks, &ports.rx_evt_tx, IsoTpRxEvent::Backend(err)).await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::collections::VecDeque;

    #[derive(Debug, Clone, PartialEq, Eq)]
    enum FakeError {
        Backend(u8),
    }

    #[derive(Debug)]
    enum SendAction {
        Ok,
        Timeout,
        Backend(FakeError),
    }

    #[derive(Debug)]
    enum RecvAction {
        TimedOut,
        Delivered { reply_to: u8, payload: Vec<u8> },
        Backend(FakeError),
    }

    #[derive(Debug)]
    struct FakeNode {
        send_actions: VecDeque<SendAction>,
        recv_actions: VecDeque<RecvAction>,
    }

    impl FakeNode {
        fn new(send_actions: Vec<SendAction>, recv_actions: Vec<RecvAction>) -> Self {
            Self {
                send_actions: send_actions.into(),
                recv_actions: recv_actions.into(),
            }
        }
    }

    impl IsoTpAsyncEndpoint for FakeNode {
        type Error = FakeError;

        async fn send_to(
            &mut self,
            _to: u8,
            _payload: &[u8],
            _timeout: Duration,
        ) -> Result<(), SendError<Self::Error>> {
            match self.send_actions.pop_front().unwrap_or(SendAction::Ok) {
                SendAction::Ok => Ok(()),
                SendAction::Timeout => Err(SendError::Timeout),
                SendAction::Backend(err) => Err(SendError::Backend(err)),
            }
        }

        async fn send_functional_to(
            &mut self,
            _functional_to: u8,
            _payload: &[u8],
            _timeout: Duration,
        ) -> Result<(), SendError<Self::Error>> {
            match self.send_actions.pop_front().unwrap_or(SendAction::Ok) {
                SendAction::Ok => Ok(()),
                SendAction::Timeout => Err(SendError::Timeout),
                SendAction::Backend(err) => Err(SendError::Backend(err)),
            }
        }

        async fn recv_one<Cb>(
            &mut self,
            _timeout: Duration,
            mut on_payload: Cb,
        ) -> Result<RecvStatus, RecvError<Self::Error>>
        where
            Cb: FnMut(RecvMeta, &[u8]) -> Result<RecvControl, Self::Error>,
        {
            match self.recv_actions.pop_front().unwrap_or(RecvAction::TimedOut) {
                RecvAction::TimedOut => Ok(RecvStatus::TimedOut),
                RecvAction::Delivered { reply_to, payload } => {
                    let _ = on_payload(RecvMeta { reply_to }, &payload).map_err(RecvError::Backend)?;
                    Ok(RecvStatus::DeliveredOne)
                }
                RecvAction::Backend(err) => Err(RecvError::Backend(err)),
            }
        }
    }

    #[tokio::test]
    async fn send_maps_success() {
        let (mut tx, _rx, ports) = make_endpoints::<FakeError>(64, 8, 8);
        let node = FakeNode::new(vec![SendAction::Ok], vec![]);
        let task = tokio::spawn(run_actor(node, ports, ActorConfig::default(), crate::NoopHooks));

        let res = tx.send_to(0x22, &[1, 2, 3], Duration::from_millis(20)).await;
        assert!(res.is_ok());

        task.abort();
    }

    #[tokio::test]
    async fn send_maps_timeout() {
        let (mut tx, _rx, ports) = make_endpoints::<FakeError>(64, 8, 8);
        let node = FakeNode::new(vec![SendAction::Timeout], vec![]);
        let task = tokio::spawn(run_actor(node, ports, ActorConfig::default(), crate::NoopHooks));

        let res = tx.send_to(0x22, &[1, 2, 3], Duration::from_millis(20)).await;
        assert!(matches!(res, Err(SendError::Timeout)));

        task.abort();
    }

    #[tokio::test]
    async fn send_maps_backend_error() {
        let (mut tx, _rx, ports) = make_endpoints::<FakeError>(64, 8, 8);
        let node = FakeNode::new(vec![SendAction::Backend(FakeError::Backend(7))], vec![]);
        let task = tokio::spawn(run_actor(node, ports, ActorConfig::default(), crate::NoopHooks));

        let res = tx.send_to(0x22, &[1, 2, 3], Duration::from_millis(20)).await;
        assert!(matches!(
            res,
            Err(SendError::Backend(QueueIsoTpError::Transport(FakeError::Backend(7))))
        ));

        task.abort();
    }

    #[tokio::test]
    async fn send_rejects_payload_too_large() {
        let (mut tx, _rx, _ports) = make_endpoints::<FakeError>(2, 8, 8);

        let res = tx.send_to(0x22, &[1, 2, 3], Duration::from_millis(20)).await;
        assert!(matches!(
            res,
            Err(SendError::Backend(QueueIsoTpError::PayloadTooLarge {
                needed: 3,
                capacity: 2
            }))
        ));
    }

    #[tokio::test]
    async fn recv_into_delivers_with_fixed_reply_to_override() {
        let (mut _tx, mut rx, ports) = make_endpoints::<FakeError>(64, 8, 8);
        let node = FakeNode::new(
            vec![],
            vec![RecvAction::Delivered {
                reply_to: 0x11,
                payload: vec![0xAA, 0xBB, 0xCC],
            }],
        );
        let cfg = ActorConfig::default().with_fixed_reply_to(Some(0x77));
        let task = tokio::spawn(run_actor(node, ports, cfg, crate::NoopHooks));

        let mut out = [0u8; 16];
        let recv = rx.recv_one_into(Duration::from_millis(100), &mut out).await;
        match recv {
            Ok(RecvMetaIntoStatus::DeliveredOne { meta, len }) => {
                assert_eq!(meta.reply_to, 0x77);
                assert_eq!(len, 3);
                assert_eq!(&out[..3], &[0xAA, 0xBB, 0xCC]);
            }
            _ => panic!("unexpected recv result: {recv:?}"),
        }

        task.abort();
    }

    #[tokio::test]
    async fn recv_drops_disallowed_source() {
        let (mut _tx, mut rx, ports) = make_endpoints::<FakeError>(64, 8, 8);
        let node = FakeNode::new(
            vec![],
            vec![RecvAction::Delivered {
                reply_to: 0x11,
                payload: vec![0xAA, 0xBB, 0xCC],
            }],
        );
        let cfg = ActorConfig::default().with_allowed_reply_from(Some(0x77));
        let task = tokio::spawn(run_actor(node, ports, cfg, crate::NoopHooks));

        let mut out = [0u8; 16];
        let recv = rx.recv_one_into(Duration::from_millis(100), &mut out).await;
        assert!(matches!(recv, Ok(RecvMetaIntoStatus::TimedOut)));

        task.abort();
    }

    #[tokio::test]
    async fn recv_into_returns_buffer_too_small() {
        let (mut _tx, mut rx, ports) = make_endpoints::<FakeError>(64, 8, 8);
        let node = FakeNode::new(
            vec![],
            vec![RecvAction::Delivered {
                reply_to: 0x11,
                payload: vec![0xAA, 0xBB, 0xCC],
            }],
        );
        let task = tokio::spawn(run_actor(node, ports, ActorConfig::default(), crate::NoopHooks));

        let mut out = [0u8; 2];
        let recv = rx.recv_one_into(Duration::from_millis(100), &mut out).await;
        assert!(matches!(
            recv,
            Err(RecvError::BufferTooSmall { needed: 3, got: 2 })
        ));

        task.abort();
    }

    #[tokio::test]
    async fn recv_maps_backend_error() {
        let (mut _tx, mut rx, ports) = make_endpoints::<FakeError>(64, 8, 8);
        let node = FakeNode::new(vec![], vec![RecvAction::Backend(FakeError::Backend(9))]);
        let task = tokio::spawn(run_actor(node, ports, ActorConfig::default(), crate::NoopHooks));

        let mut out = [0u8; 8];
        let recv = rx.recv_one_into(Duration::from_millis(100), &mut out).await;
        assert!(matches!(
            recv,
            Err(RecvError::Backend(QueueIsoTpError::Transport(FakeError::Backend(9))))
        ));

        task.abort();
    }

    #[tokio::test]
    async fn send_returns_channel_closed_after_actor_stops() {
        let (mut tx, _rx, ports) = make_endpoints::<FakeError>(64, 8, 8);
        let node = FakeNode::new(vec![], vec![]);
        let task = tokio::spawn(run_actor(node, ports, ActorConfig::default(), crate::NoopHooks));
        task.abort();
        let _ = task.await;

        let res = tx.send_to(0x22, &[1], Duration::from_millis(20)).await;
        assert!(matches!(
            res,
            Err(SendError::Backend(QueueIsoTpError::ChannelClosed))
        ));
    }

    #[tokio::test]
    async fn fairness_delivers_rx_with_tx_backlog() {
        let (mut tx, mut rx, ports) = make_endpoints::<FakeError>(64, 64, 8);
        let send_actions = (0..32).map(|_| SendAction::Ok).collect::<Vec<_>>();
        let recv_actions = vec![RecvAction::Delivered {
            reply_to: 0x44,
            payload: vec![0xFE],
        }];
        let node = FakeNode::new(send_actions, recv_actions);
        let cfg = ActorConfig::new(Duration::from_millis(1), 1, None);
        let task = tokio::spawn(run_actor(node, ports, cfg, crate::NoopHooks));

        let sender = tokio::spawn(async move {
            for _ in 0..16 {
                let _ = tx.send_to(0x22, &[1, 2, 3], Duration::from_millis(20)).await;
            }
        });

        let mut out = [0u8; 8];
        let recv = rx.recv_one_into(Duration::from_millis(200), &mut out).await;
        match recv {
            Ok(RecvMetaIntoStatus::DeliveredOne { meta, len }) => {
                assert_eq!(meta.reply_to, 0x44);
                assert_eq!(len, 1);
                assert_eq!(out[0], 0xFE);
            }
            _ => panic!("unexpected recv result: {recv:?}"),
        }

        let _ = sender.await;
        task.abort();
    }
}
