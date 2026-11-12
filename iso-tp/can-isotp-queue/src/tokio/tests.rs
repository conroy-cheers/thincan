use super::{ActorError, ActorStop, make_endpoints, run_actor};
use crate::{ActorConfig, ActorHooks, NoopHooks, QueueError};
use can_isotp_interface::{
    Deadline, Destination, IsoTpAsyncRead, IsoTpAsyncWrite, ReceiveError, ReceivedPayload,
    TransmitError,
};
use std::sync::atomic::AtomicUsize;
use std::time::Duration;
use std::{
    future::Future,
    sync::{Arc, atomic::Ordering},
};
use tokio::sync::{mpsc, oneshot};

struct Reader {
    input: mpsc::Receiver<Result<Vec<u8>, ReceiveError<&'static str>>>,
    calls: Arc<AtomicUsize>,
}
impl IsoTpAsyncRead for Reader {
    type Error = &'static str;
    async fn recv_into(
        &mut self,
        out: &mut [u8],
    ) -> Result<ReceivedPayload, ReceiveError<Self::Error>> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        let bytes = self.input.recv().await.ok_or(ReceiveError::Closed)??;
        if bytes.len() > out.len() {
            return Err(ReceiveError::BufferTooSmall {
                needed: bytes.len(),
                got: out.len(),
            });
        }
        out[..bytes.len()].copy_from_slice(&bytes);
        Ok(ReceivedPayload {
            meta: can_isotp_interface::RecvMeta { reply_to: 0x22 },
            len: bytes.len(),
        })
    }
}
struct Writer {
    accepted: mpsc::UnboundedSender<Vec<u8>>,
}
impl IsoTpAsyncWrite for Writer {
    type Error = &'static str;
    type Deadline = tokio::time::Instant;
    async fn send(
        &mut self,
        _: Destination,
        bytes: &[u8],
        deadline: Self::Deadline,
    ) -> Result<(), TransmitError<Self::Error>> {
        if bytes == [0xff] {
            return Err(TransmitError::Fatal("TX failed"));
        }
        if bytes == [0xfe] {
            return Err(TransmitError::Rejected("bad command"));
        }
        if bytes == [0xfd] {
            core::future::pending::<()>().await;
        }
        if bytes == [0xfc] {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        if deadline.remaining().is_zero() {
            return Err(TransmitError::Timeout);
        }
        self.accepted.send(bytes.to_vec()).unwrap();
        Ok(())
    }
}
fn deadline(ms: u64) -> tokio::time::Instant {
    tokio::time::Instant::now() + Duration::from_millis(ms)
}
type Backend = (
    Reader,
    Writer,
    mpsc::Sender<Result<Vec<u8>, ReceiveError<&'static str>>>,
    mpsc::UnboundedReceiver<Vec<u8>>,
    Arc<AtomicUsize>,
);
fn backend() -> Backend {
    let (input, rx) = mpsc::channel(8);
    let (accepted, output) = mpsc::unbounded_channel();
    let calls = Arc::new(AtomicUsize::new(0));
    (
        Reader {
            input: rx,
            calls: calls.clone(),
        },
        Writer { accepted },
        input,
        output,
        calls,
    )
}

#[tokio::test]
async fn rx_backpressure_leaves_tx_and_shutdown_live() {
    let (mut tx, _rx, ports) = make_endpoints(32, 2, 1).unwrap();
    let (reader, writer, input, mut output, calls) = backend();
    let (stop, shutdown) = oneshot::channel();
    let actor = tokio::spawn(run_actor(
        reader,
        writer,
        ports,
        ActorConfig::default(),
        NoopHooks,
        async {
            let _ = shutdown.await;
        },
    ));
    input.send(Ok(vec![1])).await.unwrap();
    input.send(Ok(vec![2])).await.unwrap();
    while calls.load(Ordering::Relaxed) < 2 {
        tokio::task::yield_now().await;
    }
    tx.send(Destination::Physical(3), &[7], deadline(100))
        .await
        .unwrap();
    assert_eq!(output.recv().await.unwrap(), vec![7]);
    stop.send(()).unwrap();
    assert!(matches!(actor.await.unwrap(), Ok(ActorStop::Cancelled)));
}
#[tokio::test]
async fn rejected_commands_continue_and_fatal_cause_stays_with_owner() {
    let (mut tx, mut rx, ports) = make_endpoints(32, 4, 2).unwrap();
    let (reader, writer, _input, mut output, _) = backend();
    let actor = tokio::spawn(run_actor(
        reader,
        writer,
        ports,
        ActorConfig::default(),
        NoopHooks,
        core::future::pending(),
    ));
    assert!(matches!(
        tx.send(Destination::Physical(1), &[0xfe], deadline(100))
            .await,
        Err(TransmitError::Rejected(QueueError::Backend("bad command")))
    ));
    tx.send(Destination::Functional(1), &[1], deadline(100))
        .await
        .unwrap();
    assert_eq!(output.recv().await.unwrap(), vec![1]);
    assert!(matches!(
        tx.send(Destination::Physical(1), &[0xff], deadline(100))
            .await,
        Err(TransmitError::Closed)
    ));
    assert!(matches!(
        actor.await.unwrap(),
        Err(ActorError::Send("TX failed"))
    ));
    assert!(matches!(
        rx.recv_into(&mut [0; 32]).await,
        Err(ReceiveError::Closed)
    ));
}
#[tokio::test]
async fn expired_and_abandoned_requests_are_never_dispatched() {
    let (mut tx, rx, ports) = make_endpoints(32, 2, 2).unwrap();
    let (reader, writer, _input, mut output, _) = backend();
    assert!(matches!(
        tx.send(Destination::Physical(1), &[1], tokio::time::Instant::now())
            .await,
        Err(TransmitError::Timeout)
    ));
    {
        let send = tx.send(Destination::Physical(1), &[2], deadline(100));
        tokio::pin!(send);
        std::future::poll_fn(|cx| {
            assert!(send.as_mut().poll(cx).is_pending());
            std::task::Poll::Ready(())
        })
        .await;
    }
    drop(tx);
    drop(rx);
    assert!(matches!(
        run_actor(
            reader,
            writer,
            ports,
            ActorConfig::default(),
            NoopHooks,
            core::future::pending()
        )
        .await,
        Ok(ActorStop::Closed)
    ));
    assert!(output.try_recv().is_err());
}
#[tokio::test]
async fn frontends_close_independently_and_empty_buffer_consumes_one_payload() {
    let (tx, mut rx, ports) = make_endpoints(32, 2, 2).unwrap();
    let (reader, writer, input, _output, _) = backend();
    drop(tx);
    let actor = tokio::spawn(run_actor(
        reader,
        writer,
        ports,
        ActorConfig::default(),
        NoopHooks,
        core::future::pending(),
    ));
    input.send(Ok(vec![1, 2])).await.unwrap();
    input.send(Ok(vec![3])).await.unwrap();
    assert!(matches!(
        rx.recv_into(&mut []).await,
        Err(ReceiveError::BufferTooSmall { needed: 2, got: 0 })
    ));
    let mut out = [0; 8];
    assert_eq!(rx.recv_into(&mut out).await.unwrap().meta.reply_to, 0x22);
    assert_eq!(out[0], 3);
    drop(rx);
    assert!(matches!(actor.await.unwrap(), Ok(ActorStop::Closed)));
    let (mut tx, rx, ports) = make_endpoints(32, 2, 2).unwrap();
    let (reader, writer, _input, mut output, _) = backend();
    drop(rx);
    let actor = tokio::spawn(run_actor(
        reader,
        writer,
        ports,
        ActorConfig::default(),
        NoopHooks,
        core::future::pending(),
    ));
    tx.send(Destination::Physical(1), &[7], deadline(100))
        .await
        .unwrap();
    assert_eq!(output.recv().await.unwrap(), vec![7]);
    drop(tx);
    actor.await.unwrap().unwrap();
}
struct SlowHook;
impl ActorHooks<&'static str, &'static str> for SlowHook {
    fn on_tx_request(&self, _: Destination, _: usize, _: Duration) {
        std::thread::sleep(Duration::from_millis(30));
    }
}
#[tokio::test]
async fn slow_hook_cannot_reset_the_admission_deadline() {
    let (mut tx, _rx, ports) = make_endpoints(32, 2, 2).unwrap();
    let (reader, writer, _input, mut output, _) = backend();
    let (stop, shutdown) = oneshot::channel();
    let actor = tokio::spawn(run_actor(
        reader,
        writer,
        ports,
        ActorConfig::default(),
        SlowHook,
        async {
            let _ = shutdown.await;
        },
    ));
    assert!(matches!(
        tx.send(Destination::Physical(1), &[1], deadline(10)).await,
        Err(TransmitError::Timeout)
    ));
    assert!(output.try_recv().is_err());
    stop.send(()).unwrap();
    actor.await.unwrap().unwrap();
}
#[tokio::test]
async fn backend_closed_terminates_both_directions() {
    let (mut tx, mut rx, ports) = make_endpoints(32, 2, 2).unwrap();
    let (reader, writer, input, _output, _) = backend();
    drop(input);
    run_actor(
        reader,
        writer,
        ports,
        ActorConfig::default(),
        NoopHooks,
        core::future::pending(),
    )
    .await
    .unwrap();
    assert!(matches!(
        tx.send(Destination::Physical(1), &[1], deadline(10)).await,
        Err(TransmitError::Closed)
    ));
    assert!(matches!(
        rx.recv_into(&mut [0; 8]).await,
        Err(ReceiveError::Closed)
    ));
}
struct TimeoutHook(Arc<AtomicUsize>);
impl ActorHooks<&'static str, &'static str> for TimeoutHook {
    fn on_tx_result(&self, result: &Result<(), TransmitError<&'static str>>) {
        if matches!(result, Err(TransmitError::Timeout)) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }
}
#[tokio::test]
async fn expiry_during_a_stalled_send_is_reported_to_hooks() {
    let (mut tx, _rx, ports) = make_endpoints(32, 2, 2).unwrap();
    let (reader, writer, _input, _output, _) = backend();
    let timeouts = Arc::new(AtomicUsize::new(0));
    let (stop, shutdown) = oneshot::channel();
    let actor = tokio::spawn(run_actor(
        reader,
        writer,
        ports,
        ActorConfig::default(),
        TimeoutHook(timeouts.clone()),
        async {
            let _ = shutdown.await;
        },
    ));
    // 0xfd makes the test writer stall forever.
    assert!(matches!(
        tx.send(Destination::Physical(1), &[0xfd], deadline(20))
            .await,
        Err(TransmitError::Timeout)
    ));
    tokio::time::sleep(Duration::from_millis(10)).await;
    stop.send(()).unwrap();
    actor.await.unwrap().unwrap();
    assert_eq!(timeouts.load(Ordering::SeqCst), 1);
}
#[tokio::test]
async fn expiry_while_queued_is_reported_to_hooks() {
    let (mut tx, _rx, ports) = make_endpoints(32, 2, 2).unwrap();
    let mut tx2 = tx.clone();
    let (reader, writer, _input, _output, _) = backend();
    let timeouts = Arc::new(AtomicUsize::new(0));
    let (stop, shutdown) = oneshot::channel();
    let actor = tokio::spawn(run_actor(
        reader,
        writer,
        ports,
        ActorConfig::default(),
        TimeoutHook(timeouts.clone()),
        async {
            let _ = shutdown.await;
        },
    ));
    // 0xfc keeps the writer busy for 50 ms; the second request expires in the queue.
    let (slow, queued) = tokio::join!(
        tx.send(Destination::Physical(1), &[0xfc], deadline(1_000)),
        async {
            tokio::task::yield_now().await;
            tx2.send(Destination::Physical(1), &[1], deadline(10)).await
        }
    );
    slow.unwrap();
    assert!(matches!(queued, Err(TransmitError::Timeout)));
    tokio::time::sleep(Duration::from_millis(10)).await;
    stop.send(()).unwrap();
    actor.await.unwrap().unwrap();
    assert_eq!(timeouts.load(Ordering::SeqCst), 1);
}
