# Changelog

This file lists the notable changes to the crates in this workspace. The crates
are pre-1.0, so under semver a minor version bump is a breaking change. For
upgrade steps, see [the 0.2 migration guide](docs/migration-0.2.md). For the
reasons behind the transport changes, see
[the async transport design](docs/async-transport-refactor.md).

## Unreleased: coordinated async release

This release replaces the async transport that polled with timeouts. Readers
and writers now wait indefinitely, and each send takes one absolute deadline.

Each crate depends on unpublished versions of the crates before it, so publish
them in this order:

1. `can-isotp-interface` 0.2.0 (separate repository)
2. `can-isotp-queue` 0.3.0 and `linux-socketcan-iso-tp` 0.2.0
3. `can-iso-tp` 0.3.0 (separate repository)
4. `thincan` 0.2.0, then `thincan-file-transfer` 0.2.0

You can publish `embedded-can-unix-socket` 0.1.2 at any point.

[`can-isotp-interface`](https://github.com/conroy-cheers/can-isotp-interface/blob/master/CHANGELOG.md)
and [`can-iso-tp`](https://github.com/conroy-cheers/can-iso-tp/blob/master/CHANGELOG.md)
are in separate repositories and have their own changelogs.

Until these versions are on crates.io, the workspace builds only with local
`[patch.crates-io]` overrides. The top-level README shows the setup.

Every crate in this release declares a minimum supported Rust version of 1.88.

The error and stop enums in these crates are `#[non_exhaustive]`, so later
releases can add variants without a breaking change. Code outside a crate that
matches on one of these enums needs a wildcard arm.

The file-transfer wire format stays compatible with 0.1 peers in both
directions. The `compat-tests` crate checks this end to end against the
published 0.1 release.

### `thincan` 0.2.0

#### Breaking

- Bundles and interfaces take a write-only transport (`IsoTpAsyncWrite`)
  instead of a combined endpoint. `Interface::new(writer, buffer)` replaces
  `Interface::new(tx, rx, buffer)` and `Interface::new_shared`.
- `node_mut` and `rx_node_mut` are removed. Use `tx_node_mut` and `into_tx`.
- A separate `ReceivePump` now owns the reader and does the receiving. Create
  it with `ReceivePump::new(reader, &interface, &mut scratch)` and call
  `next()`.
- `ReceivePump::next` and `PumpError` replace `pump_ingest_once`,
  `IngestPumpStatus` and `IngestPumpError`.
- Async sends take the writer's absolute deadline (`Tx::Deadline`) instead of a
  relative `Duration`. They fail with `SendMessageError<Tx::Error>`, which keeps
  the transport's own error instead of erasing it.
- `ErrorKind`, `IngestError`, `SendMessageError` and `PumpError` are
  `#[non_exhaustive]`.
- The `async` feature, which enabled nothing, is removed.

#### Added

- `Interface::close` and `is_closed`. Closing an interface wakes pending sends,
  the pump and mailbox waiters, and they return `Closed`.
- `PumpError::is_terminal`, which separates errors that end the session from
  per-payload errors. After a per-payload error, keep pumping.
- `Error::closed()`, next to `Error::timeout()`.
- Hidden protocol primitives for bundles: receive from any peer by predicate,
  take a message without blocking, and wait for closure.

#### Changed

- The deadline bounds a send only while it waits for the encoder. After that,
  the send relies on the writer to honour the deadline, as `IsoTpAsyncWrite`
  requires.
- Dropping the `ReceivePump` closes the interface.

### `thincan-file-transfer` 0.2.0

#### Breaking

- `send_file`, `send_file_with_id`, `recv_file` and `recv_file_no_alloc` take
  the writer's absolute deadline instead of a relative `Duration`. The deadline
  covers the whole call, including the wait for the offer.
- `send_file` and `send_file_with_id` take `&self` instead of `&mut self`, so
  they can run next to `serve`.
- `send_state()` returns a copy of `SendState`.
- Sender errors are `SendFileError<Tx::Error>`, which has `is_timeout()`.
- Receiver errors are `Error<StoreError, Tx::Error>`. The new `Send` and
  `Receive` variants carry the transport cause.
- `Error`, `SendFileError` and `ServeError` are `#[non_exhaustive]`.
- `SendConfig` gains `ack_timeout` (default 3 s) and `offer_retry_interval`
  (default 10 s). `max_retries` now also limits how often the sender repeats an
  offer.
- `ReceiverConfig` gains `idle_timeout` (default 10 s). Struct literals need the
  new field or `..Default::default()`.
- `RecvFileResult` and `RecvFileResultNoAlloc<N>` are now aliases of one generic
  type, `RecvFileResultOf<M>`.
- `recv_file_no_alloc` drops its `MAX_CHUNK` parameter, because it no longer
  copies chunks into a buffer. `ReceiverConfig::max_chunk_size` still limits the
  chunk size.
- The bundle instance requires its transport to implement `IsoTpAsyncWrite`.
- The `tokio` feature no longer depends on `tokio`, and the `embassy` feature no
  longer depends on `embassy-time`. Timers run on the writer's deadline clock.
  `embassy-sync` and `embassy-futures` are now optional dependencies, and those
  two features enable them.

#### Added

- A completion handshake. Offers set the new `FileReq.confirmsComplete` flag,
  and the sender confirms `Complete` with an empty chunk at the end of the file.
  The flag uses spare bits, so an encoded offer keeps its size.
- TIME_WAIT. Each bundle remembers finished transfers for the `set_time_wait`
  period (default 30 s). It uses this record to drop repeated offers and to
  answer a sender that missed `Complete`.
- `serve()`, an optional future that runs next to the receive pump. While
  `serve` runs, `recv_file` returns as soon as it sends `Complete`, and `serve`
  answers a lost `Complete` from TIME_WAIT. `ServeError` reports a second
  `serve` call or a bus failure.
- `FileOfferValue::confirming_complete`, `file_ack_complete`, `file_ack_abort`,
  `DEFAULT_RECEIVE_IDLE_TIMEOUT` and `DEFAULT_TIME_WAIT`.

#### Changed (protocol behaviour, wire compatible)

- If no chunk arrives for `idle_timeout`, the receiver aborts the transfer. It
  tells the sender with `Abort`, which every release treats as fatal.
- Without `serve`, `recv_file` waits up to `idle_timeout` for the sender to
  confirm `Complete`. During that wait it answers a retransmitted final chunk.
- If the receiver does not answer an offer, the sender sends it again after
  `offer_retry_interval`, at most `max_retries` times. Then the sender waits for
  Accept until the deadline.
- When a transfer ends, for any reason, the receiver drops queued repeats of
  its offer. The receiver also recognises a repeat of a remembered transfer.
- `Complete` is best-effort after the receiver commits the file. If sending
  `Complete` fails, the receive still succeeds, because the data is stored.
- TIME_WAIT records zero-length transfers like any other transfer.

### `can-isotp-queue` 0.3.0

#### Breaking

- A cloneable `QueueTx` and a uniquely owned `QueueRx` replace the combined
  `QueueIsoTpEndpoint`. The actor gets `DriverPorts`.
  - Tokio: `tokio::make_endpoints(capacity, tx_depth, rx_depth)` and
    `tokio::run_actor(reader, writer, ports, config, hooks, shutdown)`.
  - Embassy: `embassy::QueueResources<…>` with `split()`, `run_actor`, and
    `RequestLease` for user-space drivers.
- Removed `split_endpoints`, `ActorPorts`, `IsoTpTxRequest`, `IsoTpTxResponse`,
  `IsoTpRxEvent`, `QueueIsoTpError`, `with_fixed_reply_to`,
  `with_allowed_reply_from` and `normalized_tx_burst_limit`. Peer addressing
  comes from the backend.
- `ActorHooks::on_queue_backpressure(QueueKind, Duration)` is now
  `on_rx_backpressure(Duration)`. `QueueKind` is removed.
- `RxEvent<E, B>` is one generic event type. `embassy::RxEvent<E, MAX>` is an
  alias with the same variants.
- New shared types: `QueueError`, `ActorError` and `ActorStop` (`Cancelled`,
  `Closed`). All three are `#[non_exhaustive]`.

#### Changed

- Both runtimes now dispatch requests by the same rules:
  - If a request expires in the queue, the actor reports it to `on_tx_result`
    as `Timeout`.
  - The actor skips an abandoned request.
  - If the deadline expires during a send, the result is `Timeout`.
- The Embassy actor times backpressure on the writer's deadline clock. It no
  longer depends on `embassy-time`.
- If the reader returns a receive error kind that this release does not know,
  the actor stops forwarding, as it does for `Closed`.

### `linux-socketcan-iso-tp` 0.2.0

#### Breaking

- The Tokio socket now splits into halves. Open it with
  `TokioSocketCanIsoTp::open_peer` (UDS normal-fixed addressing) or
  `open_with_ids` (with a `SocketRoute`). Then call `into_split()` to get a
  reader, a writer and a `SocketControl`.
- `inner`, `inner_mut` and `into_inner` are removed.
- Opening a socket requires a `SocketCloser`. It closes sockets off the
  executor, on a bounded pool of workers.
- `set_rx_flow_control` is removed from `SocketCanIsoTp` and the Tokio socket.
  Linux rejects changes to ISO-TP options on a bound socket (`EISCONN`), so
  these methods never worked. Set flow control with
  `IsoTpKernelOptions::flow_control` when you open the socket.
- The Tokio socket rejects `CAN_ISOTP_WAIT_TX_DONE`, because a send with that
  flag would block an executor thread.

#### Added

- `SocketControl::failed()` and `closed()`. `TerminalFailure` and `Operation`
  record the first fatal error and the operation that saw it. `Operation` is
  `#[non_exhaustive]`.

#### Fixed

- `KernelUdsDemux::set_rx_flow_control` now applies the new settings. It
  reopens the demux's peer sockets with them, which drops transfers in progress
  on those sockets. Before, it failed silently.
- A full CAN transmit queue (`ENOBUFS`) now rejects only that send. Before, it
  failed the whole socket.

### `embedded-can-unix-socket` 0.1.2

#### Added

- `UnixCan::control()`, which returns an `io::Result<UnixCanControl>`. A
  `UnixCanControl` can shut the connection down to wake a thread that is
  blocked on it.

### `thincan-example` (not published)

- The benchmark binaries share one `run_node` helper. They run `serve` and rely
  on the library to repeat offers.
- bench-a no longer has the `--offer-retries` and `--offer-retry-delay-ms`
  flags.
- The `socketcan-isotp` feature enables the Tokio socket of the kernel backend.
  A test that needs vcan transfers files over kernel ISO-TP sockets.

### `thincan-compat-tests` (new, not published)

- End-to-end tests between a node built from the published 0.1 release and a
  node built from this workspace. The nodes talk over a unix-socket CAN bus.
