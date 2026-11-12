//! Fixed-storage application ports with generation-tagged completion leases.
pub use crate::common::{ActorError, ActorStop};
use crate::common::{
    BACKPRESSURE_REPORT_THRESHOLD, QueuedRequest, SendStop, Stopwatch, classify_receive, dispatch,
};
use crate::{ActorConfig, ActorHooks, QueueError};
use can_isotp_interface::{
    Deadline, Destination, IsoTpAsyncRead, IsoTpAsyncWrite, ReceiveError, ReceivedPayload,
    TransmitError, until,
};
use core::{
    cell::{Cell, RefCell},
    future::Future,
};
use embassy_futures::select::{Either, Either3, select, select3};
use embassy_sync::{
    blocking_mutex::{Mutex, raw::RawMutex},
    channel::Channel,
    semaphore::{FairSemaphore, Semaphore},
    signal::Signal,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RequestId {
    slot: usize,
    generation: u64,
}
struct Slot<E, D, const MAX: usize> {
    generation: u64,
    /// The caller still waits on (or holds a lease for) this request.
    caller: bool,
    /// The request is queued for, or leased by, the driver.
    driver: bool,
    accepted: bool,
    destination: Destination,
    payload: [u8; MAX],
    len: usize,
    deadline: Option<D>,
    result: Option<Result<(), TransmitError<QueueError<E>>>>,
}
impl<E, D, const MAX: usize> Slot<E, D, MAX> {
    const fn new() -> Self {
        Self {
            generation: 0,
            caller: false,
            driver: false,
            accepted: false,
            destination: Destination::Physical(0),
            payload: [0; MAX],
            len: 0,
            deadline: None,
            result: None,
        }
    }
    /// A slot is free once neither the caller nor the driver holds it.
    fn is_free(&self) -> bool {
        !self.caller && !self.driver
    }
    fn holds(&self, id: RequestId) -> bool {
        !self.is_free() && self.generation == id.generation
    }
}
/// Session flags, kept in one cell so transitions are observed together.
#[derive(Clone, Copy)]
struct State {
    claimed: bool,
    tx_handles: usize,
    rx_alive: bool,
    closed: bool,
}
/// Bounded payload events. Fatal reasons are driver results, never queued here.
pub type RxEvent<E, const MAX: usize> = crate::common::RxEvent<E, [u8; MAX]>;

pub struct QueueResources<
    RM: RawMutex,
    RxE,
    TxE,
    D,
    const MAX: usize,
    const N: usize,
    const RX: usize,
> {
    slots: Mutex<RM, RefCell<[Slot<TxE, D, MAX>; N]>>,
    permits: FairSemaphore<RM, N>,
    requests: Channel<RM, RequestId, N>,
    completions: [Signal<RM, ()>; N],
    cancellations: [Signal<RM, ()>; N],
    state: Mutex<RM, Cell<State>>,
    rx: Channel<RM, RxEvent<RxE, MAX>, RX>,
    rx_closed: Signal<RM, ()>,
    request_closed: Signal<RM, ()>,
    event_closed: Signal<RM, ()>,
    control_closed: Signal<RM, ()>,
}
/// Separately owned application capabilities and driver ports.
pub type Endpoints<'a, RM, RxE, TxE, D, const MAX: usize, const N: usize, const RX: usize> = (
    QueueTx<'a, RM, RxE, TxE, D, MAX, N, RX>,
    QueueRx<'a, RM, RxE, TxE, D, MAX, N, RX>,
    DriverPorts<'a, RM, RxE, TxE, D, MAX, N, RX>,
);
impl<RM: RawMutex, RxE, TxE, D, const MAX: usize, const N: usize, const RX: usize>
    QueueResources<RM, RxE, TxE, D, MAX, N, RX>
{
    pub const fn new() -> Self {
        assert!(MAX > 0 && N > 0 && RX > 0);
        Self {
            slots: Mutex::new(RefCell::new([const { Slot::new() }; N])),
            permits: FairSemaphore::new(N),
            requests: Channel::new(),
            completions: [const { Signal::new() }; N],
            cancellations: [const { Signal::new() }; N],
            state: Mutex::new(Cell::new(State {
                claimed: false,
                tx_handles: 1,
                rx_alive: true,
                closed: false,
            })),
            rx: Channel::new(),
            rx_closed: Signal::new(),
            request_closed: Signal::new(),
            event_closed: Signal::new(),
            control_closed: Signal::new(),
        }
    }
    fn update<T>(&self, f: impl FnOnce(&mut State) -> T) -> T {
        self.state.lock(|cell| {
            let mut state = cell.get();
            let out = f(&mut state);
            cell.set(state);
            out
        })
    }
    pub fn split(&self) -> Endpoints<'_, RM, RxE, TxE, D, MAX, N, RX> {
        assert!(
            !self.update(|s| core::mem::replace(&mut s.claimed, true)),
            "resources may only be split once per session"
        );
        (
            QueueTx { resources: self },
            QueueRx { resources: self },
            DriverPorts { resources: self },
        )
    }
    /// Start a fresh session after every old handle and lease has ended.
    /// Exclusive access prevents late completions from touching the new storage.
    pub fn reset(&mut self) {
        *self = Self::new();
    }
    pub fn is_closed(&self) -> bool {
        self.state.lock(Cell::get).closed
    }
    /// Static channel storage has an explicit terminal transition.
    pub fn close(&self) {
        self.update(|s| s.closed = true);
        for completion in &self.completions {
            completion.signal(());
        }
        for cancellation in &self.cancellations {
            cancellation.signal(());
        }
        self.rx_closed.signal(());
        self.request_closed.signal(());
        self.event_closed.signal(());
        self.control_closed.signal(());
        // Wake admission waiters; they check closed before allocating a slot.
        self.permits.set(N);
        while let Ok(id) = self.requests.try_receive() {
            self.release(id, false);
        }
    }
    fn release(&self, id: RequestId, caller: bool) {
        let release = self.slots.lock(|slots| {
            let mut slots = slots.borrow_mut();
            let slot = &mut slots[id.slot];
            if !slot.holds(id) {
                return false;
            }
            if caller {
                slot.caller = false;
            } else {
                slot.driver = false;
            }
            if slot.is_free() {
                slot.deadline = None;
                slot.result = None;
                true
            } else {
                false
            }
        });
        if release {
            self.permits.release(1);
        }
        if caller {
            self.cancellations[id.slot].signal(());
        } else {
            self.completions[id.slot].signal(());
        }
    }
}
impl<RM: RawMutex, RxE, TxE, D, const MAX: usize, const N: usize, const RX: usize> Default
    for QueueResources<RM, RxE, TxE, D, MAX, N, RX>
{
    fn default() -> Self {
        Self::new()
    }
}

pub struct QueueTx<'a, RM: RawMutex, RxE, TxE, D, const MAX: usize, const N: usize, const RX: usize>
{
    resources: &'a QueueResources<RM, RxE, TxE, D, MAX, N, RX>,
}
impl<RM: RawMutex, RxE, TxE, D, const MAX: usize, const N: usize, const RX: usize> Clone
    for QueueTx<'_, RM, RxE, TxE, D, MAX, N, RX>
{
    fn clone(&self) -> Self {
        self.resources.update(|s| {
            s.tx_handles = s
                .tx_handles
                .checked_add(1)
                .expect("TX handle count overflow");
        });
        Self {
            resources: self.resources,
        }
    }
}
pub struct QueueRx<'a, RM: RawMutex, RxE, TxE, D, const MAX: usize, const N: usize, const RX: usize>
{
    resources: &'a QueueResources<RM, RxE, TxE, D, MAX, N, RX>,
}
pub struct DriverPorts<
    'a,
    RM: RawMutex,
    RxE,
    TxE,
    D,
    const MAX: usize,
    const N: usize,
    const RX: usize,
> {
    resources: &'a QueueResources<RM, RxE, TxE, D, MAX, N, RX>,
}
impl<RM: RawMutex, RxE, TxE, D, const MAX: usize, const N: usize, const RX: usize> Drop
    for QueueTx<'_, RM, RxE, TxE, D, MAX, N, RX>
{
    fn drop(&mut self) {
        let last = self.resources.update(|s| {
            s.tx_handles -= 1;
            s.tx_handles == 0
        });
        if last {
            self.resources.request_closed.signal(());
        }
    }
}
impl<RM: RawMutex, RxE, TxE, D, const MAX: usize, const N: usize, const RX: usize> Drop
    for QueueRx<'_, RM, RxE, TxE, D, MAX, N, RX>
{
    fn drop(&mut self) {
        self.resources.update(|s| s.rx_alive = false);
        self.resources.event_closed.signal(());
    }
}
struct CallerLease<'a, RM: RawMutex, RxE, TxE, D, const MAX: usize, const N: usize, const RX: usize>
{
    resources: &'a QueueResources<RM, RxE, TxE, D, MAX, N, RX>,
    id: RequestId,
}
impl<RM: RawMutex, RxE, TxE, D, const MAX: usize, const N: usize, const RX: usize> Drop
    for CallerLease<'_, RM, RxE, TxE, D, MAX, N, RX>
{
    fn drop(&mut self) {
        self.resources.release(self.id, true);
    }
}
/// Driver retains this lease through protocol completion, even after acceptance
/// was reported to the caller. Payload access is synchronous and mutex-scoped.
pub struct RequestLease<
    'a,
    RM: RawMutex,
    RxE,
    TxE,
    D,
    const MAX: usize,
    const N: usize,
    const RX: usize,
> {
    resources: &'a QueueResources<RM, RxE, TxE, D, MAX, N, RX>,
    id: RequestId,
}
impl<RM: RawMutex, RxE, TxE, D: Deadline, const MAX: usize, const N: usize, const RX: usize>
    RequestLease<'_, RM, RxE, TxE, D, MAX, N, RX>
{
    pub fn with_request<T>(&self, f: impl FnOnce(Destination, &[u8], &D) -> T) -> T {
        self.resources.slots.lock(|slots| {
            let slots = slots.borrow();
            let slot = &slots[self.id.slot];
            assert!(slot.holds(self.id));
            f(
                slot.destination,
                &slot.payload[..slot.len],
                slot.deadline.as_ref().expect("request deadline"),
            )
        })
    }
    pub fn is_cancelled(&self) -> bool {
        if self.resources.is_closed() {
            return true;
        }
        self.resources.slots.lock(|slots| {
            let slots = slots.borrow();
            let slot = &slots[self.id.slot];
            !slot.accepted
                && (!slot.caller
                    || slot
                        .deadline
                        .as_ref()
                        .expect("deadline")
                        .remaining()
                        .is_zero())
        })
    }
    pub async fn cancelled(&self) {
        loop {
            if self.is_cancelled() {
                return;
            }
            let accepted = self
                .resources
                .slots
                .lock(|slots| slots.borrow()[self.id.slot].accepted);
            if accepted {
                self.resources.cancellations[self.id.slot].wait().await;
            } else {
                let deadline = self.with_request(|_, _, d| d.clone());
                let _ = until(deadline, self.resources.cancellations[self.id.slot].wait()).await;
            }
        }
    }
    pub fn complete(&self, result: Result<(), TransmitError<QueueError<TxE>>>) {
        self.resources.slots.lock(|slots| {
            let mut slots = slots.borrow_mut();
            let slot = &mut slots[self.id.slot];
            slot.accepted = result.is_ok();
            if slot.caller {
                slot.result = Some(result);
            }
        });
        self.resources.completions[self.id.slot].signal(());
    }
    /// Acceptance transfers payload lifetime to this driver's lease.
    pub fn accept(&self) {
        self.complete(Ok(()));
    }
}
impl<RM: RawMutex, RxE, TxE, D, const MAX: usize, const N: usize, const RX: usize> Drop
    for RequestLease<'_, RM, RxE, TxE, D, MAX, N, RX>
{
    fn drop(&mut self) {
        self.resources.release(self.id, false);
    }
}

/// How `run_actor` sees a leased request. It never accepts early, so only the
/// caller leaving or the queue closing cancels; expiry is `send_until`'s job.
impl<RM: RawMutex, RxE, TxE, D: Deadline, const MAX: usize, const N: usize, const RX: usize>
    QueuedRequest<TxE> for RequestLease<'_, RM, RxE, TxE, D, MAX, N, RX>
{
    fn is_abandoned(&self) -> bool {
        self.resources.is_closed()
            || !self
                .resources
                .slots
                .lock(|slots| slots.borrow()[self.id.slot].caller)
    }
    async fn abandoned(&mut self) {
        while !self.is_abandoned() {
            self.resources.cancellations[self.id.slot].wait().await;
        }
    }
    fn finish(self, result: Result<(), TransmitError<QueueError<TxE>>>) {
        self.complete(result);
    }
}

impl<RM: RawMutex, RxE, TxE, D: Deadline, const MAX: usize, const N: usize, const RX: usize>
    IsoTpAsyncWrite for QueueTx<'_, RM, RxE, TxE, D, MAX, N, RX>
{
    type Error = QueueError<TxE>;
    type Deadline = D;
    async fn send(
        &mut self,
        destination: Destination,
        payload: &[u8],
        deadline: D,
    ) -> Result<(), TransmitError<Self::Error>> {
        if self.resources.is_closed() {
            return Err(TransmitError::Closed);
        }
        if payload.len() > MAX {
            return Err(TransmitError::Rejected(QueueError::PayloadTooLarge {
                needed: payload.len(),
                capacity: MAX,
            }));
        }
        let propagated = deadline.clone();
        let resources = self.resources;
        until(deadline, async {
            let permit = resources
                .permits
                .acquire(1)
                .await
                .map_err(|_| TransmitError::Rejected(QueueError::WaitersFull))?;
            if resources.is_closed() {
                return Err(TransmitError::Closed);
            }
            if propagated.remaining().is_zero() {
                return Err(TransmitError::Timeout);
            }
            let id = resources.slots.lock(|slots| {
                let mut slots = slots.borrow_mut();
                let index = slots
                    .iter()
                    .position(Slot::is_free)
                    .expect("permit reserves a slot");
                let slot = &mut slots[index];
                resources.completions[index].reset();
                resources.cancellations[index].reset();
                slot.generation = slot
                    .generation
                    .checked_add(1)
                    .expect("request generation exhausted");
                slot.caller = true;
                slot.driver = true;
                slot.accepted = false;
                slot.destination = destination;
                slot.len = payload.len();
                slot.payload[..payload.len()].copy_from_slice(payload);
                slot.deadline = Some(propagated);
                slot.result = None;
                RequestId {
                    slot: index,
                    generation: slot.generation,
                }
            });
            permit.disarm();
            let _lease = CallerLease { resources, id };
            // Number of queued IDs cannot exceed the occupied slot count.
            assert!(resources.requests.try_send(id).is_ok());
            loop {
                // Take a stored result before checking for close: a request the
                // driver already completed must not be reported as `Closed`.
                let result = resources
                    .slots
                    .lock(|slots| slots.borrow_mut()[id.slot].result.take());
                if let Some(result) = result {
                    return result;
                }
                if resources.is_closed() {
                    return Err(TransmitError::Closed);
                }
                resources.completions[id.slot].wait().await;
            }
        })
        .await
        .unwrap_or(Err(TransmitError::Timeout))
    }
}
impl<RM: RawMutex, RxE, TxE, D, const MAX: usize, const N: usize, const RX: usize> IsoTpAsyncRead
    for QueueRx<'_, RM, RxE, TxE, D, MAX, N, RX>
{
    type Error = RxE;
    async fn recv_into(&mut self, out: &mut [u8]) -> Result<ReceivedPayload, ReceiveError<RxE>> {
        if self.resources.is_closed() {
            return Err(ReceiveError::Closed);
        }
        let event = match select(self.resources.rx_closed.wait(), self.resources.rx.receive()).await
        {
            Either::First(()) => return Err(ReceiveError::Closed),
            Either::Second(event) => event,
        };
        if self.resources.is_closed() {
            return Err(ReceiveError::Closed);
        }
        event.deliver(out)
    }
}
impl<'a, RM: RawMutex, RxE, TxE, D, const MAX: usize, const N: usize, const RX: usize>
    DriverPorts<'a, RM, RxE, TxE, D, MAX, N, RX>
{
    pub fn is_closed(&self) -> bool {
        self.resources.is_closed()
    }
    pub fn close(&self) {
        self.resources.close();
    }
    pub async fn closed(&self) {
        if !self.is_closed() {
            self.resources.control_closed.wait().await;
        }
    }
    fn lease(&self, id: RequestId) -> RequestLease<'a, RM, RxE, TxE, D, MAX, N, RX> {
        RequestLease {
            resources: self.resources,
            id,
        }
    }
    pub fn try_request(&self) -> Option<RequestLease<'a, RM, RxE, TxE, D, MAX, N, RX>> {
        if self.is_closed() {
            return None;
        }
        let id = self.resources.requests.try_receive().ok()?;
        Some(self.lease(id))
    }
    pub async fn next_request(&self) -> Option<RequestLease<'a, RM, RxE, TxE, D, MAX, N, RX>> {
        if let Some(request) = self.try_request() {
            return Some(request);
        }
        if self.is_closed() || self.resources.state.lock(Cell::get).tx_handles == 0 {
            return None;
        }
        match select(
            self.resources.request_closed.wait(),
            self.resources.requests.receive(),
        )
        .await
        {
            Either::First(()) => None,
            Either::Second(id) => Some(self.lease(id)),
        }
    }
    pub fn try_event(&self, event: RxEvent<RxE, MAX>) -> Result<(), RxEvent<RxE, MAX>> {
        self.resources.rx.try_send(event).map_err(|e| match e {
            embassy_sync::channel::TrySendError::Full(event) => event,
        })
    }
    pub async fn event_ready(&self) -> bool {
        if !self.receive_open() || self.is_closed() {
            return false;
        }
        match select(
            self.resources.event_closed.wait(),
            core::future::poll_fn(|cx| self.resources.rx.poll_ready_to_send(cx)),
        )
        .await
        {
            Either::First(()) => false,
            Either::Second(()) => true,
        }
    }
    pub fn receive_open(&self) -> bool {
        self.resources.state.lock(Cell::get).rx_alive
    }
    pub async fn receiver_closed(&self) {
        if self.receive_open() && !self.is_closed() {
            self.resources.event_closed.wait().await;
        }
    }
    pub async fn send_event(&self, event: RxEvent<RxE, MAX>) -> bool {
        if !self.receive_open() || self.is_closed() {
            return false;
        }
        match select(
            self.resources.event_closed.wait(),
            self.resources.rx.send(event),
        )
        .await
        {
            Either::First(()) => false,
            Either::Second(()) => true,
        }
    }
}
impl<RM: RawMutex, RxE, TxE, D, const MAX: usize, const N: usize, const RX: usize> Drop
    for DriverPorts<'_, RM, RxE, TxE, D, MAX, N, RX>
{
    fn drop(&mut self) {
        self.close();
    }
}

/// Own both transport halves until shutdown, a fatal error, or closure.
///
/// RX-queue backpressure is timed on `W::Deadline`'s clock.
pub async fn run_actor<R, W, RM, H, C, const MAX: usize, const N: usize, const RX: usize>(
    mut reader: R,
    mut writer: W,
    ports: DriverPorts<'_, RM, R::Error, W::Error, W::Deadline, MAX, N, RX>,
    config: ActorConfig,
    hooks: H,
    shutdown: C,
) -> Result<ActorStop, ActorError<R::Error, W::Error>>
where
    R: IsoTpAsyncRead,
    W: IsoTpAsyncWrite,
    RM: RawMutex,
    H: ActorHooks<R::Error, W::Error>,
    C: Future<Output = ()>,
{
    hooks.on_actor_started();
    let burst = config.burst_limit.max(1);
    let receive = async {
        let mut bytes = [0; MAX];
        let mut processed = 0;
        loop {
            if processed == burst {
                embassy_futures::yield_now().await;
                processed = 0;
            }
            processed += 1;
            let input = match select(ports.receiver_closed(), reader.recv_into(&mut bytes)).await {
                Either::First(()) => return Ok(ActorStop::Closed),
                Either::Second(result) => result,
            };
            let Some(event) = classify_receive(input, MAX, &hooks, |_| bytes)? else {
                ports.close();
                return Ok(ActorStop::Closed);
            };
            // Time only real waits: most events fit in the queue at once.
            let Err(event) = ports.try_event(event) else {
                continue;
            };
            let stopwatch = Stopwatch::<W::Deadline>::start();
            if !ports.send_event(event).await {
                return Ok(ActorStop::Closed);
            }
            let blocked = stopwatch.elapsed();
            if blocked >= BACKPRESSURE_REPORT_THRESHOLD {
                hooks.on_rx_backpressure(blocked);
            }
        }
    };
    let send = async {
        let mut tx_bytes = [0; MAX];
        let mut processed = 0;
        while let Some(request) = ports.next_request().await {
            if processed == burst {
                embassy_futures::yield_now().await;
                processed = 0;
            }
            processed += 1;
            // Copy out of the slot, since the mutex cannot be held across the send.
            // Only the payload's bytes are copied into the reused buffer.
            let (destination, len, deadline) = request.with_request(|d, p, t| {
                tx_bytes[..p.len()].copy_from_slice(p);
                (d, p.len(), t.clone())
            });
            match dispatch(
                &mut writer,
                &hooks,
                request,
                destination,
                &tx_bytes[..len],
                deadline,
            )
            .await
            {
                Ok(()) => {}
                Err(SendStop::Fatal(e)) => return Err(ActorError::Send(e)),
                Err(SendStop::Closed) => {
                    ports.close();
                    return Ok(ActorStop::Closed);
                }
            }
        }
        Ok(ActorStop::Closed)
    };
    let work = async {
        let mut receive = core::pin::pin!(receive);
        let mut send = core::pin::pin!(send);
        match select(receive.as_mut(), send.as_mut()).await {
            Either::First(result) => {
                result?;
                if ports.is_closed() {
                    return Ok(ActorStop::Closed);
                }
                send.await
            }
            Either::Second(result) => {
                result?;
                if ports.is_closed() {
                    return Ok(ActorStop::Closed);
                }
                receive.await
            }
        }
    };
    match select3(work, shutdown, ports.closed()).await {
        Either3::First(result) => result,
        Either3::Second(()) => Ok(ActorStop::Cancelled),
        Either3::Third(()) => Ok(ActorStop::Closed),
    }
}

#[cfg(test)]
mod tests;
