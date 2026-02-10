#![cfg(feature = "alloc")]

use std::time::Duration;

use thincan::Encode as _;
use thincan::Message;

#[derive(Clone, Copy)]
struct FileReq;
#[derive(Clone, Copy)]
struct FileChunk;
#[derive(Clone, Copy)]
struct FileAck;

impl thincan::Message for FileReq {
    const ID: u16 = 0x2001;
}
impl thincan::Message for FileChunk {
    const ID: u16 = 0x2002;
}
impl thincan::Message for FileAck {
    const ID: u16 = 0x2003;
}

struct DemoAtlas;
impl thincan_file_transfer::Atlas for DemoAtlas {
    type FileReq = FileReq;
    type FileChunk = FileChunk;
    type FileAck = FileAck;
}

#[derive(Debug, Default)]
struct Store {
    begin_calls: usize,
    write_calls: usize,
    commit_calls: usize,
    abort_calls: usize,
}

impl thincan_file_transfer::FileStore for Store {
    type Error = ();
    type WriteHandle = Vec<u8>;

    fn begin_write(
        &mut self,
        _transfer_id: u32,
        total_len: u32,
    ) -> Result<Self::WriteHandle, Self::Error> {
        self.begin_calls += 1;
        Ok(vec![0u8; total_len as usize])
    }

    fn write_at(
        &mut self,
        handle: &mut Self::WriteHandle,
        offset: u32,
        bytes: &[u8],
    ) -> Result<(), Self::Error> {
        self.write_calls += 1;
        let start = offset as usize;
        let end = start + bytes.len();
        handle[start..end].copy_from_slice(bytes);
        Ok(())
    }

    fn commit(&mut self, _handle: Self::WriteHandle) -> Result<(), Self::Error> {
        self.commit_calls += 1;
        Ok(())
    }

    fn abort(&mut self, _handle: Self::WriteHandle) {
        self.abort_calls += 1;
    }
}

fn encode_req(transfer_id: u32, total_len: u32) -> Vec<u8> {
    let v = thincan_file_transfer::file_req::<DemoAtlas>(transfer_id, total_len);
    let mut buf = vec![0u8; v.max_encoded_len()];
    let n = v.encode(&mut buf).unwrap();
    buf.truncate(n);
    buf
}

fn encode_offer(
    transfer_id: u32,
    total_len: u32,
    sender_max_chunk_size: u32,
    metadata: &[u8],
) -> Vec<u8> {
    let v = thincan_file_transfer::file_offer::<DemoAtlas>(
        transfer_id,
        total_len,
        sender_max_chunk_size,
        metadata,
    );
    let mut buf = vec![0u8; v.max_encoded_len()];
    let n = v.encode(&mut buf).unwrap();
    buf.truncate(n);
    buf
}

fn encode_chunk(transfer_id: u32, offset: u32, bytes: &[u8]) -> Vec<u8> {
    let v = thincan_file_transfer::file_chunk::<DemoAtlas>(transfer_id, offset, bytes);
    let mut buf = vec![0u8; v.max_encoded_len()];
    let n = v.encode(&mut buf).unwrap();
    buf.truncate(n);
    buf
}

#[test]
fn file_req_aborts_previous_in_progress_transfer() {
    let mut state = thincan_file_transfer::State::new(Store::default());

    let r1 = encode_req(1, 4);
    let m1 = thincan::CapnpTyped::<thincan_file_transfer::schema::file_req::Owned>::new(&r1[..]);
    thincan_file_transfer::handle_file_req(&mut state, m1).unwrap();

    let r2 = encode_req(2, 4);
    let m2 = thincan::CapnpTyped::<thincan_file_transfer::schema::file_req::Owned>::new(&r2[..]);
    thincan_file_transfer::handle_file_req(&mut state, m2).unwrap();

    assert_eq!(state.store.begin_calls, 2);
    assert_eq!(state.store.abort_calls, 1);
}

#[test]
fn file_chunk_errors_without_in_progress_state() {
    let mut state = thincan_file_transfer::State::new(Store::default());
    let c = encode_chunk(1, 0, b"abc");
    let msg = thincan::CapnpTyped::<thincan_file_transfer::schema::file_chunk::Owned>::new(&c[..]);
    let err = thincan_file_transfer::handle_file_chunk(&mut state, msg).unwrap_err();
    assert!(matches!(err, thincan_file_transfer::Error::Protocol));
}

#[test]
fn file_chunk_errors_on_transfer_id_mismatch() {
    let mut state = thincan_file_transfer::State::new(Store::default());
    let r = encode_req(1, 4);
    let req = thincan::CapnpTyped::<thincan_file_transfer::schema::file_req::Owned>::new(&r[..]);
    thincan_file_transfer::handle_file_req(&mut state, req).unwrap();

    let c = encode_chunk(2, 0, b"abc");
    let chunk =
        thincan::CapnpTyped::<thincan_file_transfer::schema::file_chunk::Owned>::new(&c[..]);
    let err = thincan_file_transfer::handle_file_chunk(&mut state, chunk).unwrap_err();
    assert!(matches!(err, thincan_file_transfer::Error::Protocol));
}

#[test]
fn file_chunk_offset_past_end_is_noop() {
    let mut state = thincan_file_transfer::State::new(Store::default());
    let r = encode_req(1, 4);
    let req = thincan::CapnpTyped::<thincan_file_transfer::schema::file_req::Owned>::new(&r[..]);
    thincan_file_transfer::handle_file_req(&mut state, req).unwrap();

    let c = encode_chunk(1, 4, b"abc");
    let chunk =
        thincan::CapnpTyped::<thincan_file_transfer::schema::file_chunk::Owned>::new(&c[..]);
    thincan_file_transfer::handle_file_chunk(&mut state, chunk).unwrap();

    assert_eq!(state.store.write_calls, 0);
    assert_eq!(state.store.commit_calls, 0);
}

#[test]
fn file_chunk_does_not_commit_until_all_ranges_received() {
    let mut state = thincan_file_transfer::State::new(Store::default());
    let r = encode_req(1, 8);
    let req = thincan::CapnpTyped::<thincan_file_transfer::schema::file_req::Owned>::new(&r[..]);
    thincan_file_transfer::handle_file_req(&mut state, req).unwrap();

    // Send the last chunk first: previously this would incorrectly trigger commit.
    let c2 = encode_chunk(1, 4, b"EFGH");
    let chunk2 =
        thincan::CapnpTyped::<thincan_file_transfer::schema::file_chunk::Owned>::new(&c2[..]);
    thincan_file_transfer::handle_file_chunk(&mut state, chunk2).unwrap();
    assert_eq!(state.store.commit_calls, 0);

    // Now send the first chunk to complete the full range.
    let c1 = encode_chunk(1, 0, b"ABCD");
    let chunk1 =
        thincan::CapnpTyped::<thincan_file_transfer::schema::file_chunk::Owned>::new(&c1[..]);
    thincan_file_transfer::handle_file_chunk(&mut state, chunk1).unwrap();
    assert_eq!(state.store.commit_calls, 1);
}

#[test]
fn receiver_emits_accept_and_cumulative_ack() {
    let mut state = thincan_file_transfer::State::new(Store::default());
    state.set_config(thincan_file_transfer::ReceiverConfig {
        max_chunk_size: 8,
    });

    let metadata = b"opaque-metadata";
    let r = encode_offer(7, 8, 64, metadata);
    let req = thincan::CapnpTyped::<thincan_file_transfer::schema::file_req::Owned>::new(&r[..]);
    thincan_file_transfer::handle_file_req(&mut state, req).unwrap();

    let accept = state.take_pending_ack().expect("expected accept ack");
    assert_eq!(accept.transfer_id, 7);
    assert_eq!(accept.kind, thincan_file_transfer::schema::FileAckKind::Accept);
    assert_eq!(accept.chunk_size, 8);
    assert_eq!(state.in_progress_metadata().unwrap(), metadata);

    // Out-of-order chunk: ack should remain at 0.
    let c2 = encode_chunk(7, 4, b"EFGH");
    let chunk2 =
        thincan::CapnpTyped::<thincan_file_transfer::schema::file_chunk::Owned>::new(&c2[..]);
    thincan_file_transfer::handle_file_chunk(&mut state, chunk2).unwrap();
    let ack0 = state.take_pending_ack().expect("expected progress ack");
    assert_eq!(ack0.kind, thincan_file_transfer::schema::FileAckKind::Ack);
    assert_eq!(ack0.next_offset, 0);

    // Now the first chunk: cumulative ack should advance to 8 and commit triggers complete.
    let c1 = encode_chunk(7, 0, b"ABCD");
    let chunk1 =
        thincan::CapnpTyped::<thincan_file_transfer::schema::file_chunk::Owned>::new(&c1[..]);
    thincan_file_transfer::handle_file_chunk(&mut state, chunk1).unwrap();

    // `handle_file_chunk` emits an Ack and then a Complete; we should see the latest (Complete).
    let done = state.take_pending_ack().expect("expected complete ack");
    assert_eq!(done.kind, thincan_file_transfer::schema::FileAckKind::Complete);
    assert_eq!(done.next_offset, 8);
}

#[test]
fn receiver_rejects_oversize_chunks_when_configured() {
    let mut state = thincan_file_transfer::State::new(Store::default());
    state.set_config(thincan_file_transfer::ReceiverConfig {
        max_chunk_size: 4,
    });

    let r = encode_offer(1, 8, 64, &[]);
    let req = thincan::CapnpTyped::<thincan_file_transfer::schema::file_req::Owned>::new(&r[..]);
    thincan_file_transfer::handle_file_req(&mut state, req).unwrap();

    let c = encode_chunk(1, 0, b"TOO-LONG");
    let chunk = thincan::CapnpTyped::<thincan_file_transfer::schema::file_chunk::Owned>::new(&c);
    let err = thincan_file_transfer::handle_file_chunk(&mut state, chunk).unwrap_err();
    assert!(matches!(err, thincan_file_transfer::Error::Protocol));
}

#[test]
fn encode_rejects_too_small_output_buffer() {
    let req = thincan_file_transfer::file_req::<DemoAtlas>(1, 10);
    let mut out = [0u8; 1];
    assert!(req.encode(&mut out).is_err());

    let chunk = thincan_file_transfer::file_chunk::<DemoAtlas>(1, 0, b"hello");
    let mut out = [0u8; 1];
    assert!(chunk.encode(&mut out).is_err());
}

#[test]
fn bundle_dispatch_handles_ack_and_maps_errors() {
    struct Handlers {
        state: thincan_file_transfer::State<Store>,
    }
    impl thincan_file_transfer::Deps for Handlers {
        type Store = Store;
        fn state(&mut self) -> &mut thincan_file_transfer::State<Self::Store> {
            &mut self.state
        }
    }

    let mut handlers = Handlers {
        state: thincan_file_transfer::State::new(Store::default()),
    };

    // BundleMeta is_known for known ids.
    assert!(<thincan_file_transfer::Bundle as thincan::BundleMeta<
        DemoAtlas,
    >>::is_known(
        <DemoAtlas as thincan_file_transfer::Atlas>::FileReq::ID
    ));
    assert!(!<thincan_file_transfer::Bundle as thincan::BundleMeta<
        DemoAtlas,
    >>::is_known(0xFFFF));

    // Ack path: always Ok even with empty body.
    let ack = thincan::RawMessage {
        id: <DemoAtlas as thincan_file_transfer::Atlas>::FileAck::ID,
        body: &[],
    };
    assert!(matches!(
        <thincan_file_transfer::Bundle as thincan::BundleDispatch<DemoAtlas, _>>::try_dispatch(
            &thincan::DefaultParser,
            &mut handlers,
            ack
        ),
        Some(Ok(()))
    ));

    // Chunk path with no request: maps protocol error to thincan::ErrorKind::Other.
    let c = encode_chunk(1, 0, b"abc");
    let chunk = thincan::RawMessage {
        id: <DemoAtlas as thincan_file_transfer::Atlas>::FileChunk::ID,
        body: &c[..],
    };
    let err =
        <thincan_file_transfer::Bundle as thincan::BundleDispatch<DemoAtlas, _>>::try_dispatch(
            &thincan::DefaultParser,
            &mut handlers,
            chunk,
        )
        .unwrap()
        .unwrap_err();
    assert!(matches!(err.kind, thincan::ErrorKind::Other));

    // FileReq path mapping errors: failing begin_write should map to ErrorKind::Other.
    #[derive(Default)]
    struct FailingStore;
    impl thincan_file_transfer::FileStore for FailingStore {
        type Error = ();
        type WriteHandle = Vec<u8>;
        fn begin_write(
            &mut self,
            _transfer_id: u32,
            _total_len: u32,
        ) -> Result<Self::WriteHandle, Self::Error> {
            Err(())
        }
        fn write_at(
            &mut self,
            _handle: &mut Self::WriteHandle,
            _offset: u32,
            _bytes: &[u8],
        ) -> Result<(), Self::Error> {
            Ok(())
        }
        fn commit(&mut self, _handle: Self::WriteHandle) -> Result<(), Self::Error> {
            Ok(())
        }
        fn abort(&mut self, _handle: Self::WriteHandle) {}
    }

    struct FailingHandlers {
        state: thincan_file_transfer::State<FailingStore>,
    }
    impl thincan_file_transfer::Deps for FailingHandlers {
        type Store = FailingStore;
        fn state(&mut self) -> &mut thincan_file_transfer::State<Self::Store> {
            &mut self.state
        }
    }

    let mut failing = FailingHandlers {
        state: thincan_file_transfer::State::new(FailingStore),
    };
    let req_bytes = encode_req(1, 4);
    let req_msg = thincan::RawMessage {
        id: <DemoAtlas as thincan_file_transfer::Atlas>::FileReq::ID,
        body: &req_bytes[..],
    };
    let err =
        <thincan_file_transfer::Bundle as thincan::BundleDispatch<DemoAtlas, _>>::try_dispatch(
            &thincan::DefaultParser,
            &mut failing,
            req_msg,
        )
        .unwrap()
        .unwrap_err();
    assert!(matches!(err.kind, thincan::ErrorKind::Other));

    // Cover SendEncoded impl for thincan::Interface.
    struct Sink;
    impl thincan::Transport for Sink {
        fn send(&mut self, _payload: &[u8], _timeout: Duration) -> Result<(), thincan::Error> {
            Ok(())
        }
        fn recv_one<F>(
            &mut self,
            _timeout: Duration,
            _on_payload: F,
        ) -> Result<thincan::RecvStatus, thincan::Error>
        where
            F: FnMut(&[u8]) -> Result<thincan::RecvControl, thincan::Error>,
        {
            Ok(thincan::RecvStatus::TimedOut)
        }
    }
    let mut tx = [0u8; 512];
    let mut iface = thincan::Interface::new(Sink, (), &mut tx);
    let v = thincan_file_transfer::file_req::<DemoAtlas>(1, 4);
    thincan_file_transfer::SendEncoded::send_encoded::<FileReq, _>(
        &mut iface,
        &v,
        Duration::from_millis(1),
    )
    .unwrap();

    // Sender API quick smoke for chunk_size getters.
    let sender = thincan_file_transfer::Sender::<DemoAtlas>::default();
    assert!(sender.chunk_size() > 0);
    let _ = Duration::from_millis(1);
}
