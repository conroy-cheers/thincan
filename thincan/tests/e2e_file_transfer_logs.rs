#![cfg(feature = "std")]

use can_iso_tp::{IsoTpAsyncNode, IsoTpConfig};
use can_isotp_interface::{
    IsoTpAsyncEndpoint, IsoTpAsyncEndpointRecvInto, RecvControl, RecvError, RecvMeta,
    RecvMetaIntoStatus, RecvStatus, SendError,
};
use embedded_can::StandardId;
use embedded_can_interface::{AsyncRxFrameIo, AsyncTxFrameIo, Id};
use embedded_can_mock::MockFrame;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{mpsc, Mutex as TokioMutex};

#[derive(Debug)]
enum TokioCanError {
    Closed,
}

struct TokioBus {
    peers: TokioMutex<Vec<mpsc::UnboundedSender<MockFrame>>>,
}

impl TokioBus {
    fn new() -> Self {
        Self {
            peers: TokioMutex::new(Vec::new()),
        }
    }

    async fn add_interface(self: &Arc<Self>) -> (TokioTx, TokioRx) {
        let (tx, rx) = mpsc::unbounded_channel();
        self.peers.lock().await.push(tx);
        (TokioTx { bus: self.clone() }, TokioRx { rx })
    }

    async fn transmit(&self, frame: MockFrame) {
        let peers = self.peers.lock().await;
        for peer in peers.iter() {
            let _ = peer.send(frame.clone());
        }
    }
}

struct TokioTx {
    bus: Arc<TokioBus>,
}

struct TokioRx {
    rx: mpsc::UnboundedReceiver<MockFrame>,
}

impl AsyncTxFrameIo for TokioTx {
    type Frame = MockFrame;
    type Error = TokioCanError;

    async fn send(&mut self, frame: &Self::Frame) -> Result<(), Self::Error> {
        self.bus.transmit(frame.clone()).await;
        Ok(())
    }

    async fn send_timeout(
        &mut self,
        frame: &Self::Frame,
        _timeout: Duration,
    ) -> Result<(), Self::Error> {
        self.send(frame).await
    }
}

impl AsyncRxFrameIo for TokioRx {
    type Frame = MockFrame;
    type Error = TokioCanError;

    async fn recv(&mut self) -> Result<Self::Frame, Self::Error> {
        self.rx.recv().await.ok_or(TokioCanError::Closed)
    }

    async fn recv_timeout(&mut self, _timeout: Duration) -> Result<Self::Frame, Self::Error> {
        self.recv().await
    }

    async fn wait_not_empty(&mut self) -> Result<(), Self::Error> {
        Ok(())
    }
}

thincan::bus_atlas! {
    pub mod atlas {
        0x1001 => FileReq(len = 8);
        0x1002 => FileChunk(len = 16);
        0x1003 => FileAck(len = 4);

        0x3001 => LogLine(len = 9);
    }
}

thincan::bundle! {
    pub mod file_transfer(atlas) {
        parser: Parser;
        use msgs [FileReq, FileChunk, FileAck];
        handles {
            FileReq => on_file_req,
            FileChunk => on_file_chunk,
            FileAck => on_file_ack,
        }
        items {
            #[derive(Debug, Clone, Copy, Default)]
            pub struct Parser;

            pub trait FileStore {
                type Error;
                type WriteHandle;

                fn begin_write(
                    &mut self,
                    transfer_id: u32,
                    total_len: u32,
                ) -> Result<Self::WriteHandle, Self::Error>;

                fn write_at(
                    &mut self,
                    handle: &mut Self::WriteHandle,
                    offset: u32,
                    bytes: &[u8],
                ) -> Result<(), Self::Error>;

                fn commit(&mut self, handle: Self::WriteHandle) -> Result<(), Self::Error>;

                fn abort(&mut self, handle: Self::WriteHandle);
            }

            pub trait Deps {
                type Store: FileStore;
                fn state(&mut self) -> &mut State<Self::Store>;
            }

            #[derive(Debug)]
            pub enum Error<E> {
                Store(E),
                Protocol,
            }

            impl<E> From<E> for Error<E> {
                fn from(e: E) -> Self {
                    Self::Store(e)
                }
            }

            #[derive(Debug, Clone, Copy, PartialEq, Eq)]
            pub struct FileReqParsed {
                pub transfer_id: u32,
                pub total_len: u32,
            }

            impl thincan::MessageParser<FileReq> for Parser {
                type Parsed<'a> = FileReqParsed;
                fn parse<'a>(&self, body: &'a [u8]) -> Result<Self::Parsed<'a>, thincan::Error> {
                    if body.len() != FileReq::BODY_LEN {
                        return Err(thincan::Error {
                            kind: thincan::ErrorKind::Other,
                        });
                    }
                    let transfer_id = u32::from_le_bytes([body[0], body[1], body[2], body[3]]);
                    let total_len = u32::from_le_bytes([body[4], body[5], body[6], body[7]]);
                    Ok(FileReqParsed {
                        transfer_id,
                        total_len,
                    })
                }
            }

            #[derive(Debug, Clone, Copy, PartialEq, Eq)]
            pub struct FileChunkParsed<'a> {
                pub transfer_id: u32,
                pub offset: u32,
                pub data: &'a [u8; 8],
            }

            impl thincan::MessageParser<FileChunk> for Parser {
                type Parsed<'a> = FileChunkParsed<'a>;
                fn parse<'a>(&self, body: &'a [u8]) -> Result<Self::Parsed<'a>, thincan::Error> {
                    if body.len() != FileChunk::BODY_LEN {
                        return Err(thincan::Error {
                            kind: thincan::ErrorKind::Other,
                        });
                    }
                    let transfer_id = u32::from_le_bytes([body[0], body[1], body[2], body[3]]);
                    let offset = u32::from_le_bytes([body[4], body[5], body[6], body[7]]);
                    let data: &[u8; 8] = body[8..16].try_into().map_err(|_| thincan::Error {
                        kind: thincan::ErrorKind::Other,
                    })?;
                    Ok(FileChunkParsed {
                        transfer_id,
                        offset,
                        data,
                    })
                }
            }

            #[derive(Debug, Clone, Copy, PartialEq, Eq)]
            pub struct FileAckParsed {
                pub transfer_id: u32,
            }

            impl thincan::MessageParser<FileAck> for Parser {
                type Parsed<'a> = FileAckParsed;
                fn parse<'a>(&self, body: &'a [u8]) -> Result<Self::Parsed<'a>, thincan::Error> {
                    if body.len() != FileAck::BODY_LEN {
                        return Err(thincan::Error {
                            kind: thincan::ErrorKind::Other,
                        });
                    }
                    let transfer_id = u32::from_le_bytes([body[0], body[1], body[2], body[3]]);
                    Ok(FileAckParsed { transfer_id })
                }
            }

            struct InProgress<S: FileStore> {
                transfer_id: u32,
                total_len: u32,
                handle: S::WriteHandle,
            }

            #[derive(Default)]
            pub struct State<S: FileStore> {
                pub store: S,
                in_progress: Option<InProgress<S>>,
            }

            impl<S: FileStore> State<S> {
                pub fn new(store: S) -> Self {
                    Self {
                        store,
                        in_progress: None,
                    }
                }
            }

            impl<S: FileStore> Deps for State<S> {
                type Store = S;
                fn state(&mut self) -> &mut State<Self::Store> {
                    self
                }
            }

            impl<'a, T, ReplyTo> Handlers<'a, ReplyTo> for T
            where
                ReplyTo: Copy,
                T: Deps,
            {
                type Error = Error<<<T as Deps>::Store as FileStore>::Error>;

                async fn on_file_req(
                    &mut self,
                    _meta: thincan::RecvMeta<ReplyTo>,
                    msg: FileReqParsed,
                ) -> Result<(), Self::Error> {
                    let state = self.state();
                    if let Some(in_progress) = state.in_progress.take() {
                        state.store.abort(in_progress.handle);
                    }
                    let handle = state.store.begin_write(msg.transfer_id, msg.total_len)?;
                    state.in_progress = Some(InProgress {
                        transfer_id: msg.transfer_id,
                        total_len: msg.total_len,
                        handle,
                    });
                    Ok(())
                }

                async fn on_file_chunk(
                    &mut self,
                    _meta: thincan::RecvMeta<ReplyTo>,
                    msg: FileChunkParsed<'a>,
                ) -> Result<(), Self::Error> {
                    let state = self.state();
                    let in_progress = state.in_progress.as_mut().ok_or(Error::Protocol)?;
                    if in_progress.transfer_id != msg.transfer_id {
                        return Err(Error::Protocol);
                    }

                    let remaining = in_progress.total_len.saturating_sub(msg.offset);
                    if remaining == 0 {
                        return Ok(());
                    }
                    let chunk_len = remaining.min(8) as usize;
                    state.store.write_at(
                        &mut in_progress.handle,
                        msg.offset,
                        &msg.data[..chunk_len],
                    )?;

                    if msg.offset + (chunk_len as u32) >= in_progress.total_len {
                        let in_progress = state.in_progress.take().ok_or(Error::Protocol)?;
                        state.store.commit(in_progress.handle)?;
                    }
                    Ok(())
                }

                async fn on_file_ack(
                    &mut self,
                    _meta: thincan::RecvMeta<ReplyTo>,
                    _msg: FileAckParsed,
                ) -> Result<(), Self::Error> {
                    Ok(())
                }
            }

            pub fn encode_req(transfer_id: u32, total_len: u32) -> [u8; FileReq::BODY_LEN] {
                let mut out = [0u8; FileReq::BODY_LEN];
                out[0..4].copy_from_slice(&transfer_id.to_le_bytes());
                out[4..8].copy_from_slice(&total_len.to_le_bytes());
                out
            }

            pub fn encode_chunk(
                transfer_id: u32,
                offset: u32,
                data: &[u8],
            ) -> [u8; FileChunk::BODY_LEN] {
                let mut out = [0u8; FileChunk::BODY_LEN];
                out[0..4].copy_from_slice(&transfer_id.to_le_bytes());
                out[4..8].copy_from_slice(&offset.to_le_bytes());
                let copy_len = data.len().min(8);
                out[8..8 + copy_len].copy_from_slice(&data[..copy_len]);
                out
            }
        }
    }
}

thincan::bundle! {
    pub mod log_display(atlas) {
        parser: Parser;
        use msgs [LogLine];
        handles { LogLine => on_log_line }
        items {
            #[derive(Debug, Clone, Copy, Default)]
            pub struct Parser;

            pub trait LogSink {
                fn on_log_line(&mut self, line: &[u8]);
            }

            pub trait Deps {
                type Sink: LogSink;
                fn sink(&mut self) -> &mut Self::Sink;
            }

            impl thincan::MessageParser<LogLine> for Parser {
                type Parsed<'a> = &'a [u8];
                fn parse<'a>(&self, body: &'a [u8]) -> Result<Self::Parsed<'a>, thincan::Error> {
                    if body.len() != LogLine::BODY_LEN {
                        return Err(thincan::Error {
                            kind: thincan::ErrorKind::Other,
                        });
                    }
                    let len = (body[0] as usize).min(8);
                    Ok(&body[1..1 + len])
                }
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

            impl<'a, T, ReplyTo> Handlers<'a, ReplyTo> for T
            where
                ReplyTo: Copy,
                T: Deps,
            {
                type Error = core::convert::Infallible;

                async fn on_log_line(
                    &mut self,
                    _meta: thincan::RecvMeta<ReplyTo>,
                    line: &'a [u8],
                ) -> Result<(), Self::Error> {
                    self.sink().on_log_line(line);
                    Ok(())
                }
            }
        }
    }
}

thincan::bundle! {
    pub mod log_tx(atlas) {
        parser: thincan::DefaultParser;
        use msgs [LogLine];
        handles {}
        items {
            pub fn encode_line(bytes: &[u8]) -> [u8; LogLine::BODY_LEN] {
                let mut out = [0u8; LogLine::BODY_LEN];
                let len = bytes.len().min(8);
                out[0] = len as u8;
                out[1..1 + len].copy_from_slice(&bytes[..len]);
                out
            }
        }
    }
}

thincan::bus_maplet! {
    pub mod log_displayer_bus: atlas {
        reply_to: u8;
        bundles [file_transfer, log_display];
        parser: thincan::DefaultParser;
        use msgs [FileReq, FileChunk, FileAck, LogLine];
        handles {}
        unhandled_by_default = true;
        ignore [];
    }
}

thincan::bus_maplet! {
    pub mod log_sender_bus: atlas {
        reply_to: u8;
        bundles [file_transfer, log_tx];
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
struct RecordingFileStore {
    begin_calls: Vec<(u32, u32)>,
    commit_calls: usize,
    abort_calls: usize,
    committed: Vec<CommittedTransfer>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct WriteOp {
    offset: u32,
    bytes: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CommittedTransfer {
    transfer_id: u32,
    total_len: u32,
    bytes: Vec<u8>,
    writes: Vec<WriteOp>,
}

#[derive(Debug)]
struct WriteHandle {
    transfer_id: u32,
    total_len: u32,
    bytes: Vec<u8>,
    writes: Vec<WriteOp>,
}

impl file_transfer::FileStore for RecordingFileStore {
    type Error = core::convert::Infallible;
    type WriteHandle = WriteHandle;

    fn begin_write(
        &mut self,
        transfer_id: u32,
        total_len: u32,
    ) -> Result<Self::WriteHandle, Self::Error> {
        self.begin_calls.push((transfer_id, total_len));
        Ok(WriteHandle {
            transfer_id,
            total_len,
            bytes: vec![0u8; total_len as usize],
            writes: Vec::new(),
        })
    }

    fn write_at(
        &mut self,
        handle: &mut Self::WriteHandle,
        offset: u32,
        bytes: &[u8],
    ) -> Result<(), Self::Error> {
        let start = offset as usize;
        let end = start + bytes.len();
        handle.bytes[start..end].copy_from_slice(bytes);
        handle.writes.push(WriteOp {
            offset,
            bytes: bytes.to_vec(),
        });
        Ok(())
    }

    fn commit(&mut self, handle: Self::WriteHandle) -> Result<(), Self::Error> {
        self.commit_calls += 1;
        self.committed.push(CommittedTransfer {
            transfer_id: handle.transfer_id,
            total_len: handle.total_len,
            bytes: handle.bytes,
            writes: handle.writes,
        });
        Ok(())
    }

    fn abort(&mut self, _handle: Self::WriteHandle) {
        self.abort_calls += 1;
    }
}

impl RecordingFileStore {
    fn take_latest(&mut self) -> Option<CommittedTransfer> {
        self.committed.pop()
    }
}

#[derive(Default)]
struct CollectingLogSink {
    lines: Vec<Vec<u8>>,
}

impl log_display::LogSink for CollectingLogSink {
    fn on_log_line(&mut self, line: &[u8]) {
        println!("LOG: {}", String::from_utf8_lossy(line));
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

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct DispatchCounts {
    handled: usize,
    unhandled: usize,
    unknown: usize,
}

#[derive(Debug)]
struct LoggingTransport<T> {
    inner: T,
    sent: Arc<Mutex<Vec<Vec<u8>>>>,
    received: Arc<Mutex<Vec<Vec<u8>>>>,
}

impl<T> LoggingTransport<T> {
    fn new(inner: T, sent: Arc<Mutex<Vec<Vec<u8>>>>, received: Arc<Mutex<Vec<Vec<u8>>>>) -> Self {
        Self {
            inner,
            sent,
            received,
        }
    }
}

impl<T> IsoTpAsyncEndpoint for LoggingTransport<T>
where
    T: IsoTpAsyncEndpoint,
{
    type Error = T::Error;

    async fn send_to(
        &mut self,
        to: u8,
        payload: &[u8],
        timeout: Duration,
    ) -> Result<(), SendError<Self::Error>> {
        self.sent.lock().unwrap().push(payload.to_vec());
        self.inner.send_to(to, payload, timeout).await
    }

    async fn recv_one<Cb>(
        &mut self,
        timeout: Duration,
        mut on_payload: Cb,
    ) -> Result<RecvStatus, RecvError<Self::Error>>
    where
        Cb: FnMut(RecvMeta, &[u8]) -> Result<RecvControl, Self::Error>,
    {
        let received = self.received.clone();
        self.inner
            .recv_one(timeout, |meta, payload| {
                received.lock().unwrap().push(payload.to_vec());
                on_payload(meta, payload)
            })
            .await
    }
}

impl<T> IsoTpAsyncEndpointRecvInto for LoggingTransport<T>
where
    T: IsoTpAsyncEndpointRecvInto,
{
    type Error = T::Error;

    async fn recv_one_into(
        &mut self,
        timeout: Duration,
        out: &mut [u8],
    ) -> Result<RecvMetaIntoStatus, RecvError<Self::Error>> {
        match self.inner.recv_one_into(timeout, out).await? {
            RecvMetaIntoStatus::TimedOut => Ok(RecvMetaIntoStatus::TimedOut),
            RecvMetaIntoStatus::DeliveredOne { meta, len } => {
                self.received.lock().unwrap().push(out[..len].to_vec());
                Ok(RecvMetaIntoStatus::DeliveredOne { meta, len })
            }
        }
    }
}

#[derive(Clone, Copy)]
struct TokioRuntime;

impl can_iso_tp::AsyncRuntime for TokioRuntime {
    type TimeoutError = tokio::time::error::Elapsed;

    type Sleep<'a>
        = tokio::time::Sleep
    where
        Self: 'a;
    fn sleep<'a>(&'a self, duration: Duration) -> Self::Sleep<'a> {
        tokio::time::sleep(duration)
    }

    type Timeout<'a, F>
        = tokio::time::Timeout<F>
    where
        Self: 'a,
        F: core::future::Future + 'a;
    fn timeout<'a, F>(&'a self, duration: Duration, future: F) -> Self::Timeout<'a, F>
    where
        F: core::future::Future + 'a,
    {
        tokio::time::timeout(duration, future)
    }
}

#[derive(Clone, Copy, Debug, Default)]
struct TokioClock;

impl can_iso_tp::Clock for TokioClock {
    type Instant = tokio::time::Instant;

    fn now(&self) -> Self::Instant {
        tokio::time::Instant::now()
    }

    fn elapsed(&self, earlier: Self::Instant) -> Duration {
        earlier.elapsed()
    }

    fn add(&self, instant: Self::Instant, dur: Duration) -> Self::Instant {
        instant + dur
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn end_to_end_file_to_logs() -> Result<(), thincan::Error> {
    let bus = Arc::new(TokioBus::new());
    let (tx_a, rx_a) = bus.add_interface().await;
    let (tx_b, rx_b) = bus.add_interface().await;

    let file = b"one\ntwoooooooo\nthree\n".to_vec();
    let transfer_id = 1u32;

    let expected_logs: Vec<Vec<u8>> = file
        .split(|b| *b == b'\n')
        .filter(|line| !line.is_empty())
        .map(|line| line[..line.len().min(8)].to_vec())
        .collect();

    let expected_displayer_sent: Vec<Vec<u8>> = {
        let mut out = Vec::new();
        let req_body = file_transfer::encode_req(transfer_id, file.len() as u32);
        let req_id = <atlas::FileReq as thincan::Message>::ID;
        out.push([&req_id.to_le_bytes()[..], &req_body[..]].concat());
        for offset in (0..file.len()).step_by(8) {
            let chunk_body =
                file_transfer::encode_chunk(transfer_id, offset as u32, &file[offset..]);
            let chunk_id = <atlas::FileChunk as thincan::Message>::ID;
            out.push([&chunk_id.to_le_bytes()[..], &chunk_body[..]].concat());
        }
        out
    };

    let expected_sender_sent: Vec<Vec<u8>> = expected_logs
        .iter()
        .map(|line| {
            let body = log_tx::encode_line(line);
            let log_id = <atlas::LogLine as thincan::Message>::ID;
            [&log_id.to_le_bytes()[..], &body[..]].concat()
        })
        .collect();

    let displayer_sent = Arc::new(Mutex::new(Vec::<Vec<u8>>::new()));
    let displayer_received = Arc::new(Mutex::new(Vec::<Vec<u8>>::new()));
    let sender_sent = Arc::new(Mutex::new(Vec::<Vec<u8>>::new()));
    let sender_received = Arc::new(Mutex::new(Vec::<Vec<u8>>::new()));

    let file_for_sender = file.clone();
    let file_for_displayer = file.clone();
    let expected_logs_for_sender = expected_logs.clone();

    let sender_sent_t = sender_sent.clone();
    let sender_received_t = sender_received.clone();
    let sender_fut = async move {
        let mut rx_buf = [0u8; 256];
        let node_sender = IsoTpAsyncNode::with_clock(
            tx_b,
            rx_b,
            cfg(0x701, 0x700),
            TokioClock,
            &mut rx_buf,
        )
        .unwrap();

        let rt = TokioRuntime;
        let node_sender = can_iso_tp::AsyncWithRt::new(&rt, node_sender);
        let node_sender = LoggingTransport::new(node_sender, sender_sent_t, sender_received_t);

        let mut tx_buf = [0u8; 512];
        let mut iface =
            thincan::Interface::new(node_sender, log_sender_bus::Router::new(), &mut tx_buf);
        let mut handlers = log_sender_bus::HandlersImpl {
            app: NoApp,
            file_transfer: file_transfer::State::new(RecordingFileStore::default()),
            log_tx: log_tx::DefaultHandlers,
        };

        let mut rx_payload = [0u8; 512];
        let mut dispatch = DispatchCounts::default();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        let committed = loop {
            if tokio::time::Instant::now() > deadline {
                return Err(thincan::Error {
                    kind: thincan::ErrorKind::Timeout,
                });
            }
            match iface
                .recv_one_dispatch_meta_async(
                    &mut handlers,
                    Duration::from_millis(50),
                    &mut rx_payload,
                )
                .await?
            {
                thincan::RecvDispatch::TimedOut => continue,
                thincan::RecvDispatch::MalformedPayload => {
                    return Err(thincan::Error {
                        kind: thincan::ErrorKind::Other,
                    });
                }
                thincan::RecvDispatch::Dispatched { id, kind } => match kind {
                    thincan::RecvDispatchKind::Handled => dispatch.handled += 1,
                    thincan::RecvDispatchKind::Unhandled => {
                        panic!("sender received unhandled message id=0x{:04x}", id);
                    }
                    thincan::RecvDispatchKind::Unknown => {
                        panic!("sender received unknown message id=0x{:04x}", id);
                    }
                },
            }
            if let Some(transfer) = handlers.file_transfer.store.take_latest() {
                break transfer;
            }
        };

        assert_eq!(
            committed.transfer_id, transfer_id,
            "sender committed unexpected transfer id"
        );
        assert_eq!(
            committed.total_len as usize,
            file_for_sender.len(),
            "sender committed unexpected total len"
        );
        assert_eq!(
            committed.bytes, file_for_sender,
            "sender stored file bytes differ from sent bytes"
        );
        assert_eq!(
            handlers.file_transfer.store.begin_calls,
            vec![(transfer_id, committed.total_len)],
            "sender should begin exactly one write with the correct parameters"
        );
        assert_eq!(
            handlers.file_transfer.store.commit_calls, 1,
            "sender should commit exactly once"
        );
        assert_eq!(
            handlers.file_transfer.store.abort_calls, 0,
            "sender should not abort transfers in this scenario"
        );

        let expected_chunk_count = (committed.total_len as usize).div_ceil(8);
        assert_eq!(
            committed.writes.len(),
            expected_chunk_count,
            "expected one write_at per chunk"
        );
        for (i, op) in committed.writes.iter().enumerate() {
            assert_eq!(
                op.offset as usize,
                i * 8,
                "write_at offset should be chunk-aligned and increasing"
            );
            let expected = &committed.bytes
                [(i * 8)..((i * 8 + op.bytes.len()).min(committed.bytes.len()))];
            assert_eq!(op.bytes, expected, "write_at bytes should match source slice");
        }

        let mut logs_sent = 0usize;
        for line in committed.bytes.split(|b| *b == b'\n') {
            if line.is_empty() {
                continue;
            }
            logs_sent += 1;
            iface.send_msg_to_async::<atlas::LogLine>(0, 
                &log_tx::encode_line(line),
                Duration::from_millis(50),
            )
            .await?;
        }
        assert_eq!(
            logs_sent,
            expected_logs_for_sender.len(),
            "sender should send one log line per non-empty file line"
        );

        Ok::<(CommittedTransfer, DispatchCounts), thincan::Error>((committed, dispatch))
    };

    let expected_log_count = expected_logs.len();
    let displayer_sent_t = displayer_sent.clone();
    let displayer_received_t = displayer_received.clone();
    let displayer_fut = async move {
        let mut rx_buf = [0u8; 256];
        let node_displayer = IsoTpAsyncNode::with_clock(
            tx_a,
            rx_a,
            cfg(0x700, 0x701),
            TokioClock,
            &mut rx_buf,
        )
        .unwrap();

        let rt = TokioRuntime;
        let node_displayer = can_iso_tp::AsyncWithRt::new(&rt, node_displayer);
        let node_displayer = LoggingTransport::new(node_displayer, displayer_sent_t, displayer_received_t);

        let mut tx_buf = [0u8; 512];
        let mut iface = thincan::Interface::new(
            node_displayer,
            log_displayer_bus::Router::new(),
            &mut tx_buf,
        );
        let displayer_store = RecordingFileStore::default();
        let mut handlers = log_displayer_bus::HandlersImpl {
            app: NoApp,
            file_transfer: file_transfer::State::new(displayer_store),
            log_display: log_display::State {
                sink: CollectingLogSink::default(),
            },
        };

        iface
            .send_msg_to_async::<atlas::FileReq>(0, 
                &file_transfer::encode_req(transfer_id, file_for_displayer.len() as u32),
                Duration::from_millis(50),
            )
            .await?;
        for offset in (0..file_for_displayer.len()).step_by(8) {
            let body = file_transfer::encode_chunk(
                transfer_id,
                offset as u32,
                &file_for_displayer[offset..],
            );
            iface
                .send_msg_to_async::<atlas::FileChunk>(0, &body, Duration::from_millis(50))
                .await?;
        }

        let mut dispatch = DispatchCounts::default();
        let mut rx_payload = [0u8; 512];
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        while handlers.log_display.sink.lines.len() < expected_log_count {
            if tokio::time::Instant::now() > deadline {
                return Err(thincan::Error {
                    kind: thincan::ErrorKind::Timeout,
                });
            }
            match iface
                .recv_one_dispatch_meta_async(
                    &mut handlers,
                    Duration::from_millis(50),
                    &mut rx_payload,
                )
                .await?
            {
                thincan::RecvDispatch::TimedOut => continue,
                thincan::RecvDispatch::MalformedPayload => {
                    return Err(thincan::Error {
                        kind: thincan::ErrorKind::Other,
                    });
                }
                thincan::RecvDispatch::Dispatched { kind, .. } => match kind {
                    thincan::RecvDispatchKind::Handled => {
                        dispatch.handled += 1;
                        assert!(
                            handlers.log_display.sink.lines.len() <= expected_log_count,
                            "displayer received more logs than expected"
                        );
                    }
                    thincan::RecvDispatchKind::Unhandled => {
                        panic!("displayer received unhandled message")
                    }
                    thincan::RecvDispatchKind::Unknown => {
                        panic!("displayer received unknown message")
                    }
                },
            }
        }

        for _ in 0..3 {
            let r = iface
                .recv_one_dispatch_meta_async(&mut handlers, Duration::from_millis(50), &mut rx_payload)
                .await?;
            assert_eq!(r, thincan::RecvDispatch::TimedOut);
        }

        let store = handlers.file_transfer.store;
        Ok::<(Vec<Vec<u8>>, DispatchCounts, RecordingFileStore), thincan::Error>((
            handlers.log_display.sink.lines,
            dispatch,
            store,
        ))
    };

    let ((committed, sender_dispatch), (logs, displayer_dispatch, displayer_store)) =
        tokio::try_join!(sender_fut, displayer_fut)?;

    assert_eq!(
        logs, expected_logs,
        "displayer should collect log lines derived from transferred file"
    );

    assert_eq!(
        sender_dispatch.unhandled, 0,
        "sender should not receive unhandled messages in this scenario"
    );
    assert_eq!(
        sender_dispatch.unknown, 0,
        "sender should not receive unknown messages in this scenario"
    );
    assert_eq!(
        sender_dispatch.handled,
        expected_displayer_sent.len(),
        "sender should handle exactly the file-transfer messages"
    );

    assert_eq!(
        displayer_dispatch.unhandled, 0,
        "displayer should not receive unhandled messages in this scenario"
    );
    assert_eq!(
        displayer_dispatch.unknown, 0,
        "displayer should not receive unknown messages in this scenario"
    );
    assert_eq!(
        displayer_dispatch.handled,
        expected_sender_sent.len(),
        "displayer should handle exactly the log messages"
    );

    assert_eq!(
        displayer_store.begin_calls.len(),
        0,
        "displayer should not receive file-transfer messages in this scenario"
    );
    assert_eq!(
        displayer_store.commit_calls, 0,
        "displayer should not commit any file writes in this scenario"
    );
    assert_eq!(
        displayer_store.abort_calls, 0,
        "displayer should not abort any file writes in this scenario"
    );

    assert_eq!(
        committed.transfer_id, transfer_id,
        "sanity check: committed transfer id"
    );
    assert_eq!(committed.bytes, file, "sanity check: committed file bytes");

    let displayer_sent = displayer_sent.lock().unwrap().clone();
    let displayer_received = displayer_received.lock().unwrap().clone();
    let sender_sent = sender_sent.lock().unwrap().clone();
    let sender_received = sender_received.lock().unwrap().clone();

    assert_eq!(
        displayer_sent, expected_displayer_sent,
        "displayer should send FileReq followed by all FileChunk payloads"
    );
    assert_eq!(
        sender_received, expected_displayer_sent,
        "sender should receive exactly the file-transfer payloads"
    );

    assert_eq!(
        sender_sent, expected_sender_sent,
        "sender should send one LogLine payload per expected log line"
    );
    assert_eq!(
        displayer_received, expected_sender_sent,
        "displayer should receive exactly the log payloads"
    );

    for payload in displayer_sent.iter().chain(sender_sent.iter()) {
        let raw = thincan::decode_wire(payload)?;
        match raw.id {
            <atlas::FileReq as thincan::Message>::ID => {
                assert_eq!(raw.body.len(), atlas::FileReq::BODY_LEN)
            }
            <atlas::FileChunk as thincan::Message>::ID => {
                assert_eq!(raw.body.len(), atlas::FileChunk::BODY_LEN)
            }
            <atlas::LogLine as thincan::Message>::ID => {
                assert_eq!(raw.body.len(), atlas::LogLine::BODY_LEN)
            }
            other => panic!("unexpected payload on wire id=0x{:04x}", other),
        }
    }

    Ok(())
}
