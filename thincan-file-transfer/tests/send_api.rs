#![cfg(feature = "alloc")]

use capnp::message::ReaderOptions;
use std::time::Duration;
use thincan::Message;

#[derive(Clone, Copy)]
struct FileReq;
#[derive(Clone, Copy)]
struct FileChunk;
#[derive(Clone, Copy)]
struct FileAck;

impl thincan::Message for FileReq {
    const ID: u16 = 0x1001;
}
impl thincan::Message for FileChunk {
    const ID: u16 = 0x1002;
}
impl thincan::Message for FileAck {
    const ID: u16 = 0x1003;
}

struct MyAtlas;
impl thincan_file_transfer::Atlas for MyAtlas {
    type FileReq = FileReq;
    type FileChunk = FileChunk;
    type FileAck = FileAck;
}

#[derive(Default)]
struct CaptureTx {
    sent: Vec<(u16, Vec<u8>)>,
}

impl thincan_file_transfer::SendEncoded for CaptureTx {
    fn send_encoded<M: thincan::Message, V: thincan::Encode<M>>(
        &mut self,
        value: &V,
        _timeout: Duration,
    ) -> Result<(), thincan::Error> {
        let mut buf = vec![0u8; value.max_encoded_len()];
        let used = value.encode(&mut buf)?;
        buf.truncate(used);
        self.sent.push((M::ID, buf));
        Ok(())
    }
}

#[derive(Default)]
struct MemoryStore {
    committed: Vec<Vec<u8>>,
}

impl thincan_file_transfer::FileStore for MemoryStore {
    type Error = core::convert::Infallible;
    type WriteHandle = Vec<u8>;

    fn begin_write(
        &mut self,
        _transfer_id: u32,
        total_len: u32,
    ) -> Result<Self::WriteHandle, Self::Error> {
        Ok(vec![0u8; total_len as usize])
    }

    fn write_at(
        &mut self,
        handle: &mut Self::WriteHandle,
        offset: u32,
        bytes: &[u8],
    ) -> Result<(), Self::Error> {
        let start = offset as usize;
        let end = start + bytes.len();
        handle[start..end].copy_from_slice(bytes);
        Ok(())
    }

    fn commit(&mut self, handle: Self::WriteHandle) -> Result<(), Self::Error> {
        self.committed.push(handle);
        Ok(())
    }

    fn abort(&mut self, _handle: Self::WriteHandle) {}
}

fn decode_req(body: &[u8]) -> (u32, u32) {
    let msg = thincan::CapnpTyped::<thincan_file_transfer::schema::file_req::Owned>::new(body);
    msg.with_root(ReaderOptions::default(), |root| {
        (root.get_transfer_id(), root.get_total_len())
    })
    .unwrap()
}

fn decode_chunk(body: &[u8]) -> (u32, u32, Vec<u8>) {
    let msg = thincan::CapnpTyped::<thincan_file_transfer::schema::file_chunk::Owned>::new(body);
    msg.with_root(ReaderOptions::default(), |root| {
        (
            root.get_transfer_id(),
            root.get_offset(),
            root.get_data().unwrap().to_vec(),
        )
    })
    .unwrap()
}

#[test]
fn send_file_emits_req_and_chunks() {
    let file = b"hello world!".to_vec();
    let mut tx = CaptureTx::default();
    let mut sender = thincan_file_transfer::Sender::<MyAtlas>::with_chunk_size(5).unwrap();

    let result = sender
        .send_file(&mut tx, &file, Duration::from_millis(10))
        .unwrap();

    assert_eq!(result.transfer_id, 1);
    assert_eq!(result.total_len, file.len());
    assert_eq!(result.chunk_size, 5);
    assert_eq!(result.chunks, 3);
    assert_eq!(tx.sent.len(), 4);

    let (id, body) = &tx.sent[0];
    assert_eq!(*id, FileReq::ID);
    let (transfer_id, total_len) = decode_req(body);
    assert_eq!(transfer_id, 1);
    assert_eq!(total_len as usize, file.len());

    let expected_chunks = [&file[0..5], &file[5..10], &file[10..]];
    for (index, (id, body)) in tx.sent[1..].iter().enumerate() {
        assert_eq!(*id, FileChunk::ID);
        let (transfer_id, offset, data) = decode_chunk(body);
        assert_eq!(transfer_id, 1);
        assert_eq!(offset as usize, index * 5);
        assert_eq!(data, expected_chunks[index]);
    }

    let mut state = thincan_file_transfer::State::new(MemoryStore::default());
    for (id, body) in tx.sent {
        if id == FileReq::ID {
            let typed =
                thincan::CapnpTyped::<thincan_file_transfer::schema::file_req::Owned>::new(&body);
            thincan_file_transfer::handle_file_req(&mut state, typed).unwrap();
        } else if id == FileChunk::ID {
            let typed =
                thincan::CapnpTyped::<thincan_file_transfer::schema::file_chunk::Owned>::new(&body);
            thincan_file_transfer::handle_file_chunk(&mut state, typed).unwrap();
        } else {
            panic!("unexpected message id {id:#x}");
        }
    }

    assert_eq!(state.store.committed.len(), 1);
    assert_eq!(state.store.committed[0], file);
}

#[test]
fn send_file_empty_sends_only_req() {
    let file = vec![];
    let mut tx = CaptureTx::default();
    let mut sender = thincan_file_transfer::Sender::<MyAtlas>::new();

    let result = sender
        .send_file(&mut tx, &file, Duration::from_millis(10))
        .unwrap();

    assert_eq!(result.transfer_id, 1);
    assert_eq!(result.total_len, 0);
    assert_eq!(result.chunks, 0);
    assert_eq!(tx.sent.len(), 1);

    let (id, body) = &tx.sent[0];
    assert_eq!(*id, FileReq::ID);
    let (transfer_id, total_len) = decode_req(body);
    assert_eq!(transfer_id, 1);
    assert_eq!(total_len, 0);
}

#[test]
fn send_file_increments_transfer_id() {
    let mut tx = CaptureTx::default();
    let mut sender = thincan_file_transfer::Sender::<MyAtlas>::new();

    let first = sender
        .send_file(&mut tx, &[], Duration::from_millis(10))
        .unwrap();
    let second = sender
        .send_file(&mut tx, &[], Duration::from_millis(10))
        .unwrap();

    assert_eq!(first.transfer_id, 1);
    assert_eq!(second.transfer_id, 2);
    assert_eq!(sender.next_transfer_id(), 3);
}

#[test]
fn send_file_with_id_uses_explicit_id() {
    let file = b"abc".to_vec();
    let mut tx = CaptureTx::default();
    let mut sender = thincan_file_transfer::Sender::<MyAtlas>::new();

    let result = sender
        .send_file_with_id(&mut tx, 99, &file, Duration::from_millis(10))
        .unwrap();

    assert_eq!(result.transfer_id, 99);
    let (id, body) = &tx.sent[0];
    assert_eq!(*id, FileReq::ID);
    let (transfer_id, total_len) = decode_req(body);
    assert_eq!(transfer_id, 99);
    assert_eq!(total_len as usize, file.len());
}

#[test]
fn send_file_rejects_zero_chunk_size() {
    let mut sender = thincan_file_transfer::Sender::<MyAtlas>::new();
    let err = sender.set_chunk_size(0).unwrap_err();
    assert!(matches!(err.kind, thincan::ErrorKind::Other));
}
