use core::time::Duration;
use embedded_can_interface::{AsyncRxFrameIo, AsyncTxFrameIo, FilterConfig, IdMask, IdMaskFilter};
use socketcan::tokio::{CanFdSocket, CanSocket};
use socketcan::{CanAnyFrame, CanFilter, CanFrame, Error as SocketcanError, SocketOptions};

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

fn to_socketcan_filter(f: &IdMaskFilter) -> Result<CanFilter, SocketcanError> {
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

/// Tokio classic CAN SocketCAN adapter implementing `embedded-can-interface` async traits.
#[derive(Debug)]
pub struct TokioSocketCan {
    inner: CanSocket,
}

impl TokioSocketCan {
    /// Open a SocketCAN interface by name (e.g. `"can0"`).
    pub fn open(iface: &str) -> Result<Self, SocketcanError> {
        Ok(Self {
            inner: CanSocket::open(iface)?,
        })
    }

    /// Wrap an existing `socketcan::tokio::CanSocket`.
    pub fn new(inner: CanSocket) -> Self {
        Self { inner }
    }

    /// Borrow the inner socket.
    pub fn as_inner(&self) -> &CanSocket {
        &self.inner
    }

    /// Mutably borrow the inner socket.
    pub fn as_inner_mut(&mut self) -> &mut CanSocket {
        &mut self.inner
    }

    /// Unwrap into the inner socket.
    pub fn into_inner(self) -> CanSocket {
        self.inner
    }
}

/// Tokio CAN-FD SocketCAN adapter implementing `embedded-can-interface` async traits.
#[derive(Debug)]
pub struct TokioSocketCanFd {
    inner: CanFdSocket,
}

impl TokioSocketCanFd {
    /// Open a SocketCAN interface by name (e.g. `"can0"`).
    pub fn open(iface: &str) -> Result<Self, SocketcanError> {
        Ok(Self {
            inner: CanFdSocket::open(iface)?,
        })
    }

    /// Wrap an existing `socketcan::tokio::CanFdSocket`.
    pub fn new(inner: CanFdSocket) -> Self {
        Self { inner }
    }

    /// Borrow the inner socket.
    pub fn as_inner(&self) -> &CanFdSocket {
        &self.inner
    }

    /// Mutably borrow the inner socket.
    pub fn as_inner_mut(&mut self) -> &mut CanFdSocket {
        &mut self.inner
    }

    /// Unwrap into the inner socket.
    pub fn into_inner(self) -> CanFdSocket {
        self.inner
    }
}

impl AsyncTxFrameIo for TokioSocketCan {
    type Frame = CanFrame;
    type Error = SocketcanError;

    async fn send(&mut self, frame: &Self::Frame) -> Result<(), Self::Error> {
        self.inner
            .write_frame(frame.clone())
            .await
            .map_err(SocketcanError::from)
    }

    async fn send_timeout(
        &mut self,
        frame: &Self::Frame,
        _timeout: Duration,
    ) -> Result<(), Self::Error> {
        // Higher layers (e.g. can-iso-tp) typically apply their own timeouts via the runtime.
        self.send(frame).await
    }
}

impl AsyncRxFrameIo for TokioSocketCan {
    type Frame = CanFrame;
    type Error = SocketcanError;

    async fn recv(&mut self) -> Result<Self::Frame, Self::Error> {
        let frame = self
            .inner
            .read_frame()
            .await
            .map_err(SocketcanError::from)?;
        map_can_frame(frame)
    }

    async fn recv_timeout(&mut self, _timeout: Duration) -> Result<Self::Frame, Self::Error> {
        self.recv().await
    }

    async fn wait_not_empty(&mut self) -> Result<(), Self::Error> {
        Ok(())
    }
}

impl FilterConfig for TokioSocketCan {
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

impl AsyncTxFrameIo for TokioSocketCanFd {
    type Frame = CanAnyFrame;
    type Error = SocketcanError;

    async fn send(&mut self, frame: &Self::Frame) -> Result<(), Self::Error> {
        self.inner
            .write_frame(frame)
            .await
            .map_err(SocketcanError::from)
    }

    async fn send_timeout(
        &mut self,
        frame: &Self::Frame,
        _timeout: Duration,
    ) -> Result<(), Self::Error> {
        self.send(frame).await
    }
}

impl AsyncRxFrameIo for TokioSocketCanFd {
    type Frame = CanAnyFrame;
    type Error = SocketcanError;

    async fn recv(&mut self) -> Result<Self::Frame, Self::Error> {
        let frame = self
            .inner
            .read_frame()
            .await
            .map_err(SocketcanError::from)?;
        map_any_frame(frame)
    }

    async fn recv_timeout(&mut self, _timeout: Duration) -> Result<Self::Frame, Self::Error> {
        self.recv().await
    }

    async fn wait_not_empty(&mut self) -> Result<(), Self::Error> {
        Ok(())
    }
}

impl FilterConfig for TokioSocketCanFd {
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
