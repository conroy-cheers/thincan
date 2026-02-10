#![cfg(feature = "alloc")]

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

        0x3001 => LogLine(capnp = thincan_file_transfer::schema::log_line::Owned);
    }
}

impl thincan_file_transfer::Atlas for atlas::Atlas {
    type FileReq = atlas::FileReq;
    type FileChunk = atlas::FileChunk;
    type FileAck = atlas::FileAck;
}

thincan::bundle! {
    pub mod log_display(atlas) {
        parser: thincan::DefaultParser;
        use msgs [LogLine];
        handles { LogLine => on_log_line }
        items {
            pub trait LogSink {
                fn on_log_line(&mut self, line: &[u8]);
            }

            pub trait Deps {
                type Sink: LogSink;
                fn sink(&mut self) -> &mut Self::Sink;
            }

            #[derive(Default)]
            pub struct State<S: LogSink> {
                pub sink: S,
            }

            impl<S: LogSink> Deps for State<S> {
                type Sink = S;
                fn sink(&mut self) -> &mut Self::Sink {
                    &mut self.sink
                }
            }

            impl<'a, T> Handlers<'a> for T
            where
                T: Deps,
            {
                type Error = core::convert::Infallible;

                fn on_log_line(
                    &mut self,
                    msg: thincan::CapnpTyped<'a, thincan_file_transfer::schema::log_line::Owned>,
                ) -> Result<(), Self::Error> {
                    let bytes = msg
                        .with_root(capnp::message::ReaderOptions::default(), |root| {
                            root.get_bytes().unwrap().to_vec()
                        })
                        .unwrap();
                    self.sink().on_log_line(&bytes);
                    Ok(())
                }
            }

            #[derive(Debug, Clone, Copy, PartialEq, Eq)]
            pub struct LogLineValue<'a> {
                pub bytes: &'a [u8],
            }

            impl<'a> thincan::Encode<LogLine> for LogLineValue<'a> {
                fn max_encoded_len(&self) -> usize {
                    256
                }

                fn encode(&self, out: &mut [u8]) -> Result<usize, thincan::Error> {
                    let mut builder_buf = [0u8; 256];
                    let mut message = capnp::message::Builder::new(
                        capnp::message::SingleSegmentAllocator::new(&mut builder_buf),
                    );
                    let mut root: thincan_file_transfer::schema::log_line::Builder =
                        message.init_root();
                    root.set_bytes(self.bytes);

                    let segments = message.get_segments_for_output();
                    if segments.len() != 1 {
                        return Err(thincan::Error {
                            kind: thincan::ErrorKind::Other,
                        });
                    }
                    let bytes = segments[0];
                    if out.len() < bytes.len() {
                        return Err(thincan::Error {
                            kind: thincan::ErrorKind::Other,
                        });
                    }
                    out[..bytes.len()].copy_from_slice(bytes);
                    Ok(bytes.len())
                }
            }
        }
    }
}

thincan::bus_maplet! {
    pub mod log_displayer_bus: atlas {
        bundles [file_transfer = thincan_file_transfer::Bundle, log_display];
        parser: thincan::DefaultParser;
        use msgs [FileReq, FileChunk, FileAck, LogLine];
        handles {}
        unhandled_by_default = true;
        ignore [];
    }
}

thincan::bus_maplet! {
    pub mod log_sender_bus: atlas {
        bundles [file_transfer = thincan_file_transfer::Bundle, log_display];
        parser: thincan::DefaultParser;
        use msgs [FileReq, FileChunk, FileAck, LogLine];
        handles {}
        unhandled_by_default = true;
        ignore [];
    }
}

#[derive(Default)]
struct NoApp;

impl<'a> log_displayer_bus::Handlers<'a> for NoApp {
    type Error = ();
}

impl<'a> log_sender_bus::Handlers<'a> for NoApp {
    type Error = ();
}

#[derive(Default)]
struct MemoryFileStore {
    committed: Vec<Vec<u8>>,
}

impl thincan_file_transfer::FileStore for MemoryFileStore {
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

#[derive(Default)]
struct CollectingLogSink {
    lines: Vec<Vec<u8>>,
}

impl log_display::LogSink for CollectingLogSink {
    fn on_log_line(&mut self, line: &[u8]) {
        self.lines.push(line.to_vec());
    }
}

fn cfg(tx: u16, rx: u16) -> IsoTpConfig {
    IsoTpConfig {
        tx_id: Id::Standard(StandardId::new(tx).unwrap()),
        rx_id: Id::Standard(StandardId::new(rx).unwrap()),
        block_size: 0,
        max_payload_len: 256,
        ..IsoTpConfig::default()
    }
}

#[test]
fn end_to_end_file_to_logs_capnp() -> Result<(), thincan::Error> {
    let bus = BusHandle::new();
    let can_a = MockCan::new_with_bus(&bus, vec![]).unwrap();
    let can_b = MockCan::new_with_bus(&bus, vec![]).unwrap();

    let (tx_a, rx_a) = can_a.split();
    let (tx_b, rx_b) = can_b.split();

    let file = b"one\ntwoooooooo\nthree\n".to_vec();
    let transfer_id = 42u32;
    let expected_logs: Vec<Vec<u8>> = file
        .split(|b| *b == b'\n')
        .filter(|line| !line.is_empty())
        .map(|line| line[..line.len().min(8)].to_vec())
        .collect();
    let expected_log_count = expected_logs.len();
    let expected_logs_for_assert = expected_logs.clone();

    let file_for_sender = file.clone();
    let expected_logs_for_sender = expected_logs.clone();

    let sender_thread = thread::spawn(move || -> Result<(), thincan::Error> {
        #[derive(Clone, Copy, Debug, Default)]
        struct StdInstantClock;

        impl can_iso_tp::Clock for StdInstantClock {
            type Instant = Instant;
            fn now(&self) -> Self::Instant {
                Instant::now()
            }
            fn elapsed(&self, earlier: Self::Instant) -> Duration {
                earlier.elapsed()
            }
            fn add(&self, instant: Self::Instant, dur: Duration) -> Self::Instant {
                instant.checked_add(dur).unwrap_or(instant)
            }
        }

        let mut rx_buf = [0u8; 256];
        let node_sender =
            IsoTpNode::with_clock(tx_b, rx_b, cfg(0x701, 0x700), StdInstantClock, &mut rx_buf)
                .unwrap();

        let mut tx_buf = [0u8; 512];
        let mut iface =
            thincan::Interface::new(node_sender, log_sender_bus::Router::new(), &mut tx_buf);
        let mut handlers = log_sender_bus::HandlersImpl {
            app: NoApp,
            file_transfer: thincan_file_transfer::State::new(MemoryFileStore::default()),
            log_display: log_display::State {
                sink: CollectingLogSink::default(),
            },
        };

        let deadline = Instant::now() + Duration::from_secs(2);
        let file_bytes = loop {
            if Instant::now() > deadline {
                return Err(thincan::Error {
                    kind: thincan::ErrorKind::Timeout,
                });
            }
            match iface.recv_one_dispatch(&mut handlers, Duration::from_millis(50))? {
                thincan::RecvDispatch::TimedOut => continue,
                thincan::RecvDispatch::MalformedPayload => {
                    return Err(thincan::Error {
                        kind: thincan::ErrorKind::Other,
                    });
                }
                thincan::RecvDispatch::Dispatched { .. } => {}
            }
            if let Some(bytes) = handlers.file_transfer.store.committed.pop() {
                break bytes;
            }
        };

        assert_eq!(file_bytes, file_for_sender);

        for line in file_bytes.split(|b| *b == b'\n') {
            if line.is_empty() {
                continue;
            }
            iface.send_encoded::<atlas::LogLine, _>(
                &log_display::LogLineValue {
                    bytes: &line[..line.len().min(8)],
                },
                Duration::from_millis(50),
            )?;
        }
        assert_eq!(expected_logs_for_sender.len(), 3);
        Ok(())
    });

    let displayer_thread = thread::spawn(move || -> Result<Vec<Vec<u8>>, thincan::Error> {
        #[derive(Clone, Copy, Debug, Default)]
        struct StdInstantClock;

        impl can_iso_tp::Clock for StdInstantClock {
            type Instant = Instant;
            fn now(&self) -> Self::Instant {
                Instant::now()
            }
            fn elapsed(&self, earlier: Self::Instant) -> Duration {
                earlier.elapsed()
            }
            fn add(&self, instant: Self::Instant, dur: Duration) -> Self::Instant {
                instant.checked_add(dur).unwrap_or(instant)
            }
        }

        let mut rx_buf = [0u8; 256];
        let node_displayer =
            IsoTpNode::with_clock(tx_a, rx_a, cfg(0x700, 0x701), StdInstantClock, &mut rx_buf)
                .unwrap();

        let mut tx_buf = [0u8; 512];
        let mut iface = thincan::Interface::new(
            node_displayer,
            log_displayer_bus::Router::new(),
            &mut tx_buf,
        );
        let mut handlers = log_displayer_bus::HandlersImpl {
            app: NoApp,
            file_transfer: thincan_file_transfer::State::new(MemoryFileStore::default()),
            log_display: log_display::State {
                sink: CollectingLogSink::default(),
            },
        };

        iface.send_encoded::<atlas::FileReq, _>(
            &thincan_file_transfer::file_req::<atlas::Atlas>(transfer_id, file.len() as u32),
            Duration::from_millis(50),
        )?;

        for offset in (0..file.len()).step_by(8) {
            let end = (offset + 8).min(file.len());
            iface.send_encoded::<atlas::FileChunk, _>(
                &thincan_file_transfer::file_chunk::<atlas::Atlas>(
                    transfer_id,
                    offset as u32,
                    &file[offset..end],
                ),
                Duration::from_millis(50),
            )?;
        }

        let deadline = Instant::now() + Duration::from_secs(2);
        while handlers.log_display.sink.lines.len() < expected_log_count {
            if Instant::now() > deadline {
                return Err(thincan::Error {
                    kind: thincan::ErrorKind::Timeout,
                });
            }
            match iface.recv_one_dispatch(&mut handlers, Duration::from_millis(50))? {
                thincan::RecvDispatch::TimedOut => continue,
                thincan::RecvDispatch::MalformedPayload => {
                    return Err(thincan::Error {
                        kind: thincan::ErrorKind::Other,
                    });
                }
                thincan::RecvDispatch::Dispatched { .. } => {}
            }
        }

        Ok(handlers.log_display.sink.lines)
    });

    let sender_res = sender_thread.join().unwrap();
    let logs = displayer_thread.join().unwrap()?;
    sender_res?;

    assert_eq!(logs, expected_logs_for_assert);
    Ok(())
}
