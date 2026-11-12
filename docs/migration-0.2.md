# Migrating to the 0.2 release

This guide covers the upgrade to the coordinated async release from `thincan`
0.1, `thincan-file-transfer` 0.1, `can-isotp-queue` 0.2 and
`linux-socketcan-iso-tp` 0.1. [CHANGELOG.md](../CHANGELOG.md) lists every
change. [The design notes](async-transport-refactor.md) give the reasons.

Summary of the changes:

- Each transport splits into a reader and a writer. A send takes one
  **absolute deadline**. A receive waits indefinitely.
- A `thincan` interface owns only the writer. A `ReceivePump` owns the reader.
- File transfer takes the same absolute deadline. It has a liveness timer for
  each transfer and an optional `serve()` future.
- The wire format did not change, so 0.1 and 0.2 peers work together. See
  [Mixed 0.1 and 0.2 fleets](#7-mixed-01-and-02-fleets).

## 1. Dependencies

```toml
can-isotp-interface = { version = "0.2", features = ["tokio"] } # or "embassy"
thincan = { version = "0.2", features = ["std", "capnp"] }
thincan-file-transfer = { version = "0.2", features = ["tokio"] } # or "embassy"
can-isotp-queue = { version = "0.3", features = ["tokio"] }       # if you use it
linux-socketcan-iso-tp = { version = "0.2", features = ["tokio"] } # if you use it
can-iso-tp = { version = "0.3", features = ["async-driver"] }      # user-space ISO-TP
```

The `tokio` and `embassy` features of `thincan-file-transfer` no longer pull in
`tokio` or `embassy-time`. Its timers run on the deadline clock of your writer.

## 2. Deadlines

Every async send now takes the writer's `Deadline` type instead of a relative
`Duration`. A `Deadline` is an **absolute** point in time. Create it once, where
an operation starts, and pass the same value down. No layer below resets it.

```rust,ignore
// 0.1
bundles.file_transfer.send_file(peer, &bytes, Duration::from_secs(5), config).await?;

// 0.2 (Tokio)
let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
bundles.file_transfer.send_file(peer, &bytes, deadline, config).await?;
```

The `can-isotp-interface` features implement `Deadline` for
`tokio::time::Instant` and `embassy_time::Instant`. For a custom clock,
implement the trait yourself: `remaining`, `wait` and `after`.

## 3. `thincan`: interfaces and the receive pump

An interface now holds only the writer. A `ReceivePump` does the receiving. Run
it for the life of the session.

```rust,ignore
// 0.1: one combined endpoint, and a hand-written receive loop
let iface = maplet::Interface::<NoopRawMutex, _, _, DEPTH, MAX_BODY, WAITERS>::new(
    node.clone(), node.clone(), vec![0; TX_LEN],
);
loop {
    node.recv_one(timeout, |meta, payload| { /* iface.bus().ingest(...) */ }).await?;
}

// 0.2: a writer for the interface, a reader for the pump
let iface = maplet::Interface::<NoopRawMutex, _, _, DEPTH, MAX_BODY, WAITERS>::new(
    writer, vec![0; TX_LEN],
);
let bundles = maplet::Bundles::new(&iface);
let mut scratch = [0; MAX_PAYLOAD];
let mut pump = thincan::ReceivePump::new(reader, &iface, &mut scratch);
let pump = async {
    loop {
        match pump.next().await {
            Ok(_) => {}
            Err(error) if error.is_terminal() => break error, // session over
            Err(error) => log::warn!("dropped payload: {error:?}"), // keep going
        }
    }
};
// Run `pump` next to your application, e.g. in a select/join.
```

Other changes:

- `Interface::new_shared`, `node_mut` and `rx_node_mut` are removed. Use
  `tx_node_mut`, or call `into_tx` after you drop all bus handles.
- `ReceivePump::next` and `PumpError` replace `pump_ingest_once`,
  `IngestPumpStatus` and `IngestPumpError`.
- Async sends return `SendMessageError<Tx::Error>`. Its variants are
  `Encode(thincan::Error)` and `Transport(TransmitError<Tx::Error>)`, which
  carries the writer's own error.
- `Interface::close()` ends a session. Pending sends, the pump and mailbox
  waiters then return `Closed`. A fatal transport error or a dropped pump also
  closes the interface.
- Bundle instances written with `bundle_instance!` forward the writer's
  deadline and return `SendMessageError<Node::Error>`. See the macro's docs.

## 4. `thincan-file-transfer`

### Calls

```rust,ignore
// 0.1
let mut bundles = maplet::Bundles::new(&iface);
bundles.file_transfer.send_file(peer, &bytes, timeout, send_config).await?;
bundles.file_transfer.recv_file(peer, &mut store, timeout, receiver_config).await?;
bundles.file_transfer.recv_file_no_alloc::<_, MAX_METADATA, MAX_CHUNK>(/* ... */).await?;

// 0.2
let bundles = maplet::Bundles::new(&iface); // send_file takes &self now
bundles.file_transfer.send_file(peer, &bytes, deadline, send_config).await?;
bundles.file_transfer.recv_file(peer, &mut store, deadline, receiver_config).await?;
bundles.file_transfer.recv_file_no_alloc::<_, MAX_METADATA>(/* ... */).await?;
```

- `send_state()` returns a copy of `SendState`, not a reference.
- `RecvFileResult` and `RecvFileResultNoAlloc<N>` keep their names and fields.
  Both are now aliases of `RecvFileResultOf<M>`.
- `recv_file_no_alloc` no longer takes `MAX_CHUNK`. To limit the chunk size,
  set `ReceiverConfig::max_chunk_size`.

### Errors

- A send fails with `SendFileError<Tx::Error>`. Call `is_timeout()` instead of
  checking for `thincan::ErrorKind::Timeout`.
- A receive fails with `Error<StoreError, Tx::Error>`, which has two new
  variants:
  - `Send(SendMessageError<_>)`: the receiver could not send an ack.
  - `Receive(thincan::Error)`: a wait failed. Timeouts are in this group.

### Timeouts

In 0.1, one `Duration` restarted for every wait, so it worked as an inactivity
timeout. 0.2 has a separate setting for each job:

| Setting | Bounds | Default |
|---|---|---|
| `deadline` argument | the whole call, including the wait for the offer or for Accept | none |
| `SendConfig::ack_timeout` | the sender's wait for an ACK. At expiry the sender retransmits the window | 3 s |
| `SendConfig::offer_retry_interval` | the sender's wait before it sends an unanswered offer again, at most `max_retries` times. `None` sends the offer once | 10 s |
| `ReceiverConfig::idle_timeout` | the receiver's wait for the next chunk. At expiry the receiver aborts the transfer | 10 s |
| `set_time_wait` (per bundle) | how long the bundle remembers a finished transfer | 30 s |

To upgrade:

- Put your old timeout value in `ack_timeout` on the sender, or in
  `idle_timeout` on the receiver.
- Set the deadline to the total time you allow for the call. Include any time
  spent listening.
- Add the new fields, or `..Default::default()`, to `SendConfig` and
  `ReceiverConfig` literals.

### Completion and `serve`

The receiver now confirms the end of a transfer more reliably. Run `serve()`
next to the receive pump. Then, if a sender misses `Complete`, `serve` answers
it and your `recv_file` calls do not wait.

```rust,ignore
tokio::select! {
    error = pump => /* session over */,
    served = bundles.file_transfer.serve() => /* interface closed or failed */,
    out = application(&bundles) => /* ... */,
}
```

Without `serve`, `recv_file` waits for the sender's confirmation after each
transfer, for at most `idle_timeout`. If the confirmation is lost, your next
call starts late. This matters in request/response use.

While `serve` watches a transfer, it holds at most one mailbox waiter. Set the
interface's `MAX_WAITERS` to 2 or more.

## 5. `can-isotp-queue` 0.2 → 0.3

Separate transmit and receive handles replace the combined endpoint.

```rust,ignore
// Tokio
use can_isotp_queue::tokio::{make_endpoints, run_actor};
let (tx, rx, ports) = make_endpoints::<RxError, TxError, Deadline>(MAX_PAYLOAD, 8, 8)?;
let actor = run_actor(reader, writer, ports, ActorConfig::default(), hooks, shutdown);
// `tx` is cloneable (an `IsoTpAsyncWrite`); `rx` has one owner (an `IsoTpAsyncRead`).

// Embassy (fixed storage)
static RESOURCES: QueueResources<RM, RxError, TxError, Deadline, MAX_PAYLOAD, TX_SLOTS, RX_DEPTH> =
    QueueResources::new();
let (tx, rx, ports) = RESOURCES.split();
let actor = can_isotp_queue::embassy::run_actor(reader, writer, ports, config, hooks, shutdown);
```

- `split_endpoints`, `ActorPorts`, the `IsoTp*` request, response and event
  types, `QueueIsoTpError`, `with_fixed_reply_to`, `with_allowed_reply_from`
  and `normalized_tx_burst_limit` are removed. Peer addresses come from the
  backend.
- `ActorHooks::on_queue_backpressure(QueueKind, Duration)` is now
  `on_rx_backpressure(Duration)`.
- `run_actor` returns `Result<ActorStop, ActorError<_, _>>`. A fatal backend
  error goes back to the owner of the actor, and callers see `Closed`.
- If a request expires while it is in the queue, both runtimes now report it to
  `on_tx_result` as `Timeout`.

## 6. `linux-socketcan-iso-tp` 0.1 → 0.2

```rust,ignore
// 0.2
let closer = SocketCloser::new(16, 2)?; // bounded, off-executor kernel close
let options = IsoTpKernelOptions {
    // Receive flow control is fixed when the socket is bound: set it here.
    flow_control: Some(IsoTpFlowControlOptions::new(block_size, st_min_raw, wft_max)),
    ..Default::default()
};
let parts = TokioSocketCanIsoTp::open_peer(&closer, "can0", local, peer, &options)?
    .into_split();
let iface = maplet::Interface::new(parts.writer, vec![0; TX_LEN]);
let mut pump = thincan::ReceivePump::new(parts.reader, &iface, &mut scratch);
// `parts.control.failed().await` resolves when the socket fails or closes,
// with the first fatal error if there was one.
```

- `inner`, `inner_mut` and `into_inner` are removed.
- `SocketCanIsoTp` and the Tokio socket no longer have `set_rx_flow_control`.
  Linux rejects that change on a bound socket (`EISCONN`), so the method never
  took effect. Set `IsoTpKernelOptions::flow_control` when you open the socket.
- `KernelUdsDemux::set_rx_flow_control` still works. It reopens the demux's
  peer sockets, which drops transfers in progress on those sockets.
- `open_with_ids` takes a `SocketRoute` that gives the peer address and the
  destination, because raw CAN IDs carry no addressing.
- Transaction errors and a full transmit queue (`ENOBUFS`) are recoverable
  `Rejected` errors. Unknown errors are fatal and end both halves.

## 7. Mixed 0.1 and 0.2 fleets

The wire format did not change, so a 0.2 node can talk to a 0.1 node in both
directions. A 0.2 node does more to recover lost messages. Use these settings
to keep that safe with 0.1 peers:

- Run `serve` on 0.2 receivers. `serve` also answers 0.1 senders that missed
  `Complete`. Without it, a 0.1 sender that loses `Complete` fails the
  transfer, as in 0.1.
- Set `offer_retry_interval` longer than the time your 0.1 receivers need to
  prepare storage, or set it to `None`. A 0.1 receiver reads a repeated offer
  as a new transfer and starts a phantom transfer. 0.2 receivers drop repeats.
- Set `idle_timeout` on 0.2 receivers longer than the timeout of a 0.1 sender
  plus the time to send one window. A 0.1 sender waits for its full
  per-operation timeout before it retransmits.
- Make `set_time_wait` cover the whole retry window of the sender. For a 0.2
  sender this is about `ack_timeout` × (`max_retries` + 1). For a 0.1 sender it
  is about the sender's timeout × (retries + 1).

Known limitations with 0.1 peers:

- If the `Complete` from a 0.1 receiver is lost, a 0.2 sender cannot recover.
  The file arrives, but the transfer fails on the sender, as in 0.1.
- A sender can restart and send the same file again under a reused transfer
  id. The receiver cannot tell this from a repeat, so the sender waits until
  the receiver's TIME_WAIT entry expires.

## 8. Benchmark example

`file-transfer-bench-a` no longer has the `--offer-retries` and
`--offer-retry-delay-ms` flags, because the library now repeats offers itself.
Remove those flags from your scripts.
