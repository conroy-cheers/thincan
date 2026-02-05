# embedded-can-unix-socket

A Unix-domain-socket CAN bus simulator that implements `embedded-can-interface` traits.

This crate provides:
- A **server** process (`BusServer`) that hosts a shared, simulated CAN bus.
- A **client** interface (`UnixCan`) that connects to the server and implements
  `TxFrameIo` / `RxFrameIo` (plus filter configuration).

It is intended for local integration tests and tooling where you want multiple
processes to communicate over a CAN-like bus without hardware.

## Quick start

```rust
use embedded_can::{Frame as _, Id, StandardId};
use embedded_can_interface::{RxFrameIo, TxFrameIo};
use embedded_can_unix_socket::{BusServer, UnixCan, UnixFrame};
use std::time::Duration;

let path = std::env::temp_dir().join("can-unix-socket-example.sock");
let _ = std::fs::remove_file(&path);
let mut server = BusServer::start(&path).unwrap();

let mut a = UnixCan::connect(&path).unwrap();
let mut b = UnixCan::connect(&path).unwrap();

let frame = UnixFrame::new(Id::Standard(StandardId::new(0x123).unwrap()), &[1, 2]).unwrap();
a.send(&frame).unwrap();
assert_eq!(b.recv_timeout(Duration::from_millis(10)).unwrap(), frame);

server.shutdown().unwrap();
```

## Behavior notes

- **Arbitration**: the server batches contenders within a short window and
  chooses the dominant CAN identifier (plus RTR ordering) before broadcasting.
- **Broadcast**: frames are delivered to all connected clients, including the
  sender, matching CAN semantics.
- **Filters**: acceptance filters are applied on the server side, and updates
  are acknowledged to keep ordering with in-flight traffic.
- **ACKs**: blocking sends wait for at least one other client to be connected;
  if no other client exists, the sender receives `NoAck`.

## Testing

Run the crate tests:

```bash
cargo test -p embedded-can-unix-socket
```
