#![cfg(all(feature = "embassy", feature = "tokio"))]

use core::time::Duration;

use embassy_sync::blocking_mutex::raw::NoopRawMutex;
use embassy_sync::channel::Channel;

use thincan_file_transfer::{AsyncFileStore, PendingAck, ReceiverConfig};

thincan::bus_atlas! {
    pub mod atlas {
        0x1001 => FileReq(capnp = thincan_file_transfer::schema::file_req::Owned);
        0x1002 => FileChunk(capnp = thincan_file_transfer::schema::file_chunk::Owned);
        0x1003 => FileAck(capnp = thincan_file_transfer::schema::file_ack::Owned);
    }
}

impl thincan_file_transfer::Atlas for atlas::Atlas {
    type FileReq = atlas::FileReq;
    type FileChunk = atlas::FileChunk;
    type FileAck = atlas::FileAck;
}

thincan_file_transfer::file_transfer_bundle! {
    pub mod ft_bundle(atlas) {}
}

thincan::bus_maplet! {
    pub mod ft_maplet: atlas {
        reply_to: u8;
        bundles [ft_bundle];
        parser: thincan::DefaultParser;
        use msgs [FileReq, FileChunk, FileAck];
        handles {}
        unhandled_by_default = true;
        ignore [];
    }
}

#[derive(Default)]
struct NoApp;

impl<'a> ft_maplet::Handlers<'a> for NoApp {
    type Error = core::convert::Infallible;
}

#[derive(Debug, Default)]
struct MemStore {
    slow_offset: Option<u32>,
}

impl AsyncFileStore for MemStore {
    type Error = ();
    type WriteHandle = Vec<u8>;

    async fn begin_write(
        &mut self,
        _transfer_id: u32,
        total_len: u32,
    ) -> Result<Self::WriteHandle, Self::Error> {
        Ok(vec![0u8; total_len as usize])
    }

    async fn write_at(
        &mut self,
        handle: &mut Self::WriteHandle,
        offset: u32,
        bytes: &[u8],
    ) -> Result<(), Self::Error> {
        if self.slow_offset == Some(offset) {
            tokio::time::sleep(Duration::from_secs(60 * 60)).await;
        }
        let start = offset as usize;
        let end = start + bytes.len();
        handle[start..end].copy_from_slice(bytes);
        Ok(())
    }

    async fn commit(&mut self, _handle: Self::WriteHandle) -> Result<(), Self::Error> {
        Ok(())
    }

    async fn abort(&mut self, _handle: Self::WriteHandle) {}
}

fn encode_offer_into<'b>(
    iface: &mut thincan::Interface<(), (), &'b mut [u8]>,
    transfer_id: u32,
    total_len: u32,
    sender_max_chunk_size: u32,
    metadata: &[u8],
) -> Vec<u8> {
    let payload = iface
        .encode_value_into::<atlas::FileReq, _>(&thincan_file_transfer::file_offer::<atlas::Atlas>(
            transfer_id,
            total_len,
            sender_max_chunk_size,
            metadata,
        ))
        .unwrap();
    payload.to_vec()
}

fn encode_chunk_into<'b>(
    iface: &mut thincan::Interface<(), (), &'b mut [u8]>,
    transfer_id: u32,
    offset: u32,
    data: &[u8],
) -> Vec<u8> {
    let payload = iface
        .encode_value_into::<atlas::FileChunk, _>(
            &thincan_file_transfer::file_chunk::<atlas::Atlas>(transfer_id, offset, data),
        )
        .unwrap();
    payload.to_vec()
}

#[tokio::test]
async fn receiver_rejects_oversize_metadata() {
    type ReplyTo = u8;
    const MAX_METADATA: usize = 4;
    const MAX_CHUNK: usize = 16;
    const IN_DEPTH: usize = 8;
    const ACK_DEPTH: usize = 8;

    let inbound: thincan_file_transfer::InboundQueue<
        NoopRawMutex,
        ReplyTo,
        MAX_METADATA,
        MAX_CHUNK,
        IN_DEPTH,
    > = Channel::new();
    let acks: thincan_file_transfer::OutboundAckQueue<NoopRawMutex, ReplyTo, ACK_DEPTH> =
        Channel::new();

    let ingestor = thincan_file_transfer::ReceiverIngress::<
        atlas::Atlas,
        NoopRawMutex,
        ReplyTo,
        MAX_METADATA,
        MAX_CHUNK,
        IN_DEPTH,
    >::new(&inbound);

    let mut receiver = thincan_file_transfer::ReceiverNoAlloc::<
        atlas::Atlas,
        MemStore,
        NoopRawMutex,
        ReplyTo,
        MAX_METADATA,
        8,
        MAX_CHUNK,
        IN_DEPTH,
        ACK_DEPTH,
    >::new(MemStore::default(), &inbound, &acks);

    let mut tx_buf = [0u8; 512];
    let mut enc = thincan::Interface::new((), (), tx_buf.as_mut_slice());

    let transfer_id = 1u32;
    let reply_to = 7u8;
    let payload = encode_offer_into(&mut enc, transfer_id, 8, MAX_CHUNK as u32, b"toolong");
    let raw = thincan::decode_wire(&payload).unwrap();
    assert_eq!(raw.id, <atlas::FileReq as thincan::Message>::ID);
    assert!(ingestor.try_ingest_thincan_raw(reply_to, raw).unwrap());

    receiver.process_one().await.unwrap();
    let got = acks.receive().await;

    assert_eq!(got.reply_to, reply_to);
    assert_eq!(
        got.ack,
        PendingAck {
            transfer_id,
            kind: thincan_file_transfer::schema::FileAckKind::Reject,
            next_offset: 0,
            chunk_size: 0,
            error: thincan_file_transfer::schema::FileAckError::InvalidRequest,
        }
    );
}

#[tokio::test]
async fn receiver_cumulative_ack_only_advances_when_contiguous() {
    type ReplyTo = u8;
    const MAX_METADATA: usize = 16;
    const MAX_CHUNK: usize = 8;
    const IN_DEPTH: usize = 8;
    const ACK_DEPTH: usize = 8;

    let inbound: thincan_file_transfer::InboundQueue<
        NoopRawMutex,
        ReplyTo,
        MAX_METADATA,
        MAX_CHUNK,
        IN_DEPTH,
    > = Channel::new();
    let acks: thincan_file_transfer::OutboundAckQueue<NoopRawMutex, ReplyTo, ACK_DEPTH> =
        Channel::new();

    let ingestor = thincan_file_transfer::ReceiverIngress::<
        atlas::Atlas,
        NoopRawMutex,
        ReplyTo,
        MAX_METADATA,
        MAX_CHUNK,
        IN_DEPTH,
    >::new(&inbound);

    let mut receiver = thincan_file_transfer::ReceiverNoAlloc::<
        atlas::Atlas,
        MemStore,
        NoopRawMutex,
        ReplyTo,
        MAX_METADATA,
        8,
        MAX_CHUNK,
        IN_DEPTH,
        ACK_DEPTH,
    >::new(MemStore::default(), &inbound, &acks);

    receiver.set_config(thincan_file_transfer::ReceiverNoAllocConfig {
        receiver: ReceiverConfig {
            max_chunk_size: MAX_CHUNK as u32,
        },
    });

    let mut tx_buf = [0u8; 512];
    let mut enc = thincan::Interface::new((), (), tx_buf.as_mut_slice());

    let transfer_id = 1u32;
    let reply_to = 3u8;

    // Req -> Accept.
    let payload = encode_offer_into(&mut enc, transfer_id, 16, MAX_CHUNK as u32, b"meta");
    let raw = thincan::decode_wire(&payload).unwrap();
    ingestor.try_ingest_thincan_raw(reply_to, raw).unwrap();
    receiver.process_one().await.unwrap();
    let accept = acks.receive().await;
    assert_eq!(
        accept.ack.kind,
        thincan_file_transfer::schema::FileAckKind::Accept
    );

    // Chunk at offset=8 arrives first; cumulative ack must remain at 0.
    let payload = encode_chunk_into(&mut enc, transfer_id, 8, b"ABCDEFGH");
    let raw = thincan::decode_wire(&payload).unwrap();
    ingestor.try_ingest_thincan_raw(reply_to, raw).unwrap();
    receiver.process_one().await.unwrap();
    let ack1 = acks.receive().await;
    assert_eq!(
        ack1.ack.kind,
        thincan_file_transfer::schema::FileAckKind::Ack
    );
    assert_eq!(ack1.ack.next_offset, 0);

    // Now offset=0 arrives; receiver can complete.
    let payload = encode_chunk_into(&mut enc, transfer_id, 0, b"abcdefgh");
    let raw = thincan::decode_wire(&payload).unwrap();
    ingestor.try_ingest_thincan_raw(reply_to, raw).unwrap();
    receiver.process_one().await.unwrap();
    let done = acks.receive().await;
    assert_eq!(
        done.ack.kind,
        thincan_file_transfer::schema::FileAckKind::Complete
    );
    assert_eq!(done.ack.next_offset, 16);
}

#[tokio::test(start_paused = true)]
async fn receiver_delays_ack_until_slow_write_completes() {
    type ReplyTo = u8;
    const MAX_METADATA: usize = 16;
    const MAX_CHUNK: usize = 8;
    const IN_DEPTH: usize = 8;
    const ACK_DEPTH: usize = 8;

    let inbound: thincan_file_transfer::InboundQueue<
        NoopRawMutex,
        ReplyTo,
        MAX_METADATA,
        MAX_CHUNK,
        IN_DEPTH,
    > = Channel::new();
    let acks: thincan_file_transfer::OutboundAckQueue<NoopRawMutex, ReplyTo, ACK_DEPTH> =
        Channel::new();

    let ingestor = thincan_file_transfer::ReceiverIngress::<
        atlas::Atlas,
        NoopRawMutex,
        ReplyTo,
        MAX_METADATA,
        MAX_CHUNK,
        IN_DEPTH,
    >::new(&inbound);

    let mut store = MemStore::default();
    store.slow_offset = Some(24);

    let receiver = thincan_file_transfer::ReceiverNoAlloc::<
        atlas::Atlas,
        MemStore,
        NoopRawMutex,
        ReplyTo,
        MAX_METADATA,
        16,
        MAX_CHUNK,
        IN_DEPTH,
        ACK_DEPTH,
    >::new(store, &inbound, &acks);

    let mut tx_buf = [0u8; 512];
    let mut enc = thincan::Interface::new((), (), tx_buf.as_mut_slice());

    let transfer_id = 1u32;
    let reply_to = 1u8;

    // Req -> Accept (process in the same task).
    let payload = encode_offer_into(&mut enc, transfer_id, 64, MAX_CHUNK as u32, b"");
    let raw = thincan::decode_wire(&payload).unwrap();
    ingestor.try_ingest_thincan_raw(reply_to, raw).unwrap();

    let mut receiver = receiver;
    receiver.process_one().await.unwrap();
    let _ = acks.receive().await;

    // Fast chunks 0..2.
    for (i, data) in [b"00000000", b"11111111", b"22222222"]
        .into_iter()
        .enumerate()
    {
        let payload = encode_chunk_into(&mut enc, transfer_id, (i as u32) * 8, data);
        let raw = thincan::decode_wire(&payload).unwrap();
        ingestor.try_ingest_thincan_raw(reply_to, raw).unwrap();
        receiver.process_one().await.unwrap();
        let _ = acks.receive().await;
    }

    // Chunk #3 is slow; ack should not be emitted until time advances.
    let payload = encode_chunk_into(&mut enc, transfer_id, 24, b"33333333");
    let raw = thincan::decode_wire(&payload).unwrap();
    ingestor.try_ingest_thincan_raw(reply_to, raw).unwrap();

    let mut process_fut = core::pin::pin!(receiver.process_one());
    tokio::select! {
        res = &mut process_fut => {
            res.unwrap();
            panic!("process_one unexpectedly completed without advancing time");
        }
        _ = tokio::task::yield_now() => {}
    }

    assert!(
        tokio::time::timeout(Duration::ZERO, acks.receive())
            .await
            .is_err()
    );

    tokio::time::advance(Duration::from_secs(60 * 60)).await;
    let (got, _) = tokio::join!(acks.receive(), async {
        (&mut process_fut).await.unwrap();
    });
    assert_eq!(
        got.ack.kind,
        thincan_file_transfer::schema::FileAckKind::Ack
    );
    assert_eq!(got.ack.next_offset, 32);
}

#[tokio::test]
async fn ingress_returns_err_when_queue_is_full() {
    type ReplyTo = u8;
    const MAX_METADATA: usize = 16;
    const MAX_CHUNK: usize = 8;
    const IN_DEPTH: usize = 1;

    let inbound: thincan_file_transfer::InboundQueue<
        NoopRawMutex,
        ReplyTo,
        MAX_METADATA,
        MAX_CHUNK,
        IN_DEPTH,
    > = Channel::new();

    let ingestor = thincan_file_transfer::ReceiverIngress::<
        atlas::Atlas,
        NoopRawMutex,
        ReplyTo,
        MAX_METADATA,
        MAX_CHUNK,
        IN_DEPTH,
    >::new(&inbound);

    let mut tx_buf = [0u8; 512];
    let mut enc = thincan::Interface::new((), (), tx_buf.as_mut_slice());

    let transfer_id = 1u32;
    let reply_to = 1u8;

    let payload = encode_offer_into(&mut enc, transfer_id, 8, MAX_CHUNK as u32, b"");
    let raw = thincan::decode_wire(&payload).unwrap();
    assert!(ingestor.try_ingest_thincan_raw(reply_to, raw).unwrap());

    let payload = encode_offer_into(&mut enc, transfer_id, 8, MAX_CHUNK as u32, b"");
    let raw = thincan::decode_wire(&payload).unwrap();
    assert!(ingestor.try_ingest_thincan_raw(reply_to, raw).is_err());
}

#[tokio::test]
async fn bundle_ingress_backpressures_when_queue_is_full() {
    use thincan::RouterDispatch;

    type ReplyTo = u8;
    const MAX_METADATA: usize = 16;
    const MAX_CHUNK: usize = 8;
    const IN_DEPTH: usize = 1;

    let inbound: thincan_file_transfer::InboundQueue<
        NoopRawMutex,
        ReplyTo,
        MAX_METADATA,
        MAX_CHUNK,
        IN_DEPTH,
    > = Channel::new();

    let mut router = ft_maplet::Router::new();
    let mut handlers = ft_maplet::HandlersImpl {
        app: NoApp,
        ft_bundle: ft_bundle::ReceiverIngressHandlers::new(&inbound),
    };

    let mut tx_buf = [0u8; 512];
    let mut enc = thincan::Interface::new((), (), tx_buf.as_mut_slice());

    let transfer_id = 1u32;
    let reply_to = 1u8;
    let meta = thincan::RecvMeta { reply_to };

    // Fill the inbound queue (depth=1).
    let payload = encode_offer_into(&mut enc, transfer_id, 8, MAX_CHUNK as u32, b"meta");
    let raw = thincan::decode_wire(&payload).unwrap();
    assert_eq!(
        router.dispatch(&mut handlers, meta, raw).await.unwrap(),
        thincan::DispatchOutcome::Handled
    );

    // Second dispatch must await on inbound queue send (backpressure).
    let payload2 = encode_offer_into(&mut enc, transfer_id, 8, MAX_CHUNK as u32, b"meta");
    let raw2 = thincan::decode_wire(&payload2).unwrap();

    let mut dispatch_fut = core::pin::pin!(router.dispatch(&mut handlers, meta, raw2));
    tokio::select! {
        r = &mut dispatch_fut => {
            r.unwrap();
            panic!("dispatch unexpectedly completed while inbound queue was full");
        }
        _ = tokio::task::yield_now() => {}
    }

    // Drain one event; this should unblock the pending dispatch.
    let _first = inbound.receive().await;
    let _ = dispatch_fut.await.unwrap();

    // And the second event should now be enqueued.
    let _second = inbound.receive().await;
}
