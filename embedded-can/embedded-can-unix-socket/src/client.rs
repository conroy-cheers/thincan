use crate::frame::UnixFrame;
use crate::wire::{
    ACK_BAD_FRAME, ACK_NO_ACK, ACK_OK, ACK_SERVER_ERR, MAX_DATA_LEN, MSG_ACK, MSG_FILTERS_ACK,
    MSG_FRAME, MSG_HELLO, MSG_SEND_FRAME, MSG_SET_FILTERS, SEND_FRAME_HDR_LEN, decode_ack,
    decode_frame, encode_filters, encode_send_frame_into,
};
use embedded_can::ErrorKind;
use embedded_can_interface::{FilterConfig, IdMaskFilter, RxFrameIo, TxFrameIo};
use std::collections::VecDeque;
use std::fmt;
use std::io;
use std::io::{Read, Write};
use std::os::unix::io::AsRawFd;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::{Duration, Instant};

/// Errors produced by the Unix-domain CAN client.
#[derive(Debug)]
pub enum UnixCanError {
    /// I/O error from the underlying Unix socket.
    Io(io::Error),
    /// Malformed or unexpected protocol message.
    Protocol(&'static str),
    /// Operation timed out waiting for data.
    Timeout,
    /// Operation would block in nonblocking mode.
    WouldBlock,
    /// Server disconnected unexpectedly.
    Disconnected,
    /// Frame was not acknowledged by any other node.
    NoAck,
}

impl fmt::Display for UnixCanError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            UnixCanError::Io(err) => write!(f, "io error: {err}"),
            UnixCanError::Protocol(msg) => write!(f, "protocol error: {msg}"),
            UnixCanError::Timeout => write!(f, "timeout"),
            UnixCanError::WouldBlock => write!(f, "would block"),
            UnixCanError::Disconnected => write!(f, "disconnected"),
            UnixCanError::NoAck => write!(f, "no ack"),
        }
    }
}

impl std::error::Error for UnixCanError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            UnixCanError::Io(err) => Some(err),
            _ => None,
        }
    }
}

impl embedded_can::Error for UnixCanError {
    fn kind(&self) -> ErrorKind {
        match self {
            UnixCanError::NoAck => ErrorKind::Acknowledge,
            UnixCanError::WouldBlock => ErrorKind::Other,
            UnixCanError::Timeout => ErrorKind::Other,
            UnixCanError::Disconnected => ErrorKind::Other,
            UnixCanError::Protocol(_) => ErrorKind::Other,
            UnixCanError::Io(_) => ErrorKind::Other,
        }
    }
}

impl From<io::Error> for UnixCanError {
    fn from(err: io::Error) -> Self {
        if err.kind() == io::ErrorKind::WouldBlock {
            UnixCanError::WouldBlock
        } else if err.kind() == io::ErrorKind::TimedOut {
            UnixCanError::Timeout
        } else {
            UnixCanError::Io(err)
        }
    }
}

/// Client-side CAN interface connected to a Unix-domain bus server.
pub struct UnixCan {
    stream: UnixStream,
    rx_queue: VecDeque<UnixFrame>,
    rx_bytes: Vec<u8>,
    rx_off: usize,
    next_seq: u64,
    filters: Vec<IdMaskFilter>,
}

#[derive(Debug, Clone, Copy)]
enum DecodedMsg {
    Frame(UnixFrame),
    Ack { seq: u64, status: u8 },
    FiltersAck { seq: u64, status: u8 },
    Hello,
    Unknown,
}

impl UnixCan {
    /// Connect to a running bus server at the provided socket path.
    pub fn connect(path: impl AsRef<Path>) -> io::Result<Self> {
        let stream = UnixStream::connect(path)?;
        stream.set_nonblocking(true)?;
        let mut iface = Self {
            stream,
            rx_queue: VecDeque::new(),
            rx_bytes: Vec::new(),
            rx_off: 0,
            next_seq: 1,
            filters: Vec::new(),
        };
        iface.wait_for_hello().map_err(|err| match err {
            UnixCanError::Io(err) => err,
            _ => io::Error::new(io::ErrorKind::Other, "handshake failed"),
        })?;
        Ok(iface)
    }

    /// Replace the acceptance filters on this interface.
    pub fn set_filters(&mut self, filters: &[IdMaskFilter]) -> Result<(), UnixCanError> {
        self.filters.clear();
        self.filters.extend_from_slice(filters);
        self.send_filters()
    }

    fn send_filters(&mut self) -> Result<(), UnixCanError> {
        let seq = self.next_seq();
        let payload = encode_filters(seq, &self.filters);
        self.write_msg_deadline(MSG_SET_FILTERS, &payload, None)?;
        self.wait_for_filters_ack(seq, None)
    }

    fn next_seq(&mut self) -> u64 {
        let seq = self.next_seq;
        self.next_seq = self.next_seq.wrapping_add(1);
        seq
    }

    fn deadline_after(timeout: Option<Duration>) -> Option<Instant> {
        timeout.map(|t| Instant::now() + t)
    }

    fn remaining_ms(deadline: Option<Instant>) -> Result<i32, UnixCanError> {
        match deadline {
            None => Ok(-1),
            Some(d) => {
                let now = Instant::now();
                if now >= d {
                    return Err(UnixCanError::Timeout);
                }
                let ms = d.duration_since(now).as_millis();
                Ok(ms.min(i32::MAX as u128) as i32)
            }
        }
    }

    fn wait_readable(&self, deadline: Option<Instant>) -> Result<(), UnixCanError> {
        let fd = self.stream.as_raw_fd();
        let mut fds = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        loop {
            let timeout_ms = Self::remaining_ms(deadline)?;
            let res = unsafe { libc::poll(&mut fds, 1, timeout_ms) };
            if res > 0 {
                return Ok(());
            }
            if res == 0 {
                return Err(UnixCanError::Timeout);
            }
            let err = io::Error::last_os_error();
            if err.kind() != io::ErrorKind::Interrupted {
                return Err(UnixCanError::Io(err));
            }
        }
    }

    fn wait_writable(&self, deadline: Option<Instant>) -> Result<(), UnixCanError> {
        let fd = self.stream.as_raw_fd();
        let mut fds = libc::pollfd {
            fd,
            events: libc::POLLOUT,
            revents: 0,
        };
        loop {
            let timeout_ms = Self::remaining_ms(deadline)?;
            let res = unsafe { libc::poll(&mut fds, 1, timeout_ms) };
            if res > 0 {
                return Ok(());
            }
            if res == 0 {
                return Err(UnixCanError::Timeout);
            }
            let err = io::Error::last_os_error();
            if err.kind() != io::ErrorKind::Interrupted {
                return Err(UnixCanError::Io(err));
            }
        }
    }

    fn read_more_once(&mut self) -> Result<bool, UnixCanError> {
        let mut buf = [0u8; 4096];
        match self.stream.read(&mut buf) {
            Ok(0) => Err(UnixCanError::Disconnected),
            Ok(n) => {
                self.rx_bytes.extend_from_slice(&buf[..n]);
                Ok(true)
            }
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => Ok(false),
            Err(err) => Err(UnixCanError::from(err)),
        }
    }

    fn with_next_msg<R>(
        &mut self,
        f: impl FnOnce(u8, &[u8]) -> Result<R, UnixCanError>,
    ) -> Result<Option<R>, UnixCanError> {
        let avail = self.rx_bytes.len().saturating_sub(self.rx_off);
        if avail < 5 {
            return Ok(None);
        }
        let base = self.rx_off;
        let msg_type = self.rx_bytes[base];
        let len =
            u32::from_le_bytes(self.rx_bytes[base + 1..base + 5].try_into().unwrap()) as usize;
        if len > crate::wire::MAX_PAYLOAD_LEN {
            return Err(UnixCanError::Protocol("payload exceeds limit"));
        }
        let total = 5 + len;
        if avail < total {
            return Ok(None);
        }

        let payload = &self.rx_bytes[base + 5..base + total];
        let out = f(msg_type, payload)?;

        self.rx_off += total;
        if self.rx_off >= 4096 && self.rx_off >= (self.rx_bytes.len() / 2) {
            self.rx_bytes.drain(..self.rx_off);
            self.rx_off = 0;
        }
        Ok(Some(out))
    }

    fn recv_decoded_msg_blocking(
        &mut self,
        deadline: Option<Instant>,
    ) -> Result<DecodedMsg, UnixCanError> {
        loop {
            if let Some(msg) = self.next_decoded_msg()? {
                return Ok(msg);
            }
            if !self.read_more_once()? {
                self.wait_readable(deadline)?;
            }
        }
    }

    fn write_all_deadline(
        &mut self,
        mut bytes: &[u8],
        deadline: Option<Instant>,
    ) -> Result<(), UnixCanError> {
        while !bytes.is_empty() {
            match self.stream.write(bytes) {
                Ok(0) => return Err(UnixCanError::Disconnected),
                Ok(n) => bytes = &bytes[n..],
                Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                    self.wait_writable(deadline)?
                }
                Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
                Err(err) => return Err(UnixCanError::from(err)),
            }
        }
        Ok(())
    }

    fn write_msg_deadline(
        &mut self,
        msg_type: u8,
        payload: &[u8],
        deadline: Option<Instant>,
    ) -> Result<(), UnixCanError> {
        if payload.len() > u32::MAX as usize {
            return Err(UnixCanError::Protocol("payload too large"));
        }
        let mut header = [0u8; 5];
        header[0] = msg_type;
        header[1..5].copy_from_slice(&(payload.len() as u32).to_le_bytes());
        self.write_all_deadline(&header, deadline)?;
        self.write_all_deadline(payload, deadline)?;
        Ok(())
    }

    fn wait_for_hello(&mut self) -> Result<(), UnixCanError> {
        loop {
            match self.recv_decoded_msg_blocking(None)? {
                DecodedMsg::Hello => return Ok(()),
                DecodedMsg::Frame(frame) => self.rx_queue.push_back(frame),
                DecodedMsg::Ack { .. } | DecodedMsg::FiltersAck { .. } | DecodedMsg::Unknown => {}
            }
        }
    }

    fn wait_for_filters_ack(
        &mut self,
        seq: u64,
        timeout: Option<Duration>,
    ) -> Result<(), UnixCanError> {
        let deadline = Self::deadline_after(timeout);
        loop {
            match self.recv_decoded_msg_blocking(deadline)? {
                DecodedMsg::FiltersAck {
                    seq: ack_seq,
                    status,
                } if ack_seq == seq => match status {
                    ACK_OK => return Ok(()),
                    ACK_SERVER_ERR => return Err(UnixCanError::Protocol("server error")),
                    _ => return Err(UnixCanError::Protocol("filters ack failed")),
                },
                DecodedMsg::Frame(frame) => self.rx_queue.push_back(frame),
                DecodedMsg::Hello
                | DecodedMsg::Ack { .. }
                | DecodedMsg::FiltersAck { .. }
                | DecodedMsg::Unknown => {}
            }
        }
    }

    fn wait_for_ack(&mut self, seq: u64, timeout: Option<Duration>) -> Result<(), UnixCanError> {
        let deadline = Self::deadline_after(timeout);
        loop {
            match self.recv_decoded_msg_blocking(deadline)? {
                DecodedMsg::Ack {
                    seq: ack_seq,
                    status,
                } if ack_seq == seq => match status {
                    ACK_OK => return Ok(()),
                    ACK_NO_ACK => return Err(UnixCanError::NoAck),
                    ACK_BAD_FRAME => return Err(UnixCanError::Protocol("bad frame")),
                    ACK_SERVER_ERR => return Err(UnixCanError::Protocol("server error")),
                    _ => return Err(UnixCanError::Protocol("unknown ack status")),
                },
                DecodedMsg::Frame(frame) => self.rx_queue.push_back(frame),
                DecodedMsg::Hello
                | DecodedMsg::FiltersAck { .. }
                | DecodedMsg::Ack { .. }
                | DecodedMsg::Unknown => {}
            }
        }
    }

    fn next_decoded_msg(&mut self) -> Result<Option<DecodedMsg>, UnixCanError> {
        self.with_next_msg(|msg_type, payload| {
            Ok(match msg_type {
                MSG_FRAME => {
                    DecodedMsg::Frame(decode_frame(payload).map_err(UnixCanError::Protocol)?)
                }
                MSG_ACK => {
                    let (seq, status) = decode_ack(payload).map_err(UnixCanError::Protocol)?;
                    DecodedMsg::Ack { seq, status }
                }
                MSG_FILTERS_ACK => {
                    let (seq, status) = decode_ack(payload).map_err(UnixCanError::Protocol)?;
                    DecodedMsg::FiltersAck { seq, status }
                }
                MSG_HELLO => DecodedMsg::Hello,
                _ => DecodedMsg::Unknown,
            })
        })
    }
}

impl TxFrameIo for UnixCan {
    type Frame = UnixFrame;
    type Error = UnixCanError;

    fn send(&mut self, frame: &Self::Frame) -> Result<(), Self::Error> {
        let seq = self.next_seq();
        let mut payload = [0u8; SEND_FRAME_HDR_LEN + MAX_DATA_LEN];
        let len = encode_send_frame_into(&mut payload, seq, true, frame);
        self.write_msg_deadline(MSG_SEND_FRAME, &payload[..len], None)?;
        self.wait_for_ack(seq, None)
    }

    fn try_send(&mut self, frame: &Self::Frame) -> Result<(), Self::Error> {
        let seq = self.next_seq();
        let mut payload = [0u8; SEND_FRAME_HDR_LEN + MAX_DATA_LEN];
        let len = encode_send_frame_into(&mut payload, seq, false, frame);
        // For throughput-oriented use (e.g. ISO-TP), we prefer to block briefly rather than
        // surface WouldBlock errors to upper layers that don't retry.
        self.write_msg_deadline(MSG_SEND_FRAME, &payload[..len], None)
    }

    fn send_timeout(&mut self, frame: &Self::Frame, timeout: Duration) -> Result<(), Self::Error> {
        let seq = self.next_seq();
        let mut payload = [0u8; SEND_FRAME_HDR_LEN + MAX_DATA_LEN];
        let len = encode_send_frame_into(&mut payload, seq, true, frame);
        let deadline = Some(Instant::now() + timeout);
        self.write_msg_deadline(MSG_SEND_FRAME, &payload[..len], deadline)?;
        self.wait_for_ack(seq, Some(timeout))
    }
}

impl RxFrameIo for UnixCan {
    type Frame = UnixFrame;
    type Error = UnixCanError;

    fn recv(&mut self) -> Result<Self::Frame, Self::Error> {
        if let Some(frame) = self.rx_queue.pop_front() {
            return Ok(frame);
        }
        loop {
            match self.recv_decoded_msg_blocking(None)? {
                DecodedMsg::Frame(frame) => return Ok(frame),
                DecodedMsg::Hello
                | DecodedMsg::Ack { .. }
                | DecodedMsg::FiltersAck { .. }
                | DecodedMsg::Unknown => {}
            }
        }
    }

    fn try_recv(&mut self) -> Result<Self::Frame, Self::Error> {
        if let Some(frame) = self.rx_queue.pop_front() {
            return Ok(frame);
        }

        let _ = self.read_more_once()?;
        while let Some(msg) = self.next_decoded_msg()? {
            match msg {
                DecodedMsg::Frame(frame) => return Ok(frame),
                _ => continue,
            }
        }
        Err(UnixCanError::WouldBlock)
    }

    fn recv_timeout(&mut self, timeout: Duration) -> Result<Self::Frame, Self::Error> {
        if let Some(frame) = self.rx_queue.pop_front() {
            return Ok(frame);
        }
        let deadline = Some(Instant::now() + timeout);
        loop {
            match self.recv_decoded_msg_blocking(deadline)? {
                DecodedMsg::Frame(frame) => return Ok(frame),
                DecodedMsg::Hello
                | DecodedMsg::Ack { .. }
                | DecodedMsg::FiltersAck { .. }
                | DecodedMsg::Unknown => {}
            }
        }
    }

    fn wait_not_empty(&mut self) -> Result<(), Self::Error> {
        if !self.rx_queue.is_empty() {
            return Ok(());
        }
        self.wait_readable(None)
    }
}

impl FilterConfig for UnixCan {
    type Error = UnixCanError;

    type FiltersHandle<'a>
        = FilterHandle<'a>
    where
        Self: 'a;

    fn set_filters(&mut self, filters: &[IdMaskFilter]) -> Result<(), Self::Error> {
        UnixCan::set_filters(self, filters)
    }

    fn modify_filters(&mut self) -> Self::FiltersHandle<'_> {
        FilterHandle { iface: self }
    }
}

/// Mutable filter handle that syncs filters back to the server on drop.
pub struct FilterHandle<'a> {
    iface: &'a mut UnixCan,
}

impl<'a> Drop for FilterHandle<'a> {
    fn drop(&mut self) {
        let _ = self.iface.send_filters();
    }
}

impl<'a> std::ops::Deref for FilterHandle<'a> {
    type Target = Vec<IdMaskFilter>;

    fn deref(&self) -> &Self::Target {
        &self.iface.filters
    }
}

impl<'a> std::ops::DerefMut for FilterHandle<'a> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.iface.filters
    }
}
