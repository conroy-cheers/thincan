#![warn(missing_docs)]

//! Linux SocketCAN adapters implementing `embedded-can-interface` traits.
//!
//! On Linux, this crate wraps the [`socketcan`] crate's sockets in small newtypes that implement
//! the `embedded-can-interface` I/O traits (`TxFrameIo`, `RxFrameIo`, etc).
//!
//! On non-Linux targets, the types are present but constructors return
//! [`UnsupportedPlatformError`].

#[cfg(target_os = "linux")]
mod linux;

#[cfg(target_os = "linux")]
pub use linux::*;

#[cfg(not(target_os = "linux"))]
mod non_linux;

#[cfg(not(target_os = "linux"))]
pub use non_linux::*;
