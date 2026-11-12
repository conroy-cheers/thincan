# can-isotp-queue

Optional bounded application ports for ISO-TP readers and writers that have
separate owners. The TX handle is cloneable, and the RX handle has one owner.
Each runtime has one event-driven actor, with no idle receive timeout.

## Tokio

```rust,ignore
use can_isotp_queue::{ActorConfig, NoopHooks};
use can_isotp_queue::tokio::{make_endpoints, run_actor};

let (tx, rx, ports) = make_endpoints::<RxError, TxError, Deadline>(2048, 8, 8)?;
let result = run_actor(reader, writer, ports, ActorConfig::default(),
                       NoopHooks, shutdown).await;
```

`QueueTx` implements `IsoTpAsyncWriteSend`, and `QueueRx` implements
`IsoTpAsyncReadSend`. They get the local capabilities automatically. The
backend's error and deadline types must meet the runtime's `Send` bounds. The
generic runner also supports local backends, without allocating futures.

The original absolute deadline of a send covers admission, queuing, hooks and
acceptance by the backend. The actor skips abandoned and expired requests.
Active waits race against cancellation by the caller, and accepted work can
still finish after cancellation. The actor never replays a request. A
recoverable send rejection keeps its cause. A fatal cause goes back to the owner
in `ActorError`, and callers get `Closed`.

Under RX backpressure the actor pauses reads, but TX and shutdown stay live. A
terminal state that the actor discovers skips the payload queues. After
closure, the queues reject buffered data. The actor detects a receive failure
only when I/O resumes, or when the application monitors the interface.
Addresses come from the backend. Synchronous hooks must not block.

When all TX handles are dropped, admission closes but RX continues. When RX is
dropped, forwarding ends, including an idle receive, but TX continues. An
explicit shutdown or the end of the backend closes both directions.

## Embassy

Enable the `embassy` feature for fixed-storage local ports and `run_actor`.
Declare
`QueueResources<RM, RxError, TxError, Deadline, MAX_PAYLOAD, TX_SLOTS, RX_DEPTH>`
and call `split()` once to get `(tx, rx, driver_ports)`.

Each request has a slot tagged with a generation. The slot has separate wakeups
for completion and cancellation, and separate leases for the caller and the
driver. So a cancelled caller cannot release storage that the driver still
owns, and a late completion cannot satisfy a later request.
`RequestLease::accept()` reports admission but keeps the payload until the
protocol completes. User-space ISO-TP uses these ports directly.

`close()` wakes every operation, including a wait for capacity. A reset needs
exclusive `&mut QueueResources`, after all old handles and leases have ended.
Static allocation does not make a session immortal. `ActorConfig::burst_limit`
limits the work between cooperative yields, including rejected traffic.

`run_actor` times RX-queue backpressure on the writer's deadline clock. So it
needs no time driver beyond the one those deadlines use.
