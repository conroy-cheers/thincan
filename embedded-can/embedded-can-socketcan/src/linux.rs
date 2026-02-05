use core::time::Duration;
use embedded_can_interface::{
    BlockingControl, FilterConfig, IdMask, IdMaskFilter, RxFrameIo, TxFrameIo,
};
use socketcan::{
    CanAnyFrame, CanFdSocket, CanFilter, CanFrame, CanSocket, Error as SocketcanError, IoResult,
    Socket, SocketOptions,
};
use std::os::unix::io::{AsRawFd, RawFd};

/// Classic CAN SocketCAN adapter implementing `embedded-can-interface` traits.
#[derive(Debug)]
pub struct SocketCan {
    inner: CanSocket,
    nonblocking: bool,
}

impl SocketCan {
    /// Open a SocketCAN interface by name (e.g. `"can0"`).
    pub fn open(iface: &str) -> Result<Self, SocketcanError> {
        let inner = CanSocket::open(iface)?;
        let nonblocking = inner.nonblocking().map_err(SocketcanError::from)?;
        Ok(Self { inner, nonblocking })
    }

    /// Wrap an existing `socketcan::CanSocket`.
    pub fn new(inner: CanSocket) -> Self {
        let nonblocking = inner.nonblocking().ok().unwrap_or(false);
        Self { inner, nonblocking }
    }

    /// Borrow the inner SocketCAN socket.
    pub fn as_inner(&self) -> &CanSocket {
        &self.inner
    }

    /// Mutably borrow the inner SocketCAN socket.
    pub fn as_inner_mut(&mut self) -> &mut CanSocket {
        &mut self.inner
    }

    /// Unwrap into the inner SocketCAN socket.
    pub fn into_inner(self) -> CanSocket {
        self.inner
    }
}

impl AsRawFd for SocketCan {
    fn as_raw_fd(&self) -> RawFd {
        self.inner.as_raw_fd()
    }
}

/// CAN-FD SocketCAN adapter implementing `embedded-can-interface` traits.
#[derive(Debug)]
pub struct SocketCanFd {
    inner: CanFdSocket,
    nonblocking: bool,
}

impl SocketCanFd {
    /// Open a SocketCAN interface by name (e.g. `"can0"`).
    pub fn open(iface: &str) -> Result<Self, SocketcanError> {
        let inner = CanFdSocket::open(iface)?;
        let nonblocking = inner.nonblocking().map_err(SocketcanError::from)?;
        Ok(Self { inner, nonblocking })
    }

    /// Wrap an existing `socketcan::CanFdSocket`.
    pub fn new(inner: CanFdSocket) -> Self {
        let nonblocking = inner.nonblocking().ok().unwrap_or(false);
        Self { inner, nonblocking }
    }

    /// Borrow the inner SocketCAN socket.
    pub fn as_inner(&self) -> &CanFdSocket {
        &self.inner
    }

    /// Mutably borrow the inner SocketCAN socket.
    pub fn as_inner_mut(&mut self) -> &mut CanFdSocket {
        &mut self.inner
    }

    /// Unwrap into the inner SocketCAN socket.
    pub fn into_inner(self) -> CanFdSocket {
        self.inner
    }
}

impl AsRawFd for SocketCanFd {
    fn as_raw_fd(&self) -> RawFd {
        self.inner.as_raw_fd()
    }
}

fn map_can_frame(frame: CanFrame) -> Result<CanFrame, SocketcanError> {
    match frame {
        CanFrame::Error(frame) => Err(frame.into_error().into()),
        frame => Ok(frame),
    }
}

fn map_any_frame(frame: CanAnyFrame) -> Result<CanAnyFrame, SocketcanError> {
    match frame {
        CanAnyFrame::Error(frame) => Err(frame.into_error().into()),
        frame => Ok(frame),
    }
}

fn with_nonblocking<S, T>(
    socket: &mut S,
    nonblocking: &mut bool,
    f: impl FnOnce(&mut S) -> IoResult<T>,
) -> Result<T, SocketcanError>
where
    S: Socket,
{
    if *nonblocking {
        return f(socket).map_err(SocketcanError::from);
    }

    let was_nonblocking = socket.nonblocking()?;
    if was_nonblocking {
        *nonblocking = true;
        return f(socket).map_err(SocketcanError::from);
    }

    socket.set_nonblocking(true)?;
    let result = f(socket).map_err(SocketcanError::from);
    if let Err(err) = socket.set_nonblocking(false).map_err(SocketcanError::from) {
        return Err(err);
    }
    result
}

fn with_write_timeout<S, T>(
    socket: &mut S,
    timeout: Duration,
    f: impl FnOnce(&mut S) -> IoResult<T>,
) -> Result<T, SocketcanError>
where
    S: Socket,
{
    let prev = socket.write_timeout()?;
    socket.set_write_timeout(Some(timeout))?;
    let result = f(socket).map_err(SocketcanError::from);
    let reset = socket.set_write_timeout(prev).map_err(SocketcanError::from);
    match reset {
        Ok(()) => result,
        Err(err) => Err(err),
    }
}

fn wait_for_readable(fd: RawFd) -> Result<(), SocketcanError> {
    let mut fds = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };

    loop {
        let res = unsafe { libc::poll(&mut fds, 1, -1) };
        if res >= 0 {
            return Ok(());
        }

        let err = std::io::Error::last_os_error();
        if err.kind() == std::io::ErrorKind::Interrupted {
            continue;
        }
        return Err(err.into());
    }
}

fn to_socketcan_filter(f: &IdMaskFilter) -> Result<CanFilter, SocketcanError> {
    // SocketCAN raw filters match `received_id & mask == filter_id & mask` where IDs include flag
    // bits (EFF/RTR/ERR). Ensure we match the intended frame format by including EFF in the mask.
    let (id_raw, mask_raw) = match (f.id, f.mask) {
        (embedded_can_interface::Id::Standard(id), IdMask::Standard(mask)) => {
            let can_id = u32::from(id.as_raw());
            let can_mask = u32::from(mask) | (libc::CAN_EFF_FLAG as u32);
            (can_id, can_mask)
        }
        (embedded_can_interface::Id::Extended(id), IdMask::Extended(mask)) => {
            let can_id = id.as_raw() | (libc::CAN_EFF_FLAG as u32);
            let can_mask = (mask & 0x1FFF_FFFF) | (libc::CAN_EFF_FLAG as u32);
            (can_id, can_mask)
        }
        _ => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "id/mask width mismatch",
            )
            .into());
        }
    };
    Ok(CanFilter::new(id_raw, mask_raw))
}

impl TxFrameIo for SocketCan {
    type Frame = CanFrame;
    type Error = SocketcanError;

    fn send(&mut self, frame: &Self::Frame) -> Result<(), Self::Error> {
        self.inner
            .write_frame_insist(frame)
            .map_err(SocketcanError::from)
    }

    fn try_send(&mut self, frame: &Self::Frame) -> Result<(), Self::Error> {
        with_nonblocking(&mut self.inner, &mut self.nonblocking, |socket| {
            socket.write_frame(frame)
        })
    }

    fn send_timeout(&mut self, frame: &Self::Frame, timeout: Duration) -> Result<(), Self::Error> {
        with_write_timeout(&mut self.inner, timeout, |socket| socket.write_frame(frame))
    }
}

impl RxFrameIo for SocketCan {
    type Frame = CanFrame;
    type Error = SocketcanError;

    fn recv(&mut self) -> Result<Self::Frame, Self::Error> {
        let frame = self.inner.read_frame().map_err(SocketcanError::from)?;
        map_can_frame(frame)
    }

    fn try_recv(&mut self) -> Result<Self::Frame, Self::Error> {
        let frame = with_nonblocking(&mut self.inner, &mut self.nonblocking, |socket| {
            socket.read_frame()
        })?;
        map_can_frame(frame)
    }

    fn recv_timeout(&mut self, timeout: Duration) -> Result<Self::Frame, Self::Error> {
        let frame = self
            .inner
            .read_frame_timeout(timeout)
            .map_err(SocketcanError::from)?;
        map_can_frame(frame)
    }

    fn wait_not_empty(&mut self) -> Result<(), Self::Error> {
        wait_for_readable(self.as_raw_fd())
    }
}

impl BlockingControl for SocketCan {
    type Error = SocketcanError;

    fn set_nonblocking(&mut self, on: bool) -> Result<(), Self::Error> {
        self.inner
            .set_nonblocking(on)
            .map_err(SocketcanError::from)?;
        self.nonblocking = on;
        Ok(())
    }
}

impl FilterConfig for SocketCan {
    type Error = SocketcanError;
    type FiltersHandle<'a>
        = ()
    where
        Self: 'a;

    fn set_filters(&mut self, filters: &[IdMaskFilter]) -> Result<(), Self::Error> {
        if filters.is_empty() {
            // `embedded-can-interface` generally treats an empty list as “accept all”.
            return self
                .inner
                .set_filter_accept_all()
                .map_err(SocketcanError::from);
        }
        let mut out: Vec<CanFilter> = Vec::with_capacity(filters.len());
        for f in filters {
            out.push(to_socketcan_filter(f)?);
        }
        self.inner.set_filters(&out).map_err(SocketcanError::from)
    }

    fn modify_filters(&mut self) -> Self::FiltersHandle<'_> {
        ()
    }
}

impl TxFrameIo for SocketCanFd {
    type Frame = CanAnyFrame;
    type Error = SocketcanError;

    fn send(&mut self, frame: &Self::Frame) -> Result<(), Self::Error> {
        self.inner
            .write_frame_insist(frame)
            .map_err(SocketcanError::from)
    }

    fn try_send(&mut self, frame: &Self::Frame) -> Result<(), Self::Error> {
        with_nonblocking(&mut self.inner, &mut self.nonblocking, |socket| {
            socket.write_frame(frame)
        })
    }

    fn send_timeout(&mut self, frame: &Self::Frame, timeout: Duration) -> Result<(), Self::Error> {
        with_write_timeout(&mut self.inner, timeout, |socket| socket.write_frame(frame))
    }
}

impl RxFrameIo for SocketCanFd {
    type Frame = CanAnyFrame;
    type Error = SocketcanError;

    fn recv(&mut self) -> Result<Self::Frame, Self::Error> {
        let frame = self.inner.read_frame().map_err(SocketcanError::from)?;
        map_any_frame(frame)
    }

    fn try_recv(&mut self) -> Result<Self::Frame, Self::Error> {
        let frame = with_nonblocking(&mut self.inner, &mut self.nonblocking, |socket| {
            socket.read_frame()
        })?;
        map_any_frame(frame)
    }

    fn recv_timeout(&mut self, timeout: Duration) -> Result<Self::Frame, Self::Error> {
        let frame = self
            .inner
            .read_frame_timeout(timeout)
            .map_err(SocketcanError::from)?;
        map_any_frame(frame)
    }

    fn wait_not_empty(&mut self) -> Result<(), Self::Error> {
        wait_for_readable(self.as_raw_fd())
    }
}

impl BlockingControl for SocketCanFd {
    type Error = SocketcanError;

    fn set_nonblocking(&mut self, on: bool) -> Result<(), Self::Error> {
        self.inner
            .set_nonblocking(on)
            .map_err(SocketcanError::from)?;
        self.nonblocking = on;
        Ok(())
    }
}

impl FilterConfig for SocketCanFd {
    type Error = SocketcanError;
    type FiltersHandle<'a>
        = ()
    where
        Self: 'a;

    fn set_filters(&mut self, filters: &[IdMaskFilter]) -> Result<(), Self::Error> {
        if filters.is_empty() {
            return self
                .inner
                .set_filter_accept_all()
                .map_err(SocketcanError::from);
        }
        let mut out: Vec<CanFilter> = Vec::with_capacity(filters.len());
        for f in filters {
            out.push(to_socketcan_filter(f)?);
        }
        self.inner.set_filters(&out).map_err(SocketcanError::from)
    }

    fn modify_filters(&mut self) -> Self::FiltersHandle<'_> {
        ()
    }
}

#[cfg(feature = "tokio")]
#[path = "tokio_impl.rs"]
mod tokio_impl;

#[cfg(feature = "tokio")]
pub use tokio_impl::{TokioSocketCan, TokioSocketCanFd};
