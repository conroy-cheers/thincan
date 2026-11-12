//! Shared queue configuration and observation hooks.
#[cfg(any(feature = "tokio", feature = "embassy"))]
use can_isotp_interface::{Deadline, IsoTpAsyncWrite, ReceiveError, until};
use can_isotp_interface::{Destination, ReceivedPayload, TransmitError};
use core::time::Duration;
#[cfg(any(feature = "tokio", feature = "embassy"))]
use core::{future::Future, pin::pin, task::Poll};

#[derive(Debug)]
#[non_exhaustive]
pub enum QueueError<E> {
    PayloadTooLarge { needed: usize, capacity: usize },
    Backend(E),
    WaitersFull,
}
impl<E: core::fmt::Display> core::fmt::Display for QueueError<E> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::PayloadTooLarge { needed, capacity } => {
                write!(f, "payload length {needed} exceeds capacity {capacity}")
            }
            Self::WaitersFull => f.write_str("request admission waiter capacity exhausted"),
            Self::Backend(e) => e.fmt(f),
        }
    }
}
impl<E: core::error::Error + 'static> core::error::Error for QueueError<E> {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::Backend(e) => Some(e),
            _ => None,
        }
    }
}
#[derive(Debug, Clone, Copy)]
pub struct ActorConfig {
    pub burst_limit: usize,
}
impl Default for ActorConfig {
    fn default() -> Self {
        Self { burst_limit: 32 }
    }
}
pub trait ActorHooks<RxE, TxE> {
    fn on_actor_started(&self) {}
    fn on_tx_request(&self, _destination: Destination, _len: usize, _remaining: Duration) {}
    fn on_tx_result(&self, _result: &Result<(), TransmitError<TxE>>) {}
    fn on_rx_delivered(&self, _from: u8, _len: usize) {}
    fn on_rx_buffer_too_small(&self, _needed: usize, _got: usize) {}
    fn on_rx_rejected(&self, _error: &RxE) {}
    fn on_rx_fatal(&self, _error: &RxE) {}
    /// The actor waited `blocked` for room in the receive queue.
    fn on_rx_backpressure(&self, _blocked: Duration) {}
}
/// Shortest receive-queue wait reported through [`ActorHooks::on_rx_backpressure`].
#[cfg(any(feature = "tokio", feature = "embassy"))]
pub(crate) const BACKPRESSURE_REPORT_THRESHOLD: Duration = Duration::from_millis(1);

/// Measures elapsed time on a deadline's own clock, so timing needs no time
/// driver beyond the one the writer's deadlines already use.
#[cfg(feature = "embassy")]
pub(crate) struct Stopwatch<D>(Option<D>);

#[cfg(feature = "embassy")]
impl<D: can_isotp_interface::Deadline> Stopwatch<D> {
    /// Longest interval measured; longer ones read as this.
    const SPAN: Duration = Duration::from_secs(3600);

    pub(crate) fn start() -> Self {
        Self(D::after(Self::SPAN))
    }

    /// Time since `start`, or zero if the clock could not represent the span.
    pub(crate) fn elapsed(&self) -> Duration {
        self.0.as_ref().map_or(Duration::ZERO, |end| {
            Self::SPAN.saturating_sub(end.remaining())
        })
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct NoopHooks;
impl<RxE, TxE> ActorHooks<RxE, TxE> for NoopHooks {}

/// Why an actor stopped without a fatal transport error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ActorStop {
    /// The shutdown future completed.
    Cancelled,
    /// The transport or the application endpoints closed.
    Closed,
}
#[derive(Debug)]
#[non_exhaustive]
pub enum ActorError<RxE, TxE> {
    Receive(RxE),
    Send(TxE),
    InvalidPayloadLength { reported: usize, capacity: usize },
}
impl<R: core::fmt::Display, W: core::fmt::Display> core::fmt::Display for ActorError<R, W> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Receive(e) => write!(f, "receive failed: {e}"),
            Self::Send(e) => write!(f, "send failed: {e}"),
            Self::InvalidPayloadLength { reported, capacity } => write!(
                f,
                "reader reported {reported} bytes for {capacity}-byte buffer"
            ),
        }
    }
}
impl<R: core::error::Error + 'static, W: core::error::Error + 'static> core::error::Error
    for ActorError<R, W>
{
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::Receive(e) => Some(e),
            Self::Send(e) => Some(e),
            Self::InvalidPayloadLength { .. } => None,
        }
    }
}

/// One receive outcome queued for the application, holding payload bytes in
/// `B`. Fatal errors and closure end the actor instead of being queued.
pub enum RxEvent<E, B> {
    Payload(ReceivedPayload, B),
    BufferTooSmall { needed: usize, got: usize },
    Rejected(E),
}

#[cfg(any(feature = "tokio", feature = "embassy"))]
impl<E, B: AsRef<[u8]>> RxEvent<E, B> {
    /// Deliver into `out` as `recv_into` reports it.
    pub(crate) fn deliver(self, out: &mut [u8]) -> Result<ReceivedPayload, ReceiveError<E>> {
        match self {
            Self::Payload(payload, bytes) => {
                let bytes = &bytes.as_ref()[..payload.len];
                if bytes.len() > out.len() {
                    return Err(ReceiveError::BufferTooSmall {
                        needed: bytes.len(),
                        got: out.len(),
                    });
                }
                out[..bytes.len()].copy_from_slice(bytes);
                Ok(payload)
            }
            Self::BufferTooSmall { needed, got } => {
                Err(ReceiveError::BufferTooSmall { needed, got })
            }
            Self::Rejected(e) => Err(ReceiveError::Rejected(e)),
        }
    }
}

#[cfg(any(feature = "tokio", feature = "embassy"))]
/// Validate a reader result against the actor buffer, report it to `hooks`,
/// and turn it into the event to queue, with payload bytes from `bytes`.
/// `Ok(None)` means the transport closed; fatal errors become actor errors.
pub(crate) fn classify_receive<RxE, TxE, H: ActorHooks<RxE, TxE>, B>(
    result: Result<ReceivedPayload, ReceiveError<RxE>>,
    capacity: usize,
    hooks: &H,
    bytes: impl FnOnce(&ReceivedPayload) -> B,
) -> Result<Option<RxEvent<RxE, B>>, ActorError<RxE, TxE>> {
    match result {
        Ok(payload) if payload.len > capacity => Err(ActorError::InvalidPayloadLength {
            reported: payload.len,
            capacity,
        }),
        Ok(payload) => {
            hooks.on_rx_delivered(payload.meta.reply_to, payload.len);
            let bytes = bytes(&payload);
            Ok(Some(RxEvent::Payload(payload, bytes)))
        }
        Err(ReceiveError::BufferTooSmall { needed, got }) => {
            hooks.on_rx_buffer_too_small(needed, got);
            Ok(Some(RxEvent::BufferTooSmall { needed, got }))
        }
        Err(ReceiveError::Rejected(e)) => {
            hooks.on_rx_rejected(&e);
            Ok(Some(RxEvent::Rejected(e)))
        }
        Err(ReceiveError::Fatal(e)) => {
            hooks.on_rx_fatal(&e);
            Err(ActorError::Receive(e))
        }
        // `Closed`, or a kind added later: stop forwarding.
        Err(_) => Ok(None),
    }
}

#[cfg(any(feature = "tokio", feature = "embassy"))]
/// Send through `writer`, failing with `Timeout` once `deadline` passes.
/// `until` checks expiry before the first poll, so an expired request never
/// reaches the writer.
pub(crate) async fn send_until<W: IsoTpAsyncWrite>(
    writer: &mut W,
    destination: Destination,
    payload: &[u8],
    deadline: W::Deadline,
) -> Result<(), TransmitError<W::Error>> {
    until(
        deadline.clone(),
        writer.send(destination, payload, deadline),
    )
    .await
    .unwrap_or(Err(TransmitError::Timeout))
}

#[cfg(any(feature = "tokio", feature = "embassy"))]
/// A dequeued send request's link back to its caller.
pub(crate) trait QueuedRequest<E> {
    /// The caller gave up on the request, or the queue closed.
    fn is_abandoned(&self) -> bool;
    /// Resolves once [`Self::is_abandoned`] holds.
    fn abandoned(&mut self) -> impl Future<Output = ()>;
    /// Report the outcome to the caller, if it is still waiting.
    fn finish(self, result: Result<(), TransmitError<QueueError<E>>>);
}

#[cfg(any(feature = "tokio", feature = "embassy"))]
/// Why the actor's send half must stop.
pub(crate) enum SendStop<E> {
    Fatal(E),
    Closed,
}

#[cfg(any(feature = "tokio", feature = "embassy"))]
/// Send one dequeued request and report it, with the same rules on every
/// runtime: an expired request is reported as `Timeout` without being sent, an
/// abandoned one is dropped, and expiry during the send is a `Timeout` even if
/// the caller gave up first on the same deadline.
pub(crate) async fn dispatch<W, RxE, H, Q>(
    writer: &mut W,
    hooks: &H,
    request: Q,
    destination: Destination,
    payload: &[u8],
    deadline: W::Deadline,
) -> Result<(), SendStop<W::Error>>
where
    W: IsoTpAsyncWrite,
    H: ActorHooks<RxE, W::Error>,
    Q: QueuedRequest<W::Error>,
{
    let mut request = request;
    if deadline.remaining().is_zero() {
        hooks.on_tx_result(&Err(TransmitError::Timeout));
        request.finish(Err(TransmitError::Timeout));
        return Ok(());
    }
    if request.is_abandoned() {
        return Ok(());
    }
    hooks.on_tx_request(destination, payload.len(), deadline.remaining());
    let expiry = deadline.clone();
    let send = send_until(writer, destination, payload, deadline);
    let result = match select_biased(request.abandoned(), send).await {
        Either::First(()) if expiry.remaining().is_zero() => Err(TransmitError::Timeout),
        Either::First(()) => return Ok(()),
        Either::Second(result) => result,
    };
    hooks.on_tx_result(&result);
    match result {
        Err(TransmitError::Fatal(e)) => {
            request.finish(Err(TransmitError::Closed));
            Err(SendStop::Fatal(e))
        }
        Err(TransmitError::Closed) => {
            request.finish(Err(TransmitError::Closed));
            Err(SendStop::Closed)
        }
        result => {
            request.finish(result.map_err(|e| e.map(QueueError::Backend)));
            Ok(())
        }
    }
}

#[cfg(any(feature = "tokio", feature = "embassy"))]
enum Either<A, B> {
    First(A),
    Second(B),
}

#[cfg(any(feature = "tokio", feature = "embassy"))]
/// Wait for either future, preferring `first` when both are ready.
async fn select_biased<A: Future, B: Future>(first: A, second: B) -> Either<A::Output, B::Output> {
    let mut first = pin!(first);
    let mut second = pin!(second);
    core::future::poll_fn(|cx| {
        if let Poll::Ready(out) = first.as_mut().poll(cx) {
            return Poll::Ready(Either::First(out));
        }
        second.as_mut().poll(cx).map(Either::Second)
    })
    .await
}
