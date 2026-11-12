# linux-socketcan-iso-tp

Blocking kernel ISO-TP sockets, and optional Tokio read and write halves with
addressing.

```rust,ignore
let closer = SocketCloser::new(16, 2)?;
let parts = TokioSocketCanIsoTp::open_peer(&closer, "can0", local, peer, &options)?
    .into_split();
let interface = maplet::Interface::new(parts.writer, tx_buffer);
let mut pump = thincan::ReceivePump::new(parts.reader, &interface, &mut rx_buffer);
// The session owner supervises pump.next(), protocol tasks, and parts.control.
```

`open_peer` sets up normal-fixed UDS addressing. `open_with_ids` needs an
explicit `SocketRoute`, which gives the source metadata and the configured
physical or functional destination. The socket rejects a mismatched
destination before it transmits.

Set kernel options when you open the socket, including receive flow control
(`IsoTpKernelOptions::flow_control`). Linux rejects changes to ISO-TP options
after a socket is bound (`EISCONN`). To apply a change,
`KernelUdsDemux::set_rx_flow_control` reopens its peer sockets. The Tokio socket
rejects `CAN_ISOTP_WAIT_TX_DONE`, because the send syscall can then block an
executor thread.

## Errors and waits

A receive waits indefinitely for the socket to become readable or to report an
error. If a pending receive is cancelled, the next datagram is kept. `MSG_TRUNC`
consumes an oversized datagram, also when the destination buffer is empty. EOF
is fatal.

The socket classifies errors as follows:

- The transaction errnos `ETIMEDOUT`, `EILSEQ`, `EBADMSG`, `ECOMM` and
  `EMSGSIZE` are recoverable.
- On send, `EINVAL` and `ENOBUFS` (CAN transmit queue full) reject that send.
- On receive, `EINVAL` is fatal.
- Unknown errors are fatal.

A send waits for the socket to become writable or to report an error. It checks
its absolute deadline just before the syscall that accepts the data. Accepted
work can finish after that, and success does not confirm delivery. The two
halves share the socket error, so either operation can observe it.

`SocketControl::failed()` keeps the first fatal cause that is discovered, with
the operation that saw it. Both halves then become unusable, and pending I/O
wakes. The control handle observes discovered failures. It does not own the
file descriptor.

## Deferred close

`SocketCloser` uses admission permits to limit the number of live, queued and
closing sockets. When the last data half is dropped, it deregisters its
`AsyncFd` and submits the kernel close to a fixed number of standard worker
threads. Nothing waits for a kernel close or joins workers on the executor.
Existing sockets keep the submission service after the public closer is
dropped. A permit is released only after the close.

Before you reopen a socket for the same peer generation, wait for
`control.closed()` within your session's cleanup budget. A stuck kernel close
keeps its file descriptor and its permit. When the capacity runs out, the
result is `Error::CloseCapacity`. The async adapter does not limit how long a
kernel close takes.

## Validation

Unit tests exercise actual `AsyncFd` behavior with Unix datagrams. Regression
tests on a real vcan interface cover:

- full-duplex segmentation
- transaction recovery
- deferred close while a PDU waits for flow control, including executor
  responsiveness and the return of capacity
- recovery on the same socket after a padding failure (`EBADMSG`) and after a
  flow-control overflow (`EMSGSIZE`)
- a 1,000-cycle open/reject/close test, which checks that every close permit
  returns

Run them on an isolated interface with `CAP_NET_RAW`:

```sh
THINCAN_TEST_VCAN_IFACE=vcan_test cargo test -p linux-socketcan-iso-tp \
  --features tokio --test event_vcan -- --ignored
```
