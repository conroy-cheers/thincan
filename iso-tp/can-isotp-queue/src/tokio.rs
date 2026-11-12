//! Optional bounded Tokio transport owner with independent frontend handles.
pub use crate::common::{ActorError, ActorStop};
use crate::common::{
    BACKPRESSURE_REPORT_THRESHOLD, QueuedRequest, SendStop, classify_receive, dispatch,
};
use crate::{ActorConfig, ActorHooks, QueueError, RxEvent};
use can_isotp_interface::{
    Deadline, Destination, IsoTpAsyncRead, IsoTpAsyncReadSend, IsoTpAsyncWrite,
    IsoTpAsyncWriteSend, ReceiveError, ReceivedPayload, TransmitError, until,
};
use core::future::Future;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use tokio::sync::{mpsc, oneshot};

type Completion<E> = oneshot::Sender<Result<(), TransmitError<QueueError<E>>>>;
struct Request<E, D> {
    destination: Destination,
    payload: Vec<u8>,
    deadline: D,
    completion: Completion<E>,
}
type Event<E> = RxEvent<E, Vec<u8>>;

impl<E> QueuedRequest<E> for Completion<E> {
    fn is_abandoned(&self) -> bool {
        self.is_closed()
    }
    fn abandoned(&mut self) -> impl Future<Output = ()> {
        self.closed()
    }
    fn finish(self, result: Result<(), TransmitError<QueueError<E>>>) {
        let _ = self.send(result);
    }
}

/// Cloneable command sender. Receive ownership is a separate type.
pub struct QueueTx<E, D> {
    requests: mpsc::Sender<Request<E, D>>,
    closed: Arc<AtomicBool>,
    capacity: usize,
}
impl<E, D> Clone for QueueTx<E, D> {
    fn clone(&self) -> Self {
        Self {
            requests: self.requests.clone(),
            closed: self.closed.clone(),
            capacity: self.capacity,
        }
    }
}
/// Uniquely owned receive endpoint.
pub struct QueueRx<E> {
    events: mpsc::Receiver<Event<E>>,
    closed: Arc<AtomicBool>,
}
pub struct DriverPorts<RxE, TxE, D> {
    requests: mpsc::Receiver<Request<TxE, D>>,
    events: mpsc::Sender<Event<RxE>>,
    closed: CloseGuard,
    capacity: usize,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InvalidCapacity;

/// The separately owned application capabilities and actor ports.
pub type Endpoints<RxE, TxE, D> = (QueueTx<TxE, D>, QueueRx<RxE>, DriverPorts<RxE, TxE, D>);

pub fn make_endpoints<RxE, TxE, D>(
    capacity: usize,
    tx_depth: usize,
    rx_depth: usize,
) -> Result<Endpoints<RxE, TxE, D>, InvalidCapacity> {
    if capacity == 0 || tx_depth == 0 || rx_depth == 0 {
        return Err(InvalidCapacity);
    }
    let (requests, receiver) = mpsc::channel(tx_depth);
    let (events, input) = mpsc::channel(rx_depth);
    let closed = Arc::new(AtomicBool::new(false));
    Ok((
        QueueTx {
            requests,
            closed: closed.clone(),
            capacity,
        },
        QueueRx {
            events: input,
            closed: closed.clone(),
        },
        DriverPorts {
            requests: receiver,
            events,
            closed: CloseGuard(closed),
            capacity,
        },
    ))
}

impl<E: Send + 'static, D> IsoTpAsyncWriteSend for QueueTx<E, D>
where
    D: Deadline + Send + 'static,
    D::Wait: Send,
{
    type Error = QueueError<E>;
    type Deadline = D;
    async fn send(
        &mut self,
        destination: Destination,
        payload: &[u8],
        deadline: D,
    ) -> Result<(), TransmitError<Self::Error>> {
        if self.closed.load(Ordering::Acquire) {
            return Err(TransmitError::Closed);
        }
        if payload.len() > self.capacity {
            return Err(TransmitError::Rejected(QueueError::PayloadTooLarge {
                needed: payload.len(),
                capacity: self.capacity,
            }));
        }
        let propagated = deadline.clone();
        until(deadline, async {
            let permit = self
                .requests
                .reserve()
                .await
                .map_err(|_| TransmitError::Closed)?;
            if self.closed.load(Ordering::Acquire) {
                return Err(TransmitError::Closed);
            }
            if propagated.remaining().is_zero() {
                return Err(TransmitError::Timeout);
            }
            let (completion, result) = oneshot::channel();
            permit.send(Request {
                destination,
                payload: payload.to_vec(),
                deadline: propagated,
                completion,
            });
            result.await.unwrap_or(Err(TransmitError::Closed))
        })
        .await
        .unwrap_or(Err(TransmitError::Timeout))
    }
}
impl<E: Send + 'static> IsoTpAsyncReadSend for QueueRx<E> {
    type Error = E;
    async fn recv_into(&mut self, out: &mut [u8]) -> Result<ReceivedPayload, ReceiveError<E>> {
        if self.closed.load(Ordering::Acquire) {
            return Err(ReceiveError::Closed);
        }
        let event = self.events.recv().await.ok_or(ReceiveError::Closed)?;
        if self.closed.load(Ordering::Acquire) {
            return Err(ReceiveError::Closed);
        }
        event.deliver(out)
    }
}

struct CloseGuard(Arc<AtomicBool>);
impl Drop for CloseGuard {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

/// Why one half of the actor stopped.
enum HalfStop {
    /// The transport closed; the whole actor stops.
    Transport,
    /// The application dropped this half's endpoint; the other half keeps running.
    Endpoint,
}

/// Own both transport halves. Frontends may close independently; a fatal result
/// or shutdown ends both. Returning or dropping this future closes the queues.
pub async fn run_actor<R, W, H, C>(
    reader: R,
    writer: W,
    ports: DriverPorts<R::Error, W::Error, W::Deadline>,
    config: ActorConfig,
    hooks: H,
    shutdown: C,
) -> Result<ActorStop, ActorError<R::Error, W::Error>>
where
    R: IsoTpAsyncRead,
    W: IsoTpAsyncWrite,
    H: ActorHooks<R::Error, W::Error>,
    C: Future<Output = ()>,
{
    let _guard = ports.closed;
    hooks.on_actor_started();
    let burst = config.burst_limit.max(1);
    let receive =
        receive_loop::<R, W::Error, H>(reader, ports.events, ports.capacity, burst, &hooks);
    let send = send_loop::<W, R::Error, H>(writer, ports.requests, burst, &hooks);
    tokio::pin!(receive, send, shutdown);
    let (mut rx_live, mut tx_live) = (true, true);
    while rx_live || tx_live {
        tokio::select! {
            result = &mut receive, if rx_live => match result? {
                HalfStop::Transport => return Ok(ActorStop::Closed),
                HalfStop::Endpoint => rx_live = false,
            },
            result = &mut send, if tx_live => match result? {
                HalfStop::Transport => return Ok(ActorStop::Closed),
                HalfStop::Endpoint => tx_live = false,
            },
            () = &mut shutdown => return Ok(ActorStop::Cancelled),
        }
    }
    Ok(ActorStop::Closed)
}

async fn receive_loop<R, TxE, H>(
    mut reader: R,
    events: mpsc::Sender<Event<R::Error>>,
    capacity: usize,
    burst: usize,
    hooks: &H,
) -> Result<HalfStop, ActorError<R::Error, TxE>>
where
    R: IsoTpAsyncRead,
    H: ActorHooks<R::Error, TxE>,
{
    let mut buffer = vec![0; capacity];
    let mut processed = 0;
    loop {
        if processed == burst {
            tokio::task::yield_now().await;
            processed = 0;
        }
        processed += 1;
        let result = tokio::select! {
            biased;
            () = events.closed() => return Ok(HalfStop::Endpoint),
            result = reader.recv_into(&mut buffer) => result,
        };
        let Some(event) = classify_receive(result, capacity, hooks, |payload| {
            buffer[..payload.len].to_vec()
        })?
        else {
            return Ok(HalfStop::Transport);
        };
        let start = tokio::time::Instant::now();
        if events.send(event).await.is_err() {
            return Ok(HalfStop::Endpoint);
        }
        let blocked = start.elapsed();
        if blocked >= BACKPRESSURE_REPORT_THRESHOLD {
            hooks.on_rx_backpressure(blocked);
        }
    }
}
async fn send_loop<W, RxE, H>(
    mut writer: W,
    mut requests: mpsc::Receiver<Request<W::Error, W::Deadline>>,
    burst: usize,
    hooks: &H,
) -> Result<HalfStop, ActorError<RxE, W::Error>>
where
    W: IsoTpAsyncWrite,
    H: ActorHooks<RxE, W::Error>,
{
    let mut processed = 0;
    while let Some(request) = requests.recv().await {
        if processed == burst {
            tokio::task::yield_now().await;
            processed = 0;
        }
        processed += 1;
        let Request {
            destination,
            payload,
            deadline,
            completion,
        } = request;
        match dispatch(
            &mut writer,
            hooks,
            completion,
            destination,
            &payload,
            deadline,
        )
        .await
        {
            Ok(()) => {}
            Err(SendStop::Fatal(e)) => return Err(ActorError::Send(e)),
            Err(SendStop::Closed) => return Ok(HalfStop::Transport),
        }
    }
    Ok(HalfStop::Endpoint)
}

#[cfg(test)]
mod tests;
