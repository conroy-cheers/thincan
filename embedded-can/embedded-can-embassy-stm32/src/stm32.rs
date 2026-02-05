use core::time::Duration;

use embedded_can_interface::{AsyncRxFrameIo, AsyncTxFrameIo};

use embassy_stm32::can;

/// Error type used by the embassy-stm32 CAN adapters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EmbassyStm32CanError {
    /// A bus error reported by the underlying peripheral/driver.
    Bus(can::BusError),
    /// The filter list exceeded peripheral limits.
    TooManyFilters,
    /// A filter had mismatched ID/mask widths (standard vs extended).
    MaskWidthMismatch,
    /// A filter mask value was out of range for its ID width.
    MaskOutOfRange,
}

impl From<can::BusError> for EmbassyStm32CanError {
    fn from(value: can::BusError) -> Self {
        Self::Bus(value)
    }
}

/// embassy-stm32 CAN TX adapter implementing `embedded-can-interface` async traits.
#[derive(Debug)]
pub struct EmbassyStm32Tx<'d> {
    inner: can::CanTx<'d>,
}

impl<'d> EmbassyStm32Tx<'d> {
    /// Wrap an `embassy_stm32::can::CanTx`.
    pub fn new(inner: can::CanTx<'d>) -> Self {
        Self { inner }
    }

    /// Borrow the inner embassy-stm32 TX half.
    pub fn as_inner(&self) -> &can::CanTx<'d> {
        &self.inner
    }

    /// Mutably borrow the inner embassy-stm32 TX half.
    pub fn as_inner_mut(&mut self) -> &mut can::CanTx<'d> {
        &mut self.inner
    }

    /// Unwrap into the inner embassy-stm32 TX half.
    pub fn into_inner(self) -> can::CanTx<'d> {
        self.inner
    }
}

/// embassy-stm32 CAN RX adapter implementing `embedded-can-interface` async traits.
#[derive(Debug)]
pub struct EmbassyStm32Rx<'d> {
    inner: can::CanRx<'d>,
    pending: Option<can::Frame>,
}

impl<'d> EmbassyStm32Rx<'d> {
    /// Wrap an `embassy_stm32::can::CanRx`.
    pub fn new(inner: can::CanRx<'d>) -> Self {
        Self {
            inner,
            pending: None,
        }
    }

    /// Borrow the inner embassy-stm32 RX half.
    pub fn as_inner(&self) -> &can::CanRx<'d> {
        &self.inner
    }

    /// Mutably borrow the inner embassy-stm32 RX half.
    pub fn as_inner_mut(&mut self) -> &mut can::CanRx<'d> {
        &mut self.inner
    }

    /// Unwrap into the inner embassy-stm32 RX half.
    pub fn into_inner(self) -> can::CanRx<'d> {
        self.inner
    }
}

impl<'d> AsyncTxFrameIo for EmbassyStm32Tx<'d> {
    type Frame = can::Frame;
    type Error = EmbassyStm32CanError;

    async fn send(&mut self, frame: &Self::Frame) -> Result<(), Self::Error> {
        let _ = self.inner.write(frame).await;
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

impl<'d> AsyncRxFrameIo for EmbassyStm32Rx<'d> {
    type Frame = can::Frame;
    type Error = EmbassyStm32CanError;

    async fn recv(&mut self) -> Result<Self::Frame, Self::Error> {
        if let Some(frame) = self.pending.take() {
            return Ok(frame);
        }
        let env = self.inner.read().await?;
        Ok(env.frame)
    }

    async fn recv_timeout(&mut self, _timeout: Duration) -> Result<Self::Frame, Self::Error> {
        self.recv().await
    }

    async fn wait_not_empty(&mut self) -> Result<(), Self::Error> {
        if self.pending.is_some() {
            return Ok(());
        }
        // Not all embassy-stm32 CAN implementations expose a separate "wait" primitive.
        // Prefetch one frame so the next `recv()` can return it without additional waiting.
        let env = self.inner.read().await?;
        self.pending = Some(env.frame);
        Ok(())
    }
}
