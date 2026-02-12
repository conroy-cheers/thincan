#![cfg(feature = "tokio")]

use core::time::Duration;

use can_isotp_interface::{IsoTpAsyncEndpoint, RecvControl, RecvError, RecvStatus, SendError};

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
struct DummyNode {
    inbox: thincan_file_transfer::AckInbox,
    transfer_id: u32,
    total_len: u32,
    accept_chunk_size: u32,
    chunks_seen: u32,
}

impl IsoTpAsyncEndpoint for DummyNode {
    type Error = ();

    async fn send(
        &mut self,
        payload: &[u8],
        _timeout: Duration,
    ) -> Result<(), SendError<Self::Error>> {
        if payload.len() < 2 {
            return Ok(());
        }
        let id = u16::from_le_bytes([payload[0], payload[1]]);
        if id == <atlas::FileReq as thincan::Message>::ID {
            self.inbox.push(thincan_file_transfer::Ack {
                transfer_id: self.transfer_id,
                kind: thincan_file_transfer::schema::FileAckKind::Accept,
                next_offset: 0,
                chunk_size: self.accept_chunk_size,
                error: thincan_file_transfer::schema::FileAckError::None,
            });
        } else if id == <atlas::FileChunk as thincan::Message>::ID {
            self.chunks_seen += 1;
            let next_offset = match self.chunks_seen {
                1 => self.accept_chunk_size,
                2 => self.accept_chunk_size.saturating_mul(2),
                _ => self.total_len,
            }
            .min(self.total_len);
            let kind = if next_offset == self.total_len {
                thincan_file_transfer::schema::FileAckKind::Complete
            } else {
                thincan_file_transfer::schema::FileAckKind::Ack
            };
            self.inbox.push(thincan_file_transfer::Ack {
                transfer_id: self.transfer_id,
                kind,
                next_offset,
                chunk_size: 0,
                error: thincan_file_transfer::schema::FileAckError::None,
            });
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

#[tokio::test]
async fn async_sender_completes_with_accept_and_progress_acks() {
    let (inbox, mut acks) = thincan_file_transfer::ack_channel();

    let transfer_id = 1u32;
    let total_len = 20u32;
    let accept_chunk_size = 8u32;
    let node = DummyNode {
        inbox,
        transfer_id,
        total_len,
        accept_chunk_size,
        chunks_seen: 0,
    };

    let mut tx_buf = [0u8; 256];
    let mut iface = thincan::Interface::new(node, (), &mut tx_buf);

    let mut sender = thincan_file_transfer::AsyncSender::<atlas::Atlas>::new();
    let bytes = [0xABu8; 20];
    let out = sender
        .send_file_with_id(
            &mut iface,
            &mut acks,
            transfer_id,
            &bytes,
            Duration::from_millis(50),
        )
        .await
        .unwrap();

    assert_eq!(out.transfer_id, transfer_id);
    assert_eq!(out.total_len, 20);
    assert_eq!(out.chunk_size, accept_chunk_size as usize);
    assert_eq!(out.chunks_sent, 3);
    assert_eq!(out.retries, 0);
}
