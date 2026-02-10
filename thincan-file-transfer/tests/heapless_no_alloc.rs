#![cfg(feature = "heapless")]

use thincan::RouterDispatch;

thincan::bus_atlas! {
    pub mod atlas {
        0x3001 => FileReq(capnp = thincan_file_transfer::schema::file_req::Owned);
        0x3002 => FileChunk(capnp = thincan_file_transfer::schema::file_chunk::Owned);
        0x3003 => FileAck(capnp = thincan_file_transfer::schema::file_ack::Owned);
    }
}

impl thincan_file_transfer::Atlas for atlas::Atlas {
    type FileReq = atlas::FileReq;
    type FileChunk = atlas::FileChunk;
    type FileAck = atlas::FileAck;
}

thincan::bus_maplet! {
    pub mod maplet: atlas {
        bundles [file_transfer = thincan_file_transfer::Bundle];
        parser: thincan::DefaultParser;
        use msgs [FileReq, FileChunk, FileAck];
        handles {}
        unhandled_by_default = true;
        ignore [];
    }
}

#[derive(Default)]
struct NoApp;

impl<'a> maplet::Handlers<'a> for NoApp {
    type Error = ();
}

struct FixedStore<const N: usize> {
    buf: [u8; N],
    len: usize,
    committed: bool,
    begin_calls: usize,
    write_calls: usize,
    commit_calls: usize,
    abort_calls: usize,
}

impl<const N: usize> Default for FixedStore<N> {
    fn default() -> Self {
        Self {
            buf: [0u8; N],
            len: 0,
            committed: false,
            begin_calls: 0,
            write_calls: 0,
            commit_calls: 0,
            abort_calls: 0,
        }
    }
}

impl<const N: usize> FixedStore<N> {
    fn as_bytes(&self) -> &[u8] {
        &self.buf[..self.len]
    }
}

impl<const N: usize> thincan_file_transfer::FileStore for FixedStore<N> {
    type Error = ();
    type WriteHandle = ();

    fn begin_write(
        &mut self,
        _transfer_id: u32,
        total_len: u32,
    ) -> Result<Self::WriteHandle, Self::Error> {
        self.begin_calls += 1;
        let total_len = total_len as usize;
        if total_len > N {
            return Err(());
        }
        self.len = total_len;
        self.committed = false;
        Ok(())
    }

    fn write_at(
        &mut self,
        _handle: &mut Self::WriteHandle,
        offset: u32,
        bytes: &[u8],
    ) -> Result<(), Self::Error> {
        self.write_calls += 1;
        let start = offset as usize;
        let end = start + bytes.len();
        if end > self.len {
            return Err(());
        }
        self.buf[start..end].copy_from_slice(bytes);
        Ok(())
    }

    fn commit(&mut self, _handle: Self::WriteHandle) -> Result<(), Self::Error> {
        self.commit_calls += 1;
        self.committed = true;
        Ok(())
    }

    fn abort(&mut self, _handle: Self::WriteHandle) {
        self.abort_calls += 1;
        self.committed = false;
    }
}

fn encode_req_with_metadata(
    transfer_id: u32,
    total_len: u32,
    sender_max_chunk_size: u32,
    metadata: &[u8],
) -> Vec<u8> {
    let max_len = thincan_file_transfer::file_offer_max_encoded_len(metadata.len());
    let mut scratch = vec![0u8; max_len];
    let mut message =
        capnp::message::Builder::new(capnp::message::SingleSegmentAllocator::new(&mut scratch));
    let mut root: thincan_file_transfer::schema::file_req::Builder = message.init_root();
    root.set_transfer_id(transfer_id);
    root.set_total_len(total_len);
    root.set_file_metadata(metadata);
    root.set_sender_max_chunk_size(sender_max_chunk_size);
    let segments = message.get_segments_for_output();
    assert_eq!(segments.len(), 1);
    segments[0].to_vec()
}

fn encode_chunk(transfer_id: u32, offset: u32, data: &[u8]) -> Vec<u8> {
    let max_len = thincan_file_transfer::file_chunk_max_encoded_len(data.len());
    let mut scratch = vec![0u8; max_len];
    let mut message =
        capnp::message::Builder::new(capnp::message::SingleSegmentAllocator::new(&mut scratch));
    let mut root: thincan_file_transfer::schema::file_chunk::Builder = message.init_root();
    root.set_transfer_id(transfer_id);
    root.set_offset(offset);
    root.set_data(data);
    let segments = message.get_segments_for_output();
    assert_eq!(segments.len(), 1);
    segments[0].to_vec()
}

#[test]
fn heapless_receiver_rejects_oversize_metadata_without_allocating() {
    type State = thincan_file_transfer::StateNoAlloc<FixedStore<16>, 4, 4>;
    let mut state = State::new(FixedStore::default());

    let metadata = b"12345"; // > MAX_METADATA
    let req_bytes = encode_req_with_metadata(1, 8, 64, metadata);
    let req = thincan::CapnpTyped::<thincan_file_transfer::schema::file_req::Owned>::new(&req_bytes);
    thincan_file_transfer::handle_file_req_no_alloc(&mut state, req).unwrap();

    let ack = state.take_pending_ack().expect("expected reject");
    assert_eq!(ack.transfer_id, 1);
    assert_eq!(ack.kind, thincan_file_transfer::schema::FileAckKind::Reject);
    assert_eq!(ack.error, thincan_file_transfer::schema::FileAckError::InvalidRequest);
    assert_eq!(state.store.begin_calls, 0);
}

#[test]
fn heapless_receiver_tracks_out_of_order_with_bounded_ranges() {
    type State = thincan_file_transfer::StateNoAlloc<FixedStore<16>, 16, 4>;
    let mut state = State::new(FixedStore::default());
    state.set_config(thincan_file_transfer::ReceiverConfig { max_chunk_size: 4 });

    let req_bytes = encode_req_with_metadata(7, 8, 64, b"meta");
    let req = thincan::CapnpTyped::<thincan_file_transfer::schema::file_req::Owned>::new(&req_bytes);
    thincan_file_transfer::handle_file_req_no_alloc(&mut state, req).unwrap();

    let accept = state.take_pending_ack().expect("expected accept");
    assert_eq!(accept.kind, thincan_file_transfer::schema::FileAckKind::Accept);
    assert_eq!(accept.chunk_size, 4);
    assert_eq!(state.in_progress_metadata().unwrap(), b"meta");

    let c2 = encode_chunk(7, 4, b"EFGH");
    let chunk2 =
        thincan::CapnpTyped::<thincan_file_transfer::schema::file_chunk::Owned>::new(&c2[..]);
    thincan_file_transfer::handle_file_chunk_no_alloc(&mut state, chunk2).unwrap();
    let ack0 = state.take_pending_ack().expect("expected progress ack");
    assert_eq!(ack0.kind, thincan_file_transfer::schema::FileAckKind::Ack);
    assert_eq!(ack0.next_offset, 0);

    let c1 = encode_chunk(7, 0, b"ABCD");
    let chunk1 =
        thincan::CapnpTyped::<thincan_file_transfer::schema::file_chunk::Owned>::new(&c1[..]);
    thincan_file_transfer::handle_file_chunk_no_alloc(&mut state, chunk1).unwrap();
    let done = state.take_pending_ack().expect("expected complete");
    assert_eq!(done.kind, thincan_file_transfer::schema::FileAckKind::Complete);
    assert_eq!(done.next_offset, 8);
    assert!(state.store.committed);
    assert_eq!(state.store.as_bytes(), b"ABCDEFGH");
}

#[test]
fn heapless_receiver_aborts_when_range_capacity_exceeded() {
    type State = thincan_file_transfer::StateNoAlloc<FixedStore<16>, 0, 1>;
    let mut state = State::new(FixedStore::default());

    let req_bytes = encode_req_with_metadata(1, 8, 64, b"");
    let req = thincan::CapnpTyped::<thincan_file_transfer::schema::file_req::Owned>::new(&req_bytes);
    thincan_file_transfer::handle_file_req_no_alloc(&mut state, req).unwrap();
    state.take_pending_ack().unwrap();

    // First range: [0,1)
    let c1 = encode_chunk(1, 0, b"A");
    let chunk1 =
        thincan::CapnpTyped::<thincan_file_transfer::schema::file_chunk::Owned>::new(&c1[..]);
    thincan_file_transfer::handle_file_chunk_no_alloc(&mut state, chunk1).unwrap();
    state.take_pending_ack().unwrap();

    // Second disjoint range would require 2 ranges -> overflow triggers abort.
    let c2 = encode_chunk(1, 2, b"B");
    let chunk2 =
        thincan::CapnpTyped::<thincan_file_transfer::schema::file_chunk::Owned>::new(&c2[..]);
    thincan_file_transfer::handle_file_chunk_no_alloc(&mut state, chunk2).unwrap();

    let abort = state.take_pending_ack().expect("expected abort");
    assert_eq!(abort.kind, thincan_file_transfer::schema::FileAckKind::Abort);
    assert_eq!(abort.error, thincan_file_transfer::schema::FileAckError::Internal);
    assert_eq!(state.store.abort_calls, 1);
}

#[test]
fn heapless_sender_no_alloc_round_trips_through_router() {
    type Store = FixedStore<32>;
    type FtState = thincan_file_transfer::StateNoAlloc<Store, 0, 4>;

    let mut router = maplet::Router::new();
    let mut handlers = maplet::HandlersImpl {
        app: NoApp,
        file_transfer: FtState::new(Store::default()),
    };

    struct RouterTx<'a> {
        router: &'a mut maplet::Router,
        handlers: &'a mut maplet::HandlersImpl<NoApp, FtState>,
    }

    impl<'a> thincan_file_transfer::SendMsg for RouterTx<'a> {
        fn send_msg<M: thincan::Message>(
            &mut self,
            body: &[u8],
            _timeout: core::time::Duration,
        ) -> Result<(), thincan::Error> {
            self.router.dispatch(self.handlers, thincan::RawMessage { id: M::ID, body })?;
            Ok(())
        }
    }

    let file = b"hello-heapless";
    type Sender = thincan_file_transfer::SenderNoAlloc<atlas::Atlas, 128, 32>;
    let mut sender = Sender::with_chunk_size(8).unwrap();

    let mut tx = RouterTx {
        router: &mut router,
        handlers: &mut handlers,
    };

    sender
        .send_file_with_id(&mut tx, 42, file, core::time::Duration::from_millis(1))
        .unwrap();

    assert!(handlers.file_transfer.store.committed);
    assert_eq!(handlers.file_transfer.store.as_bytes(), file);
}

#[test]
fn heapless_sender_no_alloc_rejects_chunk_size_exceeding_body_capacity() {
    type Sender = thincan_file_transfer::SenderNoAlloc<atlas::Atlas, 64, 32>;
    assert!(Sender::with_chunk_size(128).is_err());
}

#[test]
fn heapless_sender_no_alloc_rejects_chunk_size_exceeding_scratch_capacity() {
    type Sender = thincan_file_transfer::SenderNoAlloc<atlas::Atlas, 256, 8>;
    assert!(Sender::with_chunk_size(200).is_err());
}

#[test]
fn heapless_receiver_enforces_max_chunk_size() {
    type State = thincan_file_transfer::StateNoAlloc<FixedStore<16>, 0, 4>;
    let mut state = State::new(FixedStore::default());
    state.set_config(thincan_file_transfer::ReceiverConfig { max_chunk_size: 4 });

    let req_bytes = encode_req_with_metadata(1, 8, 64, b"");
    let req = thincan::CapnpTyped::<thincan_file_transfer::schema::file_req::Owned>::new(&req_bytes);
    thincan_file_transfer::handle_file_req_no_alloc(&mut state, req).unwrap();
    state.take_pending_ack().unwrap();

    let c = encode_chunk(1, 0, b"TOO-LONG");
    let chunk = thincan::CapnpTyped::<thincan_file_transfer::schema::file_chunk::Owned>::new(&c);
    let err = thincan_file_transfer::handle_file_chunk_no_alloc(&mut state, chunk).unwrap_err();
    assert!(matches!(err, thincan_file_transfer::Error::Protocol));
}
