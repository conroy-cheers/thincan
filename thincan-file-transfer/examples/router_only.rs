use std::time::Duration;
use thincan::RouterDispatch;

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

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut router = maplet::Router::new();
    let mut handlers = maplet::HandlersImpl {
        app: NoApp,
        file_transfer: thincan_file_transfer::State::new(MemoryStore::default()),
    };

    let file = b"hello world".to_vec();
    let transfer_id = 1u32;

    struct RouterTx<'a> {
        router: &'a mut maplet::Router,
        handlers: &'a mut maplet::HandlersImpl<NoApp, thincan_file_transfer::State<MemoryStore>>,
    }

    impl<'a> thincan_file_transfer::SendEncoded for RouterTx<'a> {
        fn send_encoded<M: thincan::Message, V: thincan::Encode<M>>(
            &mut self,
            value: &V,
            _timeout: Duration,
        ) -> Result<(), thincan::Error> {
            let mut buf = vec![0u8; value.max_encoded_len()];
            let used = value.encode(&mut buf)?;
            self.router.dispatch(
                self.handlers,
                thincan::RawMessage {
                    id: M::ID,
                    body: &buf[..used],
                },
            )?;
            Ok(())
        }
    }

    let mut tx = RouterTx {
        router: &mut router,
        handlers: &mut handlers,
    };
    let mut sender = thincan_file_transfer::Sender::<atlas::Atlas>::with_chunk_size(file.len())?;
    let result =
        sender.send_file_with_id(&mut tx, transfer_id, &file, Duration::from_millis(10))?;
    assert_eq!(result.transfer_id, transfer_id);

    assert_eq!(handlers.file_transfer.store.committed.len(), 1);
    assert_eq!(handlers.file_transfer.store.committed[0], file);
    Ok(())
}
