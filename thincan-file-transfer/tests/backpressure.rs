#![cfg(feature = "tokio")]

use core::time::Duration;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use can_isotp_interface::{IsoTpAsyncEndpoint, RecvControl, RecvError, RecvStatus, SendError};
use tokio::sync::mpsc;

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

#[derive(Debug)]
struct SlowReceiverNode {
    inbox: thincan_file_transfer::AckInbox,
    transfer_id: u32,
    chunk_size: u32,
    chunks_seen: Arc<AtomicUsize>,
    processed_tx: mpsc::UnboundedSender<usize>,
}

impl SlowReceiverNode {
    fn new(
        inbox: thincan_file_transfer::AckInbox,
        transfer_id: u32,
        chunk_size: u32,
        chunks_seen: Arc<AtomicUsize>,
        processed_tx: mpsc::UnboundedSender<usize>,
    ) -> Self {
        Self {
            inbox,
            transfer_id,
            chunk_size,
            chunks_seen,
            processed_tx,
        }
    }
}

impl IsoTpAsyncEndpoint for SlowReceiverNode {
    type Error = ();

    async fn send(
        &mut self,
        payload: &[u8],
        _timeout: Duration,
    ) -> Result<(), SendError<Self::Error>> {
        let raw = thincan::decode_wire(payload).map_err(|_| SendError::Backend(()))?;

        if raw.id == <atlas::FileReq as thincan::Message>::ID {
            // Immediately accept with a fixed chunk size (negotiation result).
            self.inbox.push(thincan_file_transfer::Ack {
                transfer_id: self.transfer_id,
                kind: thincan_file_transfer::schema::FileAckKind::Accept,
                next_offset: 0,
                chunk_size: self.chunk_size,
                error: thincan_file_transfer::schema::FileAckError::None,
            });
            return Ok(());
        }

        if raw.id == <atlas::FileChunk as thincan::Message>::ID {
            let typed =
                thincan::CapnpTyped::<thincan_file_transfer::schema::file_chunk::Owned>::new(
                    raw.body,
                );
            let (transfer_id, offset) = typed
                .with_root(capnp::message::ReaderOptions::default(), |root| {
                    (root.get_transfer_id(), root.get_offset())
                })
                .map_err(|_| SendError::Backend(()))?;

            if transfer_id != self.transfer_id {
                return Err(SendError::Backend(()));
            }

            let idx = (offset / self.chunk_size) as usize;
            self.chunks_seen.fetch_add(1, Ordering::SeqCst);

            // Simulate slow processing for chunk #3 only (receiver-side backpressure).
            if idx == 3 {
                let tx = self.processed_tx.clone();
                tokio::spawn(async move {
                    tokio::time::sleep(Duration::from_secs(60 * 60)).await;
                    let _ = tx.send(idx);
                });
            } else {
                let _ = self.processed_tx.send(idx);
            }
            return Ok(());
        }

        Ok(())
    }

    async fn recv_one<Cb>(
        &mut self,
        _timeout: Duration,
        _on_payload: Cb,
    ) -> Result<RecvStatus, RecvError<Self::Error>>
    where
        Cb: FnMut(&[u8]) -> Result<RecvControl, Self::Error>,
    {
        Ok(RecvStatus::TimedOut)
    }
}

#[tokio::test(start_paused = true)]
async fn async_sender_backpressures_when_receiver_is_slow() {
    let (inbox, mut acks) = thincan_file_transfer::ack_channel();

    let transfer_id = 1u32;
    let chunk_size = 8u32;
    let total_len = chunk_size * 10;
    let bytes = vec![0xA5u8; total_len as usize];

    let chunks_seen = Arc::new(AtomicUsize::new(0));
    let acked_chunks = Arc::new(AtomicUsize::new(0));
    let (processed_tx, mut processed_rx) = mpsc::unbounded_channel::<usize>();

    // Ack manager: emits strictly cumulative acks, and only advances when chunks are "processed".
    let inbox_for_mgr = inbox.clone();
    let acked_for_mgr = acked_chunks.clone();
    tokio::spawn(async move {
        let total_chunks = (total_len / chunk_size) as usize;
        let mut processed = vec![false; total_chunks];
        let mut next_idx = 0usize;

        while let Some(i) = processed_rx.recv().await {
            if i < total_chunks {
                processed[i] = true;
            }
            while next_idx < total_chunks && processed[next_idx] {
                next_idx += 1;
                acked_for_mgr.store(next_idx, Ordering::SeqCst);
                let next_offset = (next_idx as u32) * chunk_size;
                let kind = if next_idx == total_chunks {
                    thincan_file_transfer::schema::FileAckKind::Complete
                } else {
                    thincan_file_transfer::schema::FileAckKind::Ack
                };
                inbox_for_mgr.push(thincan_file_transfer::Ack {
                    transfer_id,
                    kind,
                    next_offset,
                    chunk_size: 0,
                    error: thincan_file_transfer::schema::FileAckError::None,
                });
            }
            if next_idx == total_chunks {
                break;
            }
        }
    });

    let node = SlowReceiverNode::new(
        inbox,
        transfer_id,
        chunk_size,
        chunks_seen.clone(),
        processed_tx,
    );

    let mut tx_buf = [0u8; 256];
    let mut iface = thincan::Interface::new(node, (), &mut tx_buf);

    let mut sender = thincan_file_transfer::AsyncSender::<atlas::Atlas>::new();
    sender
        .set_sender_max_chunk_size(chunk_size as usize)
        .unwrap();
    sender.set_window_chunks(4).unwrap();
    let window_chunks = sender.window_chunks();

    let send_timeout = Duration::from_secs(2 * 60 * 60);
    let send_fut =
        sender.send_file_with_id(&mut iface, &mut acks, transfer_id, &bytes, send_timeout);

    // Let the task run until it reaches the stall: chunk #3 takes a long time to "process", so
    // cumulative acks stop advancing at 3 chunks.
    let monitor_fut = async move {
        loop {
            tokio::task::yield_now().await;
            let acked = acked_chunks.load(Ordering::SeqCst);
            let seen = chunks_seen.load(Ordering::SeqCst);
            if acked >= 3 && seen >= acked + window_chunks {
                break;
            }
        }

        let acked_before = acked_chunks.load(Ordering::SeqCst);
        let seen_before = chunks_seen.load(Ordering::SeqCst);
        assert_eq!(acked_before, 3);
        assert_eq!(seen_before, acked_before + window_chunks);

        // Advance time to *just before* the slow chunk finishes; sender should remain stalled and must
        // not send more than `window_chunks` ahead of the last acked offset.
        tokio::time::advance(Duration::from_secs(60 * 60 - 1)).await;
        tokio::task::yield_now().await;
        assert_eq!(acked_chunks.load(Ordering::SeqCst), acked_before);
        assert_eq!(chunks_seen.load(Ordering::SeqCst), seen_before);

        // Now let the slow chunk complete so the transfer can finish.
        tokio::time::advance(Duration::from_secs(1)).await;
    };

    let (out, _) = tokio::join!(send_fut, monitor_fut);
    let out = out.unwrap();
    assert_eq!(out.transfer_id, transfer_id);
    assert_eq!(out.total_len, bytes.len());
    assert_eq!(out.chunk_size, chunk_size as usize);
}
