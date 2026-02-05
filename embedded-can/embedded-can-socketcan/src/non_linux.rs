use core::fmt;

/// Returned when attempting to use SocketCAN adapters on a non-Linux target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UnsupportedPlatformError;

impl fmt::Display for UnsupportedPlatformError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "embedded-can-socketcan is only supported on Linux targets"
        )
    }
}

impl std::error::Error for UnsupportedPlatformError {}

/// Classic CAN SocketCAN adapter (non-Linux stub).
#[derive(Debug, Default)]
pub struct SocketCan;

impl SocketCan {
    /// Always returns [`UnsupportedPlatformError`] on non-Linux targets.
    pub fn open(_iface: &str) -> Result<Self, UnsupportedPlatformError> {
        Err(UnsupportedPlatformError)
    }
}

/// CAN-FD SocketCAN adapter (non-Linux stub).
#[derive(Debug, Default)]
pub struct SocketCanFd;

impl SocketCanFd {
    /// Always returns [`UnsupportedPlatformError`] on non-Linux targets.
    pub fn open(_iface: &str) -> Result<Self, UnsupportedPlatformError> {
        Err(UnsupportedPlatformError)
    }
}

/// Tokio classic CAN SocketCAN adapter (non-Linux stub).
#[cfg(feature = "tokio")]
#[derive(Debug, Default)]
pub struct TokioSocketCan;

/// Tokio CAN-FD SocketCAN adapter (non-Linux stub).
#[cfg(feature = "tokio")]
#[derive(Debug, Default)]
pub struct TokioSocketCanFd;
