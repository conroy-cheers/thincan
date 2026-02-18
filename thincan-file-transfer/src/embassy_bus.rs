#![cfg(feature = "embassy")]

use core::time::Duration;

use embassy_sync::blocking_mutex::raw::RawMutex;
use embassy_sync::mutex::Mutex;
use embassy_time::Timer;

use crate::{AckSender, AckToSend, AsyncDequeue, file_ack_max_encoded_len};

const COOPERATIVE_RECV_SLICE: Duration = Duration::from_millis(2);

fn cooperative_recv_timeout(timeout: Duration) -> Duration {
    if timeout > COOPERATIVE_RECV_SLICE {
        COOPERATIVE_RECV_SLICE
    } else {
        timeout
    }
}

/// Outbound participant that drains a file-transfer ack queue and transmits each ack.
pub struct AckBusParticipant<'a, A: crate::Atlas, Q> {
    ack_sender: AckSender<'a, A>,
    ack_queue: Q,
}

impl<'a, A, Q> AckBusParticipant<'a, A, Q>
where
    A: crate::Atlas,
{
    pub fn new(
        ack_queue: Q,
        scratch: thincan::capnp::CapnpScratchRef<'a>,
        ack_send_timeout: Duration,
    ) -> Self {
        let mut ack_sender = AckSender::<A>::new(scratch);
        ack_sender.set_send_timeout(ack_send_timeout);
        Self {
            ack_sender,
            ack_queue,
        }
    }
}

impl<Node, Router, TxBuf, A, Q> thincan::BusParticipant<thincan::Interface<Node, Router, TxBuf>>
    for AckBusParticipant<'_, A, Q>
where
    A: crate::Atlas,
    Node: can_isotp_interface::IsoTpAsyncEndpointMeta,
    TxBuf: AsMut<[u8]>,
    <Node as can_isotp_interface::IsoTpAsyncEndpointMeta>::ReplyTo: Copy,
    Q: AsyncDequeue<AckToSend<<Node as can_isotp_interface::IsoTpAsyncEndpointMeta>::ReplyTo>>,
{
    async fn drain_outbound(
        &mut self,
        iface: &mut thincan::Interface<Node, Router, TxBuf>,
    ) -> Result<(), thincan::Error> {
        while let Ok(ack) = self.ack_queue.try_recv() {
            let _ = self.ack_sender.send_one(iface, ack).await;
        }
        Ok(())
    }
}

pub async fn run_bus_task<Node, Router, TxBuf, Handlers, Mtx, const RX: usize>(
    iface: &Mutex<Mtx, thincan::Interface<Node, Router, TxBuf>>,
    handlers: &mut Handlers,
    timeout: Duration,
    rx_buf: &mut [u8; RX],
) -> !
where
    Mtx: RawMutex,
    Node: can_isotp_interface::IsoTpAsyncEndpointMeta
        + can_isotp_interface::IsoTpAsyncEndpointMetaRecvInto<
            ReplyTo = <Node as can_isotp_interface::IsoTpAsyncEndpointMeta>::ReplyTo,
        >,
    Router: thincan::RouterDispatch<
            Handlers,
            <Node as can_isotp_interface::IsoTpAsyncEndpointMeta>::ReplyTo,
        >,
    TxBuf: AsMut<[u8]>,
{
    let recv_timeout = cooperative_recv_timeout(timeout);
    loop {
        let mut guard = iface.lock().await;
        let _ = guard
            .recv_one_dispatch_with_meta_async(handlers, recv_timeout, rx_buf, |_| Ok(()))
            .await;
        Timer::after_millis(0).await;
    }
}

pub async fn run_bus_task_with_acks<Node, Router, TxBuf, Handlers, Mtx, A, Q, const RX: usize>(
    iface: &Mutex<Mtx, thincan::Interface<Node, Router, TxBuf>>,
    handlers: &mut Handlers,
    timeout: Duration,
    rx_buf: &mut [u8; RX],
    ack_sender: &mut AckSender<'_, A>,
    ack_queue: &mut Q,
) -> !
where
    Mtx: RawMutex,
    Node: can_isotp_interface::IsoTpAsyncEndpointMeta
        + can_isotp_interface::IsoTpAsyncEndpointMetaRecvInto<
            ReplyTo = <Node as can_isotp_interface::IsoTpAsyncEndpointMeta>::ReplyTo,
        >,
    Router: thincan::RouterDispatch<
            Handlers,
            <Node as can_isotp_interface::IsoTpAsyncEndpointMeta>::ReplyTo,
        >,
    TxBuf: AsMut<[u8]>,
    A: crate::Atlas,
    Q: AsyncDequeue<AckToSend<<Node as can_isotp_interface::IsoTpAsyncEndpointMeta>::ReplyTo>>,
{
    run_bus_task_with_acks_and_events(
        iface,
        handlers,
        timeout,
        rx_buf,
        ack_sender,
        ack_queue,
        |_| Ok(()),
    )
    .await
}

pub async fn run_bus_task_with_acks_and_events<
    Node,
    Router,
    TxBuf,
    Handlers,
    Mtx,
    A,
    Q,
    Ev,
    const RX: usize,
>(
    iface: &Mutex<Mtx, thincan::Interface<Node, Router, TxBuf>>,
    handlers: &mut Handlers,
    timeout: Duration,
    rx_buf: &mut [u8; RX],
    ack_sender: &mut AckSender<'_, A>,
    ack_queue: &mut Q,
    mut on_event: Ev,
) -> !
where
    Mtx: RawMutex,
    Node: can_isotp_interface::IsoTpAsyncEndpointMeta
        + can_isotp_interface::IsoTpAsyncEndpointMetaRecvInto<
            ReplyTo = <Node as can_isotp_interface::IsoTpAsyncEndpointMeta>::ReplyTo,
        >,
    Router: thincan::RouterDispatch<
            Handlers,
            <Node as can_isotp_interface::IsoTpAsyncEndpointMeta>::ReplyTo,
        >,
    TxBuf: AsMut<[u8]>,
    A: crate::Atlas,
    Q: AsyncDequeue<AckToSend<<Node as can_isotp_interface::IsoTpAsyncEndpointMeta>::ReplyTo>>,
    Ev: for<'a> FnMut(
        thincan::DispatchEventMeta<
            'a,
            <Node as can_isotp_interface::IsoTpAsyncEndpointMeta>::ReplyTo,
        >,
    ) -> Result<(), thincan::Error>,
{
    let recv_timeout = cooperative_recv_timeout(timeout);
    loop {
        while let Ok(ack) = ack_queue.try_recv() {
            let mut guard = iface.lock().await;
            let _ = ack_sender.send_one(&mut *guard, ack).await;
        }

        let mut guard = iface.lock().await;
        let _ = guard
            .recv_one_dispatch_with_meta_async(handlers, recv_timeout, rx_buf, &mut on_event)
            .await;
        Timer::after_millis(0).await;
    }
}

pub async fn run_file_transfer_bus_task<
    Node,
    Router,
    TxBuf,
    Handlers,
    Mtx,
    A,
    const MAX_METADATA: usize,
    const MAX_CHUNK: usize,
    const RX: usize,
>(
    iface: &Mutex<Mtx, thincan::Interface<Node, Router, TxBuf>>,
    handlers: &mut Handlers,
    timeout: Duration,
    rx_buf: &mut [u8; RX],
    ack_send_timeout: Duration,
) -> !
where
    Mtx: RawMutex,
    Node: can_isotp_interface::IsoTpAsyncEndpointMeta
        + can_isotp_interface::IsoTpAsyncEndpointMetaRecvInto<
            ReplyTo = <Node as can_isotp_interface::IsoTpAsyncEndpointMeta>::ReplyTo,
        >,
    Router: thincan::RouterDispatch<
            Handlers,
            <Node as can_isotp_interface::IsoTpAsyncEndpointMeta>::ReplyTo,
        >,
    TxBuf: AsMut<[u8]>,
    A: crate::Atlas,
    Handlers: crate::FileTransferReceiverState<
            <Node as can_isotp_interface::IsoTpAsyncEndpointMeta>::ReplyTo,
            MAX_METADATA,
            MAX_CHUNK,
        >,
    Handlers::AckOutboundQueue:
        AsyncDequeue<AckToSend<<Node as can_isotp_interface::IsoTpAsyncEndpointMeta>::ReplyTo>>,
{
    run_file_transfer_bus_task_with_events::<
        Node,
        Router,
        TxBuf,
        Handlers,
        Mtx,
        A,
        MAX_METADATA,
        MAX_CHUNK,
        _,
        RX,
    >(iface, handlers, timeout, rx_buf, ack_send_timeout, |_| {
        Ok(())
    })
    .await
}

pub async fn run_file_transfer_bus_task_with_events<
    Node,
    Router,
    TxBuf,
    Handlers,
    Mtx,
    A,
    const MAX_METADATA: usize,
    const MAX_CHUNK: usize,
    Ev,
    const RX: usize,
>(
    iface: &Mutex<Mtx, thincan::Interface<Node, Router, TxBuf>>,
    handlers: &mut Handlers,
    timeout: Duration,
    rx_buf: &mut [u8; RX],
    ack_send_timeout: Duration,
    on_event: Ev,
) -> !
where
    Mtx: RawMutex,
    Node: can_isotp_interface::IsoTpAsyncEndpointMeta
        + can_isotp_interface::IsoTpAsyncEndpointMetaRecvInto<
            ReplyTo = <Node as can_isotp_interface::IsoTpAsyncEndpointMeta>::ReplyTo,
        >,
    Router: thincan::RouterDispatch<
            Handlers,
            <Node as can_isotp_interface::IsoTpAsyncEndpointMeta>::ReplyTo,
        >,
    TxBuf: AsMut<[u8]>,
    A: crate::Atlas,
    Handlers: crate::FileTransferReceiverState<
            <Node as can_isotp_interface::IsoTpAsyncEndpointMeta>::ReplyTo,
            MAX_METADATA,
            MAX_CHUNK,
        >,
    Handlers::AckOutboundQueue:
        AsyncDequeue<AckToSend<<Node as can_isotp_interface::IsoTpAsyncEndpointMeta>::ReplyTo>>,
    Ev: for<'a> FnMut(
        thincan::DispatchEventMeta<
            'a,
            <Node as can_isotp_interface::IsoTpAsyncEndpointMeta>::ReplyTo,
        >,
    ) -> Result<(), thincan::Error>,
{
    const ACK_SCRATCH_WORDS: usize =
        crate::capnp_scratch_words_for_bytes(file_ack_max_encoded_len());
    const ACK_SCRATCH_BYTES: usize = ACK_SCRATCH_WORDS * 8;
    let mut ack_scratch_buf = [0u8; ACK_SCRATCH_BYTES];
    let ack_scratch = thincan::capnp::CapnpScratchRef::new(&mut ack_scratch_buf);
    let mut ack_sender = AckSender::<A>::new(ack_scratch);
    ack_sender.set_send_timeout(ack_send_timeout);

    let mut ack_queue = handlers.ack_outbound_queue();
    run_bus_task_with_acks_and_events(
        iface,
        handlers,
        timeout,
        rx_buf,
        &mut ack_sender,
        &mut ack_queue,
        on_event,
    )
    .await
}
