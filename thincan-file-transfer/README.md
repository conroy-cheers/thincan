# thincan-file-transfer

`thincan-file-transfer` provides a reusable file-transfer bundle for `thincan`.

It includes message codecs, transfer state handling, and storage-facing traits for receiver implementations.

## Async migration in 0.2

A bundle instance needs only `IsoTpAsyncWrite`. `send_file`,
`send_file_with_id`, `recv_file` and `recv_file_no_alloc` take the writer's
absolute deadline. For upgrade steps from 0.1, see
[the migration guide](../docs/migration-0.2.md).

### Timeouts

Each timeout has one job:

| Timeout | Kind | Bounds |
|---|---|---|
| `deadline` argument | absolute | the whole call, on both sides. Every send gets it |
| `SendConfig::ack_timeout` | relative, per window | the sender's wait for an ACK. At expiry the sender retransmits the window, at most `max_retries` times |
| `ReceiverConfig::idle_timeout` | relative, restarts for each chunk | the receiver's wait for the next chunk. At expiry the receiver aborts the transfer |
| `set_time_wait` (per bundle) | relative, from completion | how long the bundle remembers a finished transfer (see below) |

Only the deadline bounds the receiver's wait for the offer and the sender's wait
for Accept. So a listener can pass a long deadline and still detect failures
once a transfer starts. The relative timers run on the deadline's own clock
(`Deadline::after`), and the deadline caps them. Retries never extend the
deadline.

The sender sends an unanswered offer again only after
`SendConfig::offer_retry_interval`, not after `ack_timeout`, because the
receiver may still be preparing storage before it accepts. The default interval
is 10 s, and `None` sends the offer once. The sender repeats an offer at most
`max_retries` times, as 0.1 senders did. Then it waits for Accept until the
deadline. The limit matters because each repeat holds a slot in the receiver's
offer mailbox until the receiver reads it, and a full mailbox makes the
receiver's ingest fail.

Receivers from this release discard queued repeats of an offer they handled. A
0.1 receiver does not, and would later read a repeat as a new transfer. With
0.1 receivers that prepare storage slowly, keep the interval above that time or
use `None`.

The timers are local and do not change the wire format. Keep the receiver's
`idle_timeout` (default 10 s) longer than the sender's retransmission interval
plus the time to send one window. For a sender from this release, that interval
is `ack_timeout` (default 3 s). For a 0.1 sender, it is the per-operation
timeout passed to it. When the idle timeout expires, the receiver sends
`Abort`. Every release treats `Abort` as fatal, so a stalled sender stops
promptly.

### Completion and TIME_WAIT

`Complete` is the receiver's last message. If a sender misses it, the sender
retransmits into silence and fails a transfer that actually arrived. To prevent
this, offers from this release set `FileReq.confirmsComplete`, and the sender
confirms `Complete` with an empty `FileChunk` at offset `totalLen`.

The receiver remembers each finished transfer for the bundle's `set_time_wait`
period, like TCP's TIME_WAIT. It records the peer, the transfer id, the length
and the file hash. The default period is 30 s, and zero turns TIME_WAIT off.
The receiver drops an offer that exactly repeats a remembered transfer. Any
other offer from that peer ends TIME_WAIT for that peer, so a restarted sender
can reuse a transfer id for a different file.

If a sender sends the same file again under a reused id, it waits until the
entry expires, because the receiver cannot tell it from a repeat.

To answer a sender that missed `Complete` without blocking the caller, run
`serve` next to the receive pump for the life of the session:

```rust,ignore
tokio::select! {
    result = application => result,
    served = bundles.file_transfer.serve() => /* interface closed or failed */,
}
```

While `serve` runs, `recv_file` returns as soon as it has sent `Complete`.
`serve` then answers each retransmitted final chunk with `Complete`, until the
sender confirms or the entry expires. `serve` waits on the bus only while a
transfer is unconfirmed, which normally lasts one round trip. During that time
it holds one `FileChunk` mailbox waiter, so keep `MAX_WAITERS` at 2 or more.
When idle, it holds none.

Without `serve`, `recv_file` waits for the confirmation itself and answers
retransmissions in the meantime. It waits for at most `idle_timeout`, or the
default idle timeout when that is `None`. Then it returns success, because the
file is already committed. So a lost confirmation can delay the caller's next
operation by up to that time. In request/response use, that delay can exceed a
peer's deadline.

This works with older peers in both directions:

- The flag fits in spare bits of the offer, so the encoded offer keeps its
  size.
- 0.1 receivers ignore the flag. They discard the confirmation as a chunk of a
  finished transfer.
- `serve` also answers 0.1 senders, which never confirm, until the entry
  expires.
- Without `serve`, receivers do not wait for 0.1 senders. A lost `Complete`
  then fails the transfer on the sender, as before.

### Upgrading from 0.1

In 0.1, the timeout restarted for every operation. When you upgrade, pass the
old per-operation value as `ack_timeout` or `idle_timeout`. Use the deadline for
the overall bound you want, including time spent listening.

Transport adapters never replay accepted commands.

### Errors

Sender errors are `SendFileError<Tx::Error>`, which keeps protocol failures
apart from `SendMessageError` codec and transport failures. Receiver errors are
`Error<StoreError, Tx::Error>`, with separate `Store`, `Receive`, `Send`,
`Protocol` and `Capnp` variants. If a required ACK fails, the error keeps the
transport cause, and the receiver aborts an open store write. On invalid input,
the receiver may try to send a best-effort rejection ACK. The error still
reports the original protocol failure.
