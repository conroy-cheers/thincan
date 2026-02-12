//! Linux kernel ISO-TP socket backend.
//!
//! On Linux this provides a minimal wrapper around `CAN_ISOTP` sockets and implements the
//! `can-isotp-interface` traits.
//!
//! On non-Linux targets this crate provides compile-time stubs so downstream crates can build
//! with appropriate `cfg` gating.

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
pub use linux::*;

#[cfg(not(target_os = "linux"))]
mod non_linux;
#[cfg(not(target_os = "linux"))]
pub use non_linux::*;
