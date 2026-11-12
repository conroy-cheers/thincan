# Breaking async transport refactor

Status: implemented locally, validated 2026-11-11; coordinated dependencies are not yet published.
This replaces the earlier uncommitted additive implementation.
See [validation evidence](async-transport-validation.md) for the completed checks
and remaining release/deployment boundaries.

## Decision

Use one async read capability and one async write capability, with explicit
local and `Send` variants. Make the protocol interface own its transmitter and
mailboxes; give a separate receive pump exclusive ownership of the receiver.
Keep a queue/actor adapter for applications and backends that need an independent
transport owner. Linux applications can connect socket halves directly.

This refines the earlier proposal to add `Tx` and `Rx` parameters to `Interface`.
There is no need for bundle handles to know the receive transport's type. Keeping
RX in the pump avoids propagating another type parameter through `BusHandle`,
`BundleFactory`, all generated bundle instances, and file-transfer functions.

Use an associated absolute deadline type for sends. Preserve error causes and
classify errors in the operation result, rather than asking a backend error to
classify itself without operation context. Make Linux socket lifetime and fatal
failure observable independently of application payload delivery.

The protocol wire format, message registries, bundle scoping, and bounded
mailboxes continue to serve their existing purposes. Reconnection, generation
numbers, peer availability, and command retry policy belong to the application
session owner.

## Evidence from the current implementation

- `Interface` already separates TX state, RX transport, and mailbox locks.
  Only its RX transport field requires RX and TX to share a `Node` type.
- `BusHandle::ingest` takes the mailbox lock, updates the mailbox, and publishes
  its notification without a subsequent suspension point. This supports a pump
  that retains a received payload across cancellation of a pending ingest.
- `map_isotp_send_error` currently discards backend causes into `ErrorKind::Other`.
  Direct transport use must remove that loss of information.
- The queue actor supplies peer metadata that raw Linux sockets currently report
  as zero. Direct socket use requires moving that information into socket binding.
- The optional queue currently carries two async models: a timed polling actor
  and an independently driven RX/TX actor, plus compatibility receive APIs.
- A virtual CAN probe found that cancelling the actor under a 50 ms timeout took
  1.004 seconds because final socket destruction blocked in Linux `isotp_release`.
- A 20 ms send with a slow TX hook was accepted after 150 ms and returned success.
  A relative timeout calculated before the hook and an outer `timeout_at` did
  not enforce the original deadline.
- An empty receive buffer now returns terminal configuration failure and leaves
  the payload queued; the previous receive-into implementation consumed it and
  returned `BufferTooSmall`.
- Existing Embassy users and the Unix CAN benchmark use `can-iso-tp`'s mutable
  async demultiplexer. Its application and driver views borrow the same state;
  they are not independently usable read/write halves.

## 1. Canonical transport contracts

The following signatures show the intended public contract. Derives,
documentation, error formatting, and the symmetric `Send` declarations are
omitted where they add no information.

```rust
pub enum Destination {
    Physical(u8),
    Functional(u8),
}

pub struct ReceivedPayload {
    pub meta: RecvMeta,
    pub len: usize,
}

pub enum ReceiveError<E> {
    BufferTooSmall { needed: usize, got: usize },
    Rejected(E),
    Fatal(E),
    Closed,
}

pub enum TransmitError<E> {
    Timeout,
    Rejected(E),
    Fatal(E),
    Closed,
}

pub trait IsoTpAsyncRead {
    type Error;

    async fn recv_into(
        &mut self,
        out: &mut [u8],
    ) -> Result<ReceivedPayload, ReceiveError<Self::Error>>;
}

pub trait IsoTpAsyncWrite {
    type Error;
    type Deadline: Deadline;

    async fn send(
        &mut self,
        destination: Destination,
        payload: &[u8],
        deadline: Self::Deadline,
    ) -> Result<(), TransmitError<Self::Error>>;
}
```

`Destination` replaces two copies of the backend send implementation and the
queue's boolean functional-address flag. Protocol helpers may retain useful
names such as `send_capnp_to` and `send_capnp_functional_to`; both use `send`.

`TransmitError` is the async error type. The existing synchronous `SendError`
and synchronous endpoint signatures remain available, avoiding a simultaneous
redesign of blocking callers. This is one canonical *async* contract, not a
requirement to make blocking APIs imitate futures.

Remove `IsoTpAsyncEndpoint`, `IsoTpAsyncEndpointRecvInto`, their compatibility
alias traits, and the unpublished additive `TransportError` trait from the new
interface release. Keep synchronous receive status/callback types where still
used by synchronous APIs. There is no automatic conversion from an unclassified
legacy `Backend` failure into `Rejected`.

### Receive semantics

1. Idle receive waits for input or a real transport event indefinitely. Protocol
   timers for an active ISO-TP transaction remain legitimate; periodic idle
   receive timeouts disappear.
2. Cancelling a pending receive preserves the next complete payload and active
   reassembly. After consuming a payload, the backend returns its result in the
   same poll, without another suspension point.
3. `BufferTooSmall` consumes one payload, including when `out` is empty. It
   reports the original payload length and the supplied capacity. A zero-sized
   destination buffer is not a broken socket.
4. `Rejected` consumes a failed transaction or malformed payload. A following
   operation may proceed. `Fatal` invalidates the transport session. `Closed`
   reports an already terminated endpoint.
5. Successful `len` never exceeds `out.len()`. The pump and queue still check
   this boundary defensively before slicing.
6. Socket EOF is fatal. The Linux ISO-TP backend does not interpret a zero-length
   datagram as a valid application message.

### Transmit semantics

Success means the complete payload was accepted by the underlying transport
owner. It does not mean the remote device received or acknowledged it. Admission
into the optional application's TX queue is not yet transport success.

Cancellation must leave the transport usable. Unstarted requests whose
cancellation has been observed are discarded. Once acceptance has happened,
the payload may still be delivered. Dropping the caller cannot undo it, and no
adapter automatically replays it.

The current proposed rule that *every* writer must return without another
suspension after acceptance is too strong for a queue wrapper: acceptance happens
in its driver task and its caller learns the result through a completion channel.
The replacement contract explicitly permits this uncertainty window. The native
socket still returns immediately after the accepting syscall. User-space
segmentation must keep accepted work in independently owned driver state.

An expired deadline prevents new transport acceptance when expiry is observed.
Backends check the same deadline immediately before the accepting operation.
An action already accepted can finish transmitting after its deadline. This is
a cooperative async contract, not a hard real-time guarantee across process
descheduling between a clock check and a syscall.

`Rejected` leaves the session usable and preserves its cause; `Fatal` ends it.
Classification is operation-specific. For example, invalid destination/length
on send can be rejected without treating invalid socket configuration at open
as a recoverable request failure.

### Local and threaded futures

Retain `IsoTpAsyncReadSend` and `IsoTpAsyncWriteSend`, declaring explicit
`impl Future + Send` results. Blanket implementations supply the local traits.
Do not alias a `Send` trait to the local trait's name in implementations.

The write `Send` variant requires its deadline value to be `Send`. Generic code
that also waits on the deadline requires `Deadline::Wait: Send`; helpers expose
that bound explicitly. The associated error must be `Send` when it crosses
tasks, and hooks/shared references have their usual `Send`/`Sync` constraints.

This preserves native async methods, static dispatch, and local embedded
implementations. No boxed futures or executor dependency is required by the
default interface feature set.

## 2. One absolute deadline through the complete send path

The interface crate provides a small clock boundary:

```rust
pub trait Deadline: Clone {
    type Wait: core::future::Future<Output = ()>;

    // Saturates at zero. Cloning must preserve the original clock and expiry.
    fn remaining(&self) -> core::time::Duration;

    // Each call creates its own wait future for that same expiry.
    fn wait(&self) -> Self::Wait;
}
```

Optional `tokio` and `embassy` features implement this trait for their native
monotonic instant types. Their wait futures are `Sleep` and `Timer`, respectively.
Default features stay dependency-free and `no_std`; enabling either runtime
adapter is explicit. Custom backends can supply their own deadline type.

The writer chooses an associated deadline, so `Interface` forwards a concrete
value without gaining a runtime or clock type parameter. Tokio queue writers use
the underlying writer's deadline type; an Embassy queue does likewise. Different
clocks are never converted through an untyped integer timestamp.

Provide one small portable `until(deadline, future)` helper, implemented with
`core::future::poll_fn` and pinned futures. Every poll checks expiry before
polling the operation and registers the deadline's wakeup. Its timeout branch
takes priority over an operation that has not yet started. A result already
accepted remains potentially delivered even if its observation races expiry.
Do not perform a post-success clock check that pretends to undo an accepted send.

The same deadline passes through:

```text
command creation
  -> Interface TX lock
  -> encoding into the shared TX buffer
  -> optional queue admission
  -> wait behind earlier requests
  -> backend readiness
  -> transport acceptance
  -> optional protocol response wait
```

Implementation rules:

- Create the deadline once at the command boundary, before lock acquisition.
  Use checked arithmetic when converting a caller's duration into an instant;
  an unrepresentable deadline is an input error, not a wrapped or reset timeout.
- Apply `until` to the complete interface send, including the lock. Check again
  after synchronous encoding and hooks, immediately before backend dispatch.
- Put that absolute deadline in a queue request. Never replace it with
  `now + remaining` at dequeue or inside the backend.
- Waits on socket capacity, queue capacity, and command completion are cancellable
  and bounded by the same value. A zero budget starts no send.
- Linux checks expiry again after readiness and directly before `send`.
- A legacy backend bridge may accept a remaining duration only at its final
  invocation, after checking expiry. Such a bridge must not promise the stronger
  contract until cancellation and acceptance have been audited.
- Higher-level request/reply operations use the same deadline for send and
  response. ISO-TP protocol timers retain their own protocol meanings.

This adds a small clock abstraction because merely deleting timeout parameters
would prevent a queue driver from independently rejecting stale work while its
caller is delayed or not being polled.

## 3. Protocol interface and a stateful receive pump

The interface's type parameter count stays the same. Rename `Node` to `Tx`:

```rust
pub struct Interface<Maplet, RM, Tx, TxBuf, /* existing capacities */> {
    tx_state: Mutex<RM, TxState<Tx, TxBuf>>,
    rx_state: Mutex<RM, Mailboxes>,
    // Existing mailbox notifications, plus a small terminal/closed state.
}

// Constructor: Interface::new(writer, tx_buffer)
// ReceivePump::new(reader, &interface, &mut rx_buffer)
```

`ReceivePump` owns its reader and borrows the interface and scratch buffer.
`BusHandle`, bundle instances, and file-transfer generics only carry `Tx`.
Remove `rx_transport`, its mutex, `new_shared`, and `rx_node_mut`.
The old `node_mut` alias becomes an explicitly named TX accessor; consuming
extraction of the writer/buffer is available after borrowed bus handles end.

There is one pump operation:

```rust
async fn next(
    &mut self,
) -> Result<IngestOutcome, PumpError<Rx::Error>>;
```

The pump stores `pending: Option<ReceivedPayload>` alongside its scratch buffer.
Its stages are:

1. When no payload is pending, receive into the scratch buffer and validate the
   reported length. Store the metadata before awaiting anything else.
2. Ingest that buffered payload into the existing mailbox.
3. Clear `pending` only after ingestion returns, with no intervening suspension.
   Publish the existing mailbox notification as part of the same commit.

Cancelling `next()` during the mailbox lock leaves the pending payload in the
pump. The next call resumes ingestion before another receive. Dropping the pump
discards that state and ends ownership of its reader; whole-session shutdown may
intentionally discard it. No extra payload allocation is needed.

Malformed headers, unknown IDs, body overflow, and a full mailbox retain the
existing consumed-rejection behavior. In particular, `MailboxFull` does not
silently become an infinite backpressure wait. Changing mailbox overflow policy
is a separate decision. The receive scratch buffer remains borrowed exclusively
for the pump lifetime.

On fatal receive or an invalid reported length, mark the pump ended and close
the interface before returning the typed cause. A subsequent call returns
`Closed` without touching the failed backend. A pump owner that exits unexpectedly
must also close the interface; an owning pump guard performs that transition on
drop. Basic `BusHandle::ingest` remains usable for application-managed ingress.

### Closed interfaces and typed failures

Add an idempotent `Interface::close()` transition:

- Block future sends, cancel the currently waiting send, and wake protocol
  mailbox waiters so they return `Closed` instead of waiting indefinitely.
- Check the closed state before returning buffered mailbox data; a session owner
  still controls generation fencing of records already taken by consumers.
- Keep this transition synchronous, using the existing raw-mutex primitives for
  short state updates. Never await a payload-queue slot to close an interface.
- Do not introduce unbounded notification storage. Only one send holds the TX
  lock; its close wakeup can use a single-waiter signal. TX lock waiters wake as
  that lock is released and check closed before proceeding. Existing bounded
  per-message notifications wake protocol receivers.

The default core remains `no_std`. Do not force the writer to implement async
traits at the struct declaration merely to store its associated error.

Keep codec/validation `Error` independent of the transport. Async sends return
`SendMessageError<Tx::Error>`, separating encoding failures from
`TransmitError<Tx::Error>`. Remove the async path through
`map_isotp_send_error`. A terminal TX result closes the interface synchronously
before propagating its original cause. `PumpError<Rx::Error>` similarly preserves
the original receive cause.

An interface's `Closed` notification is a lifecycle signal, not a replacement for
the original cause. The session owner also observes backend/driver termination.
For generic direct backends without a control handle, it must retain and inspect
the scoped send-task results rather than fire-and-forget them.

## 4. Direct Linux transport path

The normal direct composition is conceptually:

```rust
let parts = TokioSocketCanIsoTp::open_peer(
    &closer, iface_name, local_addr, peer_addr, options,
)?.into_split();

let interface = maplet::Interface::new(parts.writer, tx_buffer);
let mut pump = ReceivePump::new(parts.reader, &interface, &mut rx_buffer);
// The session owner retains parts.control and polls pump.next() continuously.
// Application bundles borrow interface as before.
```

This path has no TX/RX application channels and no queue-driver task. The TX
mutex serializes use of the writer and encoding buffer; the pump owns RX.
One payload is held in the scratch buffer until ingested, plus existing bounded
mailboxes and the kernel's own buffering. The maximum application payload still
comes from the supplied TX/RX buffers.

Keep readiness-based nonblocking syscalls and reject `CAN_ISOTP_WAIT_TX_DONE`
at Tokio socket construction. A safe destructor does not make that blocking
send option suitable for an executor thread. Receive waits on readable/error
readiness, and send waits on writable/error readiness, clearing only consumed
or stale readiness as established by the existing socket tests.

### Addressing is part of binding

`open_peer` constructs normal-fixed UDS RX/TX CAN IDs from local and remote
addresses. A lower-level `open_with_ids` requires explicit addressed metadata:
the receive peer and the configured `Destination` for transmission.

- Receive reports the bound peer, including when that legitimate peer is zero.
- Send checks the requested destination against the configured route and rejects
  mismatches without transmitting anything.
- Physical and functional destinations remain distinct. A functional request
  requires a socket configured with the appropriate CAN identifiers/options;
  it is never silently sent through the physical binding.
- Configure flow control and socket options before splitting. Future runtime
  option changes need explicit backend support; the split API does not expose
  unrestricted raw-fd mutation.

This removes the need for the queue actor's `fixed_reply_to`. Multi-peer
user-space demultiplexers continue to report the actual peer. Source filtering,
where an application needs it, happens at its ingress boundary; it is not a
required queue lifecycle setting.

### Terminal state and control handle

The split parts share the socket registration and a latched terminal state.
`SocketControl` provides `failed()` and `closed()` observations and does not keep
the fd alive after both data halves are dropped.

On discovering a fatal error, either data half records the operation and original
cause, invalidates both halves, and wakes pending I/O and `failed()` waiters
before returning. First failure wins. Linux can use `Arc<Error>` to retain the
same non-cloneable OS error for the owner and operation results; this allocation
policy remains in the std backend, not the portable traits.

Expected transaction failures are returned as `Rejected` and do not latch fatal
state. Linux socket errors are shared between directions; observing an error on
TX does not prove it belongs to that command. Remote acknowledgement semantics
remain a protocol concern.

Keep errno classification at the syscall boundary. The tested ISO-TP transaction
errors can be rejected in either direction when surfaced through the shared
socket error. Treat send `EINVAL` as a rejected request, but receive `EINVAL` as
a fatal implementation/configuration failure: it does not establish that a
transaction was consumed. Unknown errors remain fatal until their continued-use
behavior is established. Failure metadata identifies the operation that observed
the error, rather than claiming the error originated in that operation.

The control handle observes failures discovered by I/O; it does not secretly run
another socket-error consumer or promise to discover an unpolled kernel error.
The direct session runs its pump continuously. During queue backpressure, a
failure arising after reads stop still needs independent application interface
monitoring or resumed I/O to be discovered. Terminal errors already discovered
are published independently of payload queues.

### Teardown must not block executor threads

Use a small explicit `SocketCloser` resource owned by the application. It has a
fixed bound on live/closing sockets and dedicated standard worker threads for
kernel close operations. It is specific to the Linux Tokio adapter.

1. Opening a socket acquires a close-capacity permit before creating its fd.
   Exhaustion returns a distinct capacity error without creating another fd.
2. The permit remains attached to that socket until the kernel close completes.
   The service's total outstanding permits bound open fds, queued close jobs,
   and closing fds; channel capacity is reserved by this admission invariant.
3. Dropping the last data half deregisters the `AsyncFd`, extracts the owned
   socket, and transfers it plus its permit to the closer without waiting for
   capacity or for the kernel. Construction failures after fd creation use the
   same cleanup path. The control handle keeps only status, not fd ownership.
4. A worker performs the potentially blocking close and publishes completion.
   Dropping the service or cancelling an async `closed()` wait never joins a
   blocked worker on an executor thread. Workers retain the service state needed
   to complete already accepted jobs.
5. The service maintains at least one receiver while socket permits exist;
   submission cannot fail merely because the application's service handle was
   dropped. Do not implement a fallback that synchronously closes on executor
   drop or silently leaks the fd.

Use a bounded, explicit worker count; one stuck close must not create an
unbounded number of threads. Several workers can keep unrelated cleanup moving.
There is no claim that kernel close itself has a finite worst-case duration.
Repeated flow-control waits or a long transfer can prolong it.

The session owner bounds how long it waits for `closed()`. If that bound expires,
the old socket remains owned by the closer and its permit remains occupied. Do
not reopen the same peer generation until close completion; other peers continue
subject to available capacity. A process may finish without joining blocked
standard workers. Do not rely on Tokio `spawn_blocking` cleanup that runtime
destruction would then wait for indefinitely.

The closer is additional lifecycle machinery required by Linux behavior. It is
not replaced by removing the queue actor or rejecting `WAIT_TX_DONE`.

### Session owner sequencing

The application owns the interface, pump, socket control handle, and scoped
protocol tasks. It concurrently observes pump termination, socket fatal state,
protocol-task termination, and external shutdown/interface availability.

On termination it closes the interface, cancels and joins scoped operations,
ends their borrows, drops the pump and writer, then observes close completion
within its cleanup budget. Retain the original typed failure before interpreting
secondary `Closed` results. A completed one-shot command can intentionally end
its session; a long-lived pump ending unexpectedly fails a watch session.

Use scoped futures or explicitly owned shared state. Do not use `Box::leak` to
manufacture `'static` interfaces. An application supervisor decides retries and
new generations after cleanup. The transport crates never replay commands.

## 5. Optional queue adapter

Expose separate `QueueTx<TxError, D>` and `QueueRx<RxError>` types. Only TX is
cloneable. RX has one owner and does not carry an otherwise unused TX sender.
This removes the shared RX mutex and makes consumer ownership visible.

`QueueTx` implements `IsoTpAsyncWrite` with the backend's deadline type;
`QueueRx` implements `IsoTpAsyncRead`. The driver accepts independently owned
reader and writer capabilities, with independently typed errors:

```text
make_endpoints(capacities) -> (QueueTx, QueueRx, DriverPorts)
run_actor(reader, writer, ports, config, hooks, shutdown)
    -> Result<ActorStop, ActorError<RxError, TxError>>
```

There is one Tokio `run_actor`; delete the timed polling runner. Its config
contains cooperative burst limits. Queue depths and maximum payload are
construction parameters, validated once. Addressing comes from the backend.

TX requests contain destination, owned payload, absolute deadline, and a unique
completion channel. Reserve queue capacity before copying the payload. The
driver checks deadline/cancellation at dequeue and immediately before dispatch,
including after hooks. The active backend wait races caller abandonment,
deadline, and session shutdown. It never retries the command.

Terminal errors return to the owner outside the RX queue. The active caller and
queued callers receive `Closed` if the driver must retain the non-cloneable
original cause. Rejected sends return their cause to that command and continue.
The driver latches closure before reporting termination so receivers can reject
buffered data once closure is observed. The owner remains responsible for events
already removed from a queue and for generation fencing.

RX backpressure pauses reads while TX and shutdown stay live. This explicitly
does not guarantee discovery of new RX failures during that pause. Do not add
an unbounded staging queue or discard accepted application data merely to make
such a guarantee. Applications needing prompt interface-loss detection retain
their independent interface monitor.

Each frontend can finish independently: dropping all TX handles ends command
admission while RX may continue; dropping RX ends forwarding while TX may
continue. RX must race consumer closure even during an idle backend receive.
The driver normally finishes after both frontends close. A fatal result or
explicit shutdown ends both immediately. Pure RX and pure TX use are therefore
possible without retaining a dummy opposite-direction handle.

Hooks stay synchronous and observational. They must not block the executor, but
deadline checks after hooks still protect against delays. Cooperative yielding
counts rejected traffic as well as successful payloads. Application ingress
filters must likewise yield under an always-ready stream of discarded input.

### Embassy queues

Keep fixed storage, explicit capacities, and local futures. Static storage does
not imply an immortal session: an explicit control state marks it closed and
wakes waiters. Embassy shutdown uses that control state, not a fictitious channel
disconnect. A new generation cannot reuse its storage until the old driver and
request leases have ended.

Replace the global TX response queue and RPC mutex with bounded per-request
completion slots. Each slot has a generation-tagged ID, owned payload, deadline,
completion state, and caller/driver leases. Cancellation marks the slot abandoned;
it is reusable only after the driver can no longer touch it. A late completion
must never satisfy a later command. Slot admission itself observes the deadline.

This is required before Embassy queue endpoints claim the same cancellable
write contract. The current strategy of draining a shared response queue before
the next request cannot correlate a completion that arrives afterward.

## 6. User-space ISO-TP dependency work

`can-iso-tp` 0.2.0 cannot implement the new concurrency contract by wrapping its
demultiplexer in a mutex and awaiting indefinitely while holding that mutex.
Its TX needs incoming flow-control frames, and its RX sends flow-control frames.
Protocol progress must be owned independently of application receive/send waits.

A coordinated backend release is required:

- Move per-peer TX progress, timers, and RX reassembly into persistent driver
  state. A cancelled application future does not abandon required protocol work.
- Drive that state from frame readiness, command admission, explicit shutdown,
  and the nearest *active* protocol deadline. With no active transaction and no
  input, await readiness without an idle timer.
- Expose addressed application RX/TX ports implementing the canonical traits.
  Delivery into its bounded driver-owned TX state is its acceptance boundary;
  retain a copy/owned buffer until protocol completion. Report later transaction
  failure through the backend's documented event/control mechanism, without
  pretending it is an acknowledgement for the next command.
- Preserve functional/physical addressing and peer metadata. Verify error
  classification and cancellation against the protocol state machines.

For the current user-space backend, acceptance-based completion is a behavioral
change: its application send currently drives the ISO-TP transaction to
completion. Document this migration explicitly. Applications requiring protocol
completion use a separate backend completion facility or their application
acknowledgement; they do not infer delivery from the canonical send result.
Never reuse an accepted payload's storage until the protocol driver releases it,
even though its application caller has already received success.

The new generic queue actor is optional for an already actor-driven user-space
backend. Avoid stacking a second pair of queues merely to match an endpoint type.
The fixed-storage request machinery described above should be reused by its
Embassy application ports, rather than implementing a second RPC mechanism.

This backend change is a release prerequisite for migrating the existing Embassy
transport and Unix CAN benchmark. Do not silently keep the old polling driver
under the new trait names. Existing downstream firmware can remain pinned to its
old published stack while the coordinated release is prepared.

Remove the unused concrete `can-iso-tp` dependency from the `thincan` core after
feature-matrix verification. Applications/examples that instantiate that backend
depend on it directly. Avoid importing two incompatible interface versions into
the same new transport path and assuming their identically named traits match.

## 7. Migration map and release sequence

| Current surface | Replacement |
| --- | --- |
| Combined async endpoint/callback traits | Canonical `IsoTpAsyncRead` / `IsoTpAsyncWrite` capabilities |
| Timed receive-into and async `TimedOut` statuses | Indefinite receive; explicit boundary deadline waits |
| Relative send duration at each layer | Writer-associated absolute deadline passed unchanged |
| `TransportError::is_terminal` | `Rejected` / `Fatal` in operation results |
| `Interface::new(tx, rx, buffer)` / `new_shared` | `Interface::new(tx, buffer)` plus an owning receive pump |
| Two integrated pump methods | `ReceivePump::next`, preserving pending ingestion across cancellation |
| Generic `Node` in bundles | `Tx`, with a write-only capability bound |
| Async send errors erased into `Other` | `SendMessageError<Tx::Error>` |
| Two Tokio actor implementations/configurations | One returnable independently driven actor |
| Cloneable combined queue endpoint | Cloneable TX and uniquely owned RX |
| Queue-supplied fixed peer metadata | Explicit socket route/binding metadata |
| Final fd close on executor drop | Admitted, bounded off-executor close with observable completion |
| Embassy global TX response queue | Generation-tagged per-request completion slots |

Use breaking minor versions for these pre-1.0 crates: interface 0.2, thincan 0.2,
Linux adapter 0.2, queue 0.3, and file-transfer 0.2. Coordinate the user-space
backend's next breaking release. Do not publish the temporary additive interface
0.1.1 solely to get to this replacement design.

Implementation should proceed as reviewable units:

1. **Interface contracts and conformance harness.** Finalize the deadline,
   cancellation, error, and metadata rules. Compile local and threaded examples;
   build default `no_std` and both optional clock adapters.
2. **Backend primitives.** Implement Linux routing, shared fatal state, and safe
   closing; complete the user-space driver's persistent protocol state and
   fixed-storage application ports. Backend conformance is a prerequisite to
   asserting the new traits, not follow-up documentation.
3. **Protocol core and macros.** Remove RX storage from `Interface`, introduce
   the stateful pump, propagate typed async send errors, implement closure, and
   update all `maplet!` arms, `bundle_instance!`, and `BundleFactory` bounds.
4. **Queue replacement.** Retain one Tokio runner, distinct frontend handles,
   absolute request deadlines, terminal result ownership, and finite storage.
   Complete the Embassy request-slot/lifecycle implementation before its release.
5. **Consumers and examples.** Update file-transfer's transport abstraction and
   error returns, then tests, Unix CAN examples, and benchmark transport setup.
   Add direct Linux and queue-backed examples with the same protocol bundle.
6. **Release preparation.** Publish prerequisite interface/backend releases,
   regenerate registry lockfiles, and build without local patches. Keep each
   consumer publication in dependency order; do not leave a release depending
   on an unpublished registry version.

`thincan-file-transfer` currently exposes a concrete, cause-erasing
`thincan::Error` through its bus abstraction. Add an associated transport error
and propagate a typed transfer error through send/receive helpers. Keep protocol
timeouts and codec errors distinguishable. Whole-transfer duration and per-chunk
budgets remain distinct: for each operation use the earlier of its operation
deadline and any existing overall deadline, rather than accidentally resetting
the overall budget per chunk. Adapt test assertions to these semantics.

Downstream Abi adoption is a separate change: replace leaked static sessions,
choose direct Linux transport, introduce the planned supervisor and interface
monitor, and keep peer-generation/output fencing. Firmware migration updates
static interface/pump construction and the ISO-TP driver together. This design
does not deploy either downstream change.

## 8. Validation and acceptance gates

A standalone prototype was compiled with Tokio 1.49 during design review. It
validated the local/Send blanket-trait combination, associated deadline and wait
future bounds through a shared TX lock, non-Clone error types, and expiry checked
before an operation is polled. The implementation now has regression tests for
these properties; completed validation and its limitations are recorded in the
[validation evidence](async-transport-validation.md).

| Area | Required evidence |
| --- | --- |
| Trait portability | Local `Rc` backend compiles; generic threaded reader/writer helpers compile with explicit `Send` bounds; no-default-feature build has no runtime dependency. |
| Clock portability | Tokio paused-time, Embassy/mock monotonic time, cloned deadlines, and overflow handling; incompatible clocks cannot be mixed accidentally. |
| Complete send budget | A queued TX-lock waiter, slow encoder/hook, full request queue, delayed dequeue, and blocked backend all share one expiry; no dispatch after observed expiry. |
| Cancellation | Cancel before admission, while queued, at dispatch, while waiting for backend capacity, and after acceptance; no automatic replay or state corruption. |
| Pump state | Cancel while awaiting input and after input while the mailbox lock is held; resume ingests the payload once, before reading another. |
| Closure | Fatal RX/TX and explicit close wake sends and protocol waiters; repeated operations return `Closed` without another backend call. |
| Addressing | Direct socket reports the bound peer; wrong physical/functional destination is rejected without transmitting; raw-ID bindings require explicit metadata. |
| RX capacity | Zero, undersized, exact-size, and invalid-reported-length buffers; oversize consumes exactly one payload and the next read succeeds. |
| Readiness | Idle RX does not wake periodically; concurrent multiframe TX/RX; error-only readiness; EINTR/EAGAIN handling and readiness clearing. |
| Transaction recovery | Existing EILSEQ, ETIMEDOUT, and ECOMM vcan cases plus padding failure and flow-control overflow for every recoverable mapping claimed. |
| Linux teardown | Cancel with a PDU waiting for flow control; executor heartbeat/another peer continues; close wait can time out; no same-peer reopen before completion. |
| Closer capacity | Hold a worker in close, exercise repeated shutdown/open attempts, and verify fd/job/thread counts stay within admitted bounds and permits return only on close completion. |
| Queue lifecycle | Either frontend can end independently; idle reader notices consumer drop; both closed ends driver; fatal failure closes both regardless of RX queue fullness. |
| Error ownership | Non-Clone backend error reaches the owner once; operation callers receive the documented original cause or `Closed`; no cause-erasing `Other` conversion. |
| Embassy request slots | Late completion after cancellation cannot satisfy a new request; no slot reuse while driver retains a lease; shutdown wakes every waiter. |
| User-space protocol | RX continues while TX waits for FC, cancellation retains protocol state, active protocol timers fire, and idle driver has no periodic timeout. |
| Protocol compatibility | Macro/bundle tests, no_std smoke, Tokio and Embassy file transfers, and Unix CAN end-to-end examples pass with the new traits. |
| Recovery resources | At least 1000 create/fail/close cycles without retained tasks, fds, completion slots, or unbounded buffers. |

The existing successful tests are retained or migrated; compatibility-only
wrappers and assertions disappear. Prior review probes become regressions for
teardown, stale deadlines, and empty buffers. Hardware-dependent claims require
kernel/vcan or target evidence rather than datagram mocks alone.

## 9. Complexity boundary

The simplification removes duplicate async interfaces, polling compatibility,
the mandatory queue data path on Linux, the RX mutex, and unnecessary RX types
from application bundles. It also makes explicit three responsibilities the
existing code obscures: deadline propagation, terminal session state, and safe
kernel teardown.

Do not expand this work into a generic executor framework, arbitrary transport
registry, automatic reconnect layer, or a new protocol/mailbox implementation.
The clock trait only supplies remaining time and a wait future. The Linux
control/closer handles socket lifetime; the application supervises sessions.
The optional queue supplies ownership and buffering where those are useful.

These are the acceptance boundaries for the refactor. A shorter implementation
that drops one of the required ownership, cancellation, or deadline properties
does not satisfy this design.

## Implementation notes

`can_iso_tp::driver::run_driver` reuses the blocking demux state machines through
bounded internal raw-frame staging, with fixed-storage Embassy application ports.
Accepted request leases survive caller completion until protocol completion.
`DriverHooks::transmitted` receives the owned completion result and can retain
non-cloneable late transaction causes for the session supervisor. Raw link failure
terminates the driver. Frame send futures remain pinned across incoming events;
one raw input is retained while an output is blocked, and N_As bounds output.
Protocol processing resumes after that frame acceptance. Pacing/flow-control TX
timers start at actual frame acceptance; RX timers expire independently of a
full application event queue. There is no idle timer or shared async demux lock.

File transfer uses an overall absolute deadline plus two relative timers:
`SendConfig::ack_timeout` for sender retransmission and
`ReceiverConfig::idle_timeout` for receiver liveness between chunks. Both are
built with `Deadline::after` on the deadline's own clock and capped with
`earlier`, so neither can extend the overall expiry and neither needs a runtime
timer. Waiting for an offer or for Accept is bounded only by the deadline.
Finished transfers stay in a small per-bundle TIME_WAIT table (`set_time_wait`);
an optional `serve` future answers retransmitted final chunks from it, off the
caller's path, so `recv_file` need not wait for the sender's confirmation. The Unix
benchmark uses scoped driver/pump futures and a bounded blocking raw-frame worker
with an independent shutdown handle, without leaking interfaces or retrying a
raw send after an uncertain acknowledgement.
