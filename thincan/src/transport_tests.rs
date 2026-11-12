use super::*;
use core::{
    future::Future,
    task::{Context, Waker},
};
struct Maplet;
impl MapletSpec<1> for Maplet {
    const MESSAGE_IDS: [u16; 1] = [0x10];
}
impl MapletHasBundle<Unscoped> for Maplet {}
#[derive(Debug)]
struct Message;
impl crate::Message for Message {
    const ID: u16 = 0x10;
}
impl CapnpMessage for Message {
    type Owned = ();
}
struct Reader<'a>(&'a Cell<usize>);
impl can_isotp_interface::IsoTpAsyncRead for Reader<'_> {
    type Error = ();
    async fn recv_into(
        &mut self,
        out: &mut [u8],
    ) -> Result<can_isotp_interface::ReceivedPayload, can_isotp_interface::ReceiveError<()>> {
        self.0.set(self.0.get() + 1);
        out[..3].copy_from_slice(&[0x10, 0, 7]);
        Ok(can_isotp_interface::ReceivedPayload {
            meta: can_isotp_interface::RecvMeta { reply_to: 2 },
            len: 3,
        })
    }
}
#[tokio::test]
async fn cancelling_pending_ingestion_retains_received_payload() {
    let iface = Interface::<Maplet, NoopRawMutex, (), _, 1, 2, 32, 2>::new((), [0; 32]);
    let calls = Cell::new(0);
    let mut scratch = [0; 32];
    let mut pump = ReceivePump::new(Reader(&calls), &iface, &mut scratch);
    let locked = iface.rx_state.lock().await;
    {
        let mut next = core::pin::pin!(pump.next());
        assert!(
            next.as_mut()
                .poll(&mut Context::from_waker(Waker::noop()))
                .is_pending()
        );
    }
    assert_eq!(calls.get(), 1);
    drop(locked);
    pump.next().await.unwrap();
    assert_eq!(calls.get(), 1);
    assert_eq!(
        iface
            .bus()
            .__recv_next_capnp_from::<Message>(2)
            .await
            .unwrap()
            .body(),
        &[7]
    );
}
#[tokio::test]
async fn closure_wakes_existing_mailbox_waiters_and_suppresses_buffered_data() {
    let iface = Interface::<Maplet, NoopRawMutex, (), _, 1, 2, 32, 2>::new((), [0; 32]);
    let bus = iface.bus();
    let mut wait = core::pin::pin!(bus.__recv_next_capnp_from::<Message>(2));
    assert!(
        wait.as_mut()
            .poll(&mut Context::from_waker(Waker::noop()))
            .is_pending()
    );
    iface.close();
    assert_eq!(wait.await.unwrap_err().kind, ErrorKind::Closed);
    assert!(matches!(
        bus.ingest(2, &[0x10, 0, 7]).await,
        Err(IngestError::Closed)
    ));
    let iface = Interface::<Maplet, NoopRawMutex, (), _, 1, 2, 32, 2>::new((), [0; 32]);
    iface.bus().ingest(2, &[0x10, 0, 7]).await.unwrap();
    iface.close();
    assert_eq!(
        iface
            .bus()
            .__recv_next_capnp_from::<Message>(2)
            .await
            .unwrap_err()
            .kind,
        ErrorKind::Closed
    );
}
