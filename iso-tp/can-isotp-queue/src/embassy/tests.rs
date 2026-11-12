use super::*;
use core::{
    future::Future,
    pin::Pin,
    task::{Context, Waker},
};
use embassy_sync::blocking_mutex::raw::NoopRawMutex;
type Resources = QueueResources<NoopRawMutex, (), (), tokio::time::Instant, 32, 1, 1>;
fn pending<F: Future>(future: Pin<&mut F>) {
    assert!(
        future
            .poll(&mut Context::from_waker(Waker::noop()))
            .is_pending()
    );
}
fn deadline() -> tokio::time::Instant {
    tokio::time::Instant::now() + core::time::Duration::from_secs(1)
}
#[tokio::test]
async fn abandoned_slot_waits_for_driver_and_late_result_does_not_complete_next_command() {
    let resources = Resources::new();
    let (mut tx, _rx, ports) = resources.split();
    let old = {
        let mut send = core::pin::pin!(tx.send(Destination::Physical(1), &[1], deadline()));
        pending(send.as_mut());
        ports.try_request().unwrap()
    };
    let mut next = core::pin::pin!(tx.send(Destination::Physical(1), &[2], deadline()));
    pending(next.as_mut());
    assert!(old.is_cancelled());
    old.complete(Err(TransmitError::Rejected(QueueError::Backend(()))));
    drop(old);
    pending(next.as_mut());
    let request = ports.try_request().unwrap();
    request.with_request(|_, payload, _| assert_eq!(payload, &[2]));
    pending(next.as_mut());
    request.accept();
    next.await.unwrap();
}
#[tokio::test]
async fn accepted_payload_keeps_slot_after_caller_success_and_deadline_expiry() {
    let resources = Resources::new();
    let (mut tx, _rx, ports) = resources.split();
    let mut send = core::pin::pin!(tx.send(
        Destination::Physical(1),
        &[1],
        tokio::time::Instant::now() + core::time::Duration::from_millis(5)
    ));
    pending(send.as_mut());
    let request = ports.try_request().unwrap();
    request.accept();
    send.await.unwrap();
    tokio::time::sleep(core::time::Duration::from_millis(10)).await;
    assert!(!request.is_cancelled());
    let mut cancelled = core::pin::pin!(request.cancelled());
    pending(cancelled.as_mut());
    resources.close();
    cancelled.await;
    request.with_request(|_, payload, _| assert_eq!(payload, &[1]));
}
#[tokio::test]
async fn closure_wakes_completion_cancellation_receive_and_admission() {
    let resources = Resources::new();
    let (mut tx, mut rx, ports) = resources.split();
    let mut tx2 = tx.clone();
    let mut send = core::pin::pin!(tx.send(Destination::Physical(1), &[1], deadline()));
    pending(send.as_mut());
    let request = ports.try_request().unwrap();
    let mut cancelled = core::pin::pin!(request.cancelled());
    pending(cancelled.as_mut());
    let mut admission = core::pin::pin!(tx2.send(Destination::Physical(1), &[2], deadline()));
    pending(admission.as_mut());
    let mut bytes = [0; 32];
    let mut receive = core::pin::pin!(rx.recv_into(&mut bytes));
    pending(receive.as_mut());
    resources.close();
    assert!(matches!(send.await, Err(TransmitError::Closed)));
    assert!(matches!(admission.await, Err(TransmitError::Closed)));
    cancelled.await;
    assert!(matches!(receive.await, Err(ReceiveError::Closed)));
}

#[tokio::test]
async fn repeated_abandonment_and_late_completion_reuse_one_slot_safely() {
    let resources = Resources::new();
    let (mut tx, _rx, ports) = resources.split();
    for generation in 0u16..1_000 {
        let old = {
            let mut send = core::pin::pin!(tx.send(Destination::Physical(1), &[], deadline()));
            pending(send.as_mut());
            ports.try_request().unwrap()
        };
        assert!(old.is_cancelled());
        let payload = generation.to_le_bytes();
        let mut next = core::pin::pin!(tx.send(Destination::Physical(1), &payload, deadline()));
        pending(next.as_mut());
        assert!(
            ports.try_request().is_none(),
            "old lease must keep its slot"
        );
        old.complete(Err(TransmitError::Rejected(QueueError::Backend(()))));
        drop(old);
        pending(next.as_mut());
        let request = ports.try_request().unwrap();
        request.with_request(|_, bytes, _| assert_eq!(bytes, &payload));
        request.accept();
        next.await.unwrap();
        drop(request);
    }
}
#[tokio::test]
async fn completed_send_reports_success_even_if_queue_closes_before_caller_polls() {
    let resources = Resources::new();
    let (mut tx, _rx, ports) = resources.split();
    let mut send = core::pin::pin!(tx.send(Destination::Physical(1), &[1], deadline()));
    pending(send.as_mut());
    let request = ports.try_request().unwrap();
    request.accept();
    resources.close();
    assert!(matches!(send.await, Ok(())));
}

/// Yields the scripted one-byte payloads, then waits forever.
struct ScriptedReader(std::collections::VecDeque<u8>);
impl IsoTpAsyncRead for ScriptedReader {
    type Error = ();
    async fn recv_into(&mut self, out: &mut [u8]) -> Result<ReceivedPayload, ReceiveError<()>> {
        match self.0.pop_front() {
            Some(byte) => {
                out[0] = byte;
                Ok(ReceivedPayload {
                    meta: can_isotp_interface::RecvMeta { reply_to: 1 },
                    len: 1,
                })
            }
            None => core::future::pending().await,
        }
    }
}
struct IdleWriter;
impl IsoTpAsyncWrite for IdleWriter {
    type Error = ();
    type Deadline = tokio::time::Instant;
    async fn send(
        &mut self,
        _: Destination,
        _: &[u8],
        _: Self::Deadline,
    ) -> Result<(), TransmitError<()>> {
        Ok(())
    }
}
#[derive(Default)]
struct BackpressureHook(core::cell::Cell<Option<core::time::Duration>>);
impl ActorHooks<(), ()> for &BackpressureHook {
    fn on_rx_backpressure(&self, blocked: core::time::Duration) {
        self.0.set(Some(blocked));
    }
}
#[tokio::test]
async fn actor_reports_rx_backpressure_while_the_consumer_lags() {
    let resources = Resources::new();
    let (_tx, mut rx, ports) = resources.split();
    let hook = BackpressureHook::default();
    let actor = run_actor(
        ScriptedReader([1, 2].into()),
        IdleWriter,
        ports,
        ActorConfig::default(),
        &hook,
        core::future::pending(),
    );
    // The RX queue holds one event, so the second waits for this consumer.
    let consumer = async {
        tokio::time::sleep(core::time::Duration::from_millis(20)).await;
        let mut out = [0; 32];
        rx.recv_into(&mut out).await.unwrap();
        rx.recv_into(&mut out).await.unwrap();
        assert_eq!(out[0], 2);
    };
    match select(actor, consumer).await {
        Either::First(result) => panic!("actor stopped early: {result:?}"),
        Either::Second(()) => {}
    }
    let blocked = hook.0.get().expect("backpressure reported");
    assert!(blocked >= core::time::Duration::from_millis(1));
}

struct StalledWriter;
impl IsoTpAsyncWrite for StalledWriter {
    type Error = ();
    type Deadline = tokio::time::Instant;
    async fn send(
        &mut self,
        _: Destination,
        _: &[u8],
        _: Self::Deadline,
    ) -> Result<(), TransmitError<()>> {
        core::future::pending().await
    }
}
#[derive(Default)]
struct TimeoutHook(core::cell::Cell<usize>);
impl ActorHooks<(), ()> for &TimeoutHook {
    fn on_tx_result(&self, result: &Result<(), TransmitError<()>>) {
        if matches!(result, Err(TransmitError::Timeout)) {
            self.0.set(self.0.get() + 1);
        }
    }
}
#[tokio::test]
async fn expiry_during_a_stalled_send_is_reported_to_hooks() {
    let resources = Resources::new();
    let (mut tx, _rx, ports) = resources.split();
    let hook = TimeoutHook::default();
    let actor = run_actor(
        ScriptedReader(Default::default()),
        StalledWriter,
        ports,
        ActorConfig::default(),
        &hook,
        core::future::pending(),
    );
    let caller = async {
        let deadline = tokio::time::Instant::now() + core::time::Duration::from_millis(20);
        let result = tx.send(Destination::Physical(1), &[1], deadline).await;
        // Let the actor observe the expiry too.
        tokio::time::sleep(core::time::Duration::from_millis(10)).await;
        result
    };
    match select(actor, caller).await {
        Either::First(result) => panic!("actor stopped early: {result:?}"),
        Either::Second(result) => assert!(matches!(result, Err(TransmitError::Timeout))),
    }
    assert_eq!(hook.0.get(), 1);
}

/// Writer that takes 50 ms to accept each payload.
struct SlowWriter;
impl IsoTpAsyncWrite for SlowWriter {
    type Error = ();
    type Deadline = tokio::time::Instant;
    async fn send(
        &mut self,
        _: Destination,
        _: &[u8],
        _: Self::Deadline,
    ) -> Result<(), TransmitError<()>> {
        tokio::time::sleep(core::time::Duration::from_millis(50)).await;
        Ok(())
    }
}
#[tokio::test]
async fn expiry_while_queued_is_reported_to_hooks() {
    let resources = QueueResources::<NoopRawMutex, (), (), tokio::time::Instant, 32, 2, 1>::new();
    let (mut tx, _rx, ports) = resources.split();
    let mut tx2 = tx.clone();
    let hook = TimeoutHook::default();
    let actor = run_actor(
        ScriptedReader(Default::default()),
        SlowWriter,
        ports,
        ActorConfig::default(),
        &hook,
        core::future::pending(),
    );
    let callers = async {
        let now = tokio::time::Instant::now();
        let ms = core::time::Duration::from_millis;
        let (slow, queued) = embassy_futures::join::join(
            tx.send(Destination::Physical(1), &[1], now + ms(1_000)),
            tx2.send(Destination::Physical(1), &[2], now + ms(10)),
        )
        .await;
        // Let the actor dequeue the expired request.
        tokio::time::sleep(ms(10)).await;
        (slow, queued)
    };
    let (slow, queued) = match select(actor, callers).await {
        Either::First(result) => panic!("actor stopped early: {result:?}"),
        Either::Second(results) => results,
    };
    slow.unwrap();
    assert!(matches!(queued, Err(TransmitError::Timeout)));
    assert_eq!(hook.0.get(), 1);
}
