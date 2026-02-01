use can_iso_tp::{IsoTpConfig, IsoTpNode};
use embedded_can::StandardId;
use embedded_can_interface::{Id, SplitTxRx};
use embedded_can_mock::{BusHandle, MockCan};
use std::thread;
use std::time::{Duration, Instant};

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
    pub mod sender_bus: atlas {
        bundles [file_transfer = thincan_file_transfer::Bundle];
        parser: thincan::DefaultParser;
        use msgs [FileReq, FileChunk, FileAck];
        handles {}
        unhandled_by_default = true;
        ignore [];
    }
}

thincan::bus_maplet! {
    pub mod receiver_bus: atlas {
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

impl<'a> sender_bus::Handlers<'a> for NoApp {
    type Error = ();
}

impl<'a> receiver_bus::Handlers<'a> for NoApp {
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

fn cfg(tx: u16, rx: u16) -> IsoTpConfig {
    IsoTpConfig {
        tx_id: Id::Standard(StandardId::new(tx).unwrap()),
        rx_id: Id::Standard(StandardId::new(rx).unwrap()),
        block_size: 0,
        max_payload_len: 256,
        rx_buffer_len: 256,
        ..IsoTpConfig::default()
    }
}

fn main() -> Result<(), thincan::Error> {
    let bus = BusHandle::new();
    let can_a = MockCan::new_with_bus(&bus, vec![]).unwrap();
    let can_b = MockCan::new_with_bus(&bus, vec![]).unwrap();

    let (tx_a, rx_a) = can_a.split();
    let (tx_b, rx_b) = can_b.split();

    let node_sender = IsoTpNode::with_std_clock(tx_a, rx_a, cfg(0x700, 0x701)).unwrap();
    let node_receiver = IsoTpNode::with_std_clock(tx_b, rx_b, cfg(0x701, 0x700)).unwrap();

    let file = b"hello\nfrom\nfile_transfer\n".to_vec();
    let transfer_id = 123u32;
    let expected = file.clone();

    let receiver_thread = thread::spawn(move || -> Result<Vec<u8>, thincan::Error> {
        let mut iface = thincan::Interface::new(node_receiver, receiver_bus::Router::new());
        let mut handlers = receiver_bus::HandlersImpl {
            app: NoApp,
            file_transfer: thincan_file_transfer::State::new(MemoryStore::default()),
        };

        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if Instant::now() > deadline {
                return Err(thincan::Error::timeout());
            }
            match iface.recv_dispatch(&mut handlers, Duration::from_millis(50)) {
                Ok(_) => {}
                Err(thincan::Error {
                    kind: thincan::ErrorKind::Timeout,
                }) => continue,
                Err(e) => return Err(e),
            }
            if let Some(bytes) = handlers.file_transfer.store.committed.pop() {
                return Ok(bytes);
            }
        }
    });

    let mut iface = thincan::Interface::new(node_sender, sender_bus::Router::new());
    let mut handlers = sender_bus::HandlersImpl {
        app: NoApp,
        file_transfer: thincan_file_transfer::State::new(MemoryStore::default()),
    };

    let mut sender = thincan_file_transfer::Sender::<atlas::Atlas>::with_chunk_size(8)?;
    let result =
        sender.send_file_with_id(&mut iface, transfer_id, &file, Duration::from_millis(50))?;
    assert_eq!(result.transfer_id, transfer_id);

    // Drive RX on sender side (not strictly needed here, but shows intended usage pattern).
    let _ = iface.recv_dispatch(&mut handlers, Duration::from_millis(1));

    let received = receiver_thread.join().unwrap()?;
    assert_eq!(received, expected);
    Ok(())
}
