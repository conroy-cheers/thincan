# thincan

A small application message protocol and router for ISO-TP transports. The
atlas, bundle and maplet macros keep the existing wire format and bounded
mailboxes.

## Async transports

`Interface::new(writer, tx_buffer)` owns the transmit side and the mailboxes. A
separate `ReceivePump::new(reader, &interface, &mut rx_buffer)` owns the receive
side. The session owner must poll `pump.next()` continuously. It waits
indefinitely, with no idle timer. Construct only one pump for each interface
session.

If `next()` is cancelled while it waits for the mailbox lock, it keeps the
payload it already received. The next call ingests that payload before it
receives again.

After these errors, the pump can take the next input:

- a codec or mailbox rejection that consumed the payload
- `ReceiveError::Rejected`
- `BufferTooSmall`

`Fatal`, `Closed` or an invalid reported length closes the interface. Dropping
the pump also closes it.

`Interface::close()` wakes active sends, an idle receive and mailbox waiters.
After a waiter sees the closure, it rejects buffered data. The session owner
keeps the original backend errors and controls cancellation, cleanup and
reconnection.

The async Cap'n Proto send helpers take `Tx::Deadline` and return
`SendMessageError<Tx::Error>`, which keeps encoding errors apart from classified
transport errors. One absolute deadline covers the TX lock, encoding, the
optional queue and transport acceptance. Success means that the transport
accepted the message, not that the remote peer acknowledged it. The synchronous
send API is still available.

Linux socket halves connect directly, without an adapter. Readers and writers
with separate owners can optionally go through
`can_isotp_queue::tokio::run_actor` or `can_isotp_queue::embassy::run_actor`.
User-space ISO-TP uses `can_iso_tp::driver::run_driver`, which keeps protocol
state independent of callers. [The refactor design](../docs/async-transport-refactor.md)
describes the lifecycle. [The migration guide](../docs/migration-0.2.md) gives
the upgrade steps from 0.1.
