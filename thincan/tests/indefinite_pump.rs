#![cfg(feature = "std")]

use std::future::{Future, poll_fn};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use std::task::Poll;

use can_isotp_interface::RecvMeta;
use can_isotp_interface::{IsoTpAsyncRead, ReceiveError, ReceivedPayload};
use thincan::{IngestError, IngestOutcome, NoopRawMutex, PumpError};
use tokio::sync::{Mutex, mpsc};

struct Maplet;
impl thincan::MapletSpec<1> for Maplet {
    const MESSAGE_IDS: [u16; 1] = [0x10];
}
impl thincan::MapletHasBundle<thincan::Unscoped> for Maplet {}

#[derive(Debug)]
struct Message;
impl thincan::Message for Message {
    const ID: u16 = 0x10;
}
impl thincan::CapnpMessage for Message {
    type Owned = ();
}

enum Input {
    Payload(u8, Vec<u8>),
    Error(ReceiveError<&'static str>),
    InvalidLength,
}

#[derive(Clone)]
struct Reader {
    input: Arc<Mutex<mpsc::Receiver<Input>>>,
    calls: Arc<AtomicUsize>,
}

impl IsoTpAsyncRead for Reader {
    type Error = &'static str;

    async fn recv_into(
        &mut self,
        out: &mut [u8],
    ) -> Result<ReceivedPayload, ReceiveError<Self::Error>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        match self.input.lock().await.recv().await {
            Some(Input::Payload(from, bytes)) => {
                if bytes.len() > out.len() {
                    return Err(ReceiveError::BufferTooSmall {
                        needed: bytes.len(),
                        got: out.len(),
                    });
                }
                out[..bytes.len()].copy_from_slice(&bytes);
                Ok(ReceivedPayload {
                    meta: RecvMeta { reply_to: from },
                    len: bytes.len(),
                })
            }
            Some(Input::Error(error)) => Err(error),
            Some(Input::InvalidLength) => Ok(ReceivedPayload {
                meta: RecvMeta { reply_to: 1 },
                len: out.len() + 1,
            }),
            None => Err(ReceiveError::Fatal("closed")),
        }
    }
}

type Interface = thincan::Interface<Maplet, NoopRawMutex, Reader, [u8; 32], 1, 2, 32, 1>;

fn setup() -> (Interface, Reader, mpsc::Sender<Input>, Arc<AtomicUsize>) {
    let (sender, input) = mpsc::channel(8);
    let calls = Arc::new(AtomicUsize::new(0));
    let reader = Reader {
        input: Arc::new(Mutex::new(input)),
        calls: calls.clone(),
    };
    (
        Interface::new(reader.clone(), [0; 32]),
        reader,
        sender,
        calls,
    )
}

#[tokio::test(flavor = "current_thread")]
async fn idle_pump_waits_and_cancelled_receive_preserves_next_payload() {
    let (iface, reader, sender, calls) = setup();
    let mut scratch = [0; 32];
    let mut pump = thincan::ReceivePump::new(reader, &iface, &mut scratch);
    {
        let pump = pump.next();
        tokio::pin!(pump);
        poll_fn(|cx| {
            assert!(pump.as_mut().poll(cx).is_pending());
            Poll::Ready(())
        })
        .await;
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }
    sender
        .send(Input::Payload(0x22, vec![0x10, 0, 7, 8]))
        .await
        .unwrap();
    assert_eq!(
        pump.next().await.unwrap(),
        IngestOutcome {
            from: 0x22,
            id: 0x10,
            len: 2,
        }
    );
    let received = iface
        .bus()
        .__recv_next_capnp_from::<Message>(0x22)
        .await
        .unwrap();
    assert_eq!(received.from, 0x22);
    assert_eq!(received.id, 0x10);
    assert_eq!(received.body(), &[7, 8]);
}

#[tokio::test(flavor = "current_thread")]
async fn consumed_rejections_allow_the_next_payload_to_be_ingested() {
    let (iface, reader, sender, _) = setup();
    let mut scratch = [0; 32];
    let mut pump = thincan::ReceivePump::new(reader, &iface, &mut scratch);
    sender.send(Input::Payload(1, vec![0x10])).await.unwrap();
    sender.send(Input::Payload(1, vec![0xff, 0])).await.unwrap();
    sender.send(Input::Payload(1, vec![0; 33])).await.unwrap();
    sender
        .send(Input::Error(ReceiveError::Rejected("bad packet")))
        .await
        .unwrap();
    sender
        .send(Input::Payload(1, vec![0x10, 0, 9]))
        .await
        .unwrap();
    let errors = [
        pump.next().await.unwrap_err(),
        pump.next().await.unwrap_err(),
        pump.next().await.unwrap_err(),
        pump.next().await.unwrap_err(),
    ];
    assert!(matches!(
        errors[0],
        PumpError::Ingest(IngestError::MalformedPayload)
    ));
    assert!(matches!(
        errors[1],
        PumpError::Ingest(IngestError::UnknownId { id: 0xff })
    ));
    assert!(matches!(
        errors[2],
        PumpError::Receive(ReceiveError::BufferTooSmall {
            needed: 33,
            got: 32
        })
    ));
    assert!(matches!(
        errors[3],
        PumpError::Receive(ReceiveError::Rejected("bad packet"))
    ));
    assert!(errors.iter().all(|error| !error.is_terminal()));
    pump.next().await.unwrap();
    assert_eq!(
        iface
            .bus()
            .__recv_next_capnp_from::<Message>(1)
            .await
            .unwrap()
            .body(),
        &[9]
    );
}

#[tokio::test(flavor = "current_thread")]
async fn terminal_failures_and_contract_violations_are_returned_to_the_owner() {
    let (iface, reader, sender, _) = setup();
    let mut scratch = [0; 32];
    let mut pump = thincan::ReceivePump::new(reader, &iface, &mut scratch);
    sender.send(Input::InvalidLength).await.unwrap();
    let error = pump.next().await.unwrap_err();
    assert!(matches!(
        error,
        PumpError::InvalidPayloadLength {
            reported: 33,
            capacity: 32
        }
    ));
    assert!(error.is_terminal());
    assert!(iface.is_closed());
    let error = pump.next().await.unwrap_err();
    assert!(matches!(error, PumpError::Receive(ReceiveError::Closed)));
    assert!(error.is_terminal());
}

#[tokio::test]
async fn fatal_receive_closes_mailboxes_and_is_not_polled_again() {
    let (iface, reader, sender, calls) = setup();
    let mut scratch = [0; 32];
    let mut pump = thincan::ReceivePump::new(reader, &iface, &mut scratch);
    sender
        .send(Input::Error(ReceiveError::Fatal("device gone")))
        .await
        .unwrap();
    let error = pump.next().await.unwrap_err();
    assert!(matches!(
        error,
        PumpError::Receive(ReceiveError::Fatal("device gone"))
    ));
    assert!(error.is_terminal());
    assert!(matches!(
        pump.next().await,
        Err(PumpError::Receive(ReceiveError::Closed))
    ));
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        iface
            .bus()
            .__recv_next_capnp_from::<Message>(1)
            .await
            .unwrap_err()
            .kind,
        thincan::ErrorKind::Closed
    );
}
