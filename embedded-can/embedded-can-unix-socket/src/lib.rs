#![warn(missing_docs)]

//! Unix-domain-socket CAN bus simulator with `embedded-can-interface` support.
//!
//! This crate provides a server process that hosts a simulated CAN bus and multiple
//! client interfaces that connect over a Unix-domain socket. Each client implements
//! the `embedded-can-interface` traits so it can be used with protocol layers like
//! ISO-TP.
//!
//! # Quick start
//! ```rust
//! use embedded_can::{Frame as _, Id, StandardId};
//! use embedded_can_interface::{RxFrameIo, TxFrameIo};
//! use embedded_can_unix_socket::{BusServer, UnixCan, UnixFrame};
//! use std::time::Duration;
//!
//! let path = std::env::temp_dir().join("can-unix-socket-example.sock");
//! let _ = std::fs::remove_file(&path);
//! let mut server = BusServer::start(&path).unwrap();
//!
//! let mut a = UnixCan::connect(&path).unwrap();
//! let mut b = UnixCan::connect(&path).unwrap();
//!
//! let frame = UnixFrame::new(Id::Standard(StandardId::new(0x123).unwrap()), &[1, 2]).unwrap();
//! a.send(&frame).unwrap();
//! assert_eq!(b.recv_timeout(Duration::from_millis(10)).unwrap(), frame);
//!
//! server.shutdown().unwrap();
//! ```
//!
//! # Notes
//! - Filters are applied on the server and acknowledged back to the client to ensure
//!   ordering with in-flight traffic.

mod client;
mod frame;
mod server;
mod wire;

pub use crate::client::{FilterHandle, UnixCan, UnixCanError};
pub use crate::frame::UnixFrame;
pub use crate::server::BusServer;
