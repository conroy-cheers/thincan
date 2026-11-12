# thincan

An application-layer comms protocol intended for UDS 29-bit addressed ISO-TP transports.

The portable transport traits are in the dependency-free
[`can-isotp-interface`](https://github.com/conroy-cheers/can-isotp-interface) crate.
They include async receive and transmit capabilities that separate tasks can own,
on local and on threaded executors.

## Workspace crates

- `thincan`
  - A tiny application-layer message protocol and router.
  - Defines a simple wire format (message id + body bytes), and provides macros to declare:
    - a bus message registry (*atlas*es)
    - reusable handler groups (*bundle*s)
    - per-device routers (*maplet*s)

- `thincan-file-transfer`
  - A reusable “file transfer” bundle for `thincan`.
  - Provides encoding helpers, a state machine for decoding, and a storage trait to persist
    the received bytes.

- `can-isotp-queue`
  - Queue adapters for Tokio and Embassy.
  - The Tokio actor is event-driven. It returns a failure that ends the session
    to its owner.

- `linux-socketcan-iso-tp`
  - Linux kernel ISO-TP sockets.
  - The Tokio receive and transmit halves work independently and share one
    registered file descriptor.

- `compat-tests` (not published)
  - End-to-end tests of compatibility with the published 0.1 release. See
    [Compatibility with 0.1 peers](#compatibility-with-01-peers).

## Async transport release

The async transport follows [its design](docs/async-transport-refactor.md):

- Reads wait indefinitely.
- Each send takes an absolute deadline.
- A separate receive pump owns the reader.
- Errors that end a session are explicit.
- On Linux, sockets close outside the executor threads.
- User-space ISO-TP runs its protocol in a persistent driver of its own.

These changes shipped on 2026-11-12 in a coordinated release:
`can-isotp-interface` 0.2, `can-iso-tp` 0.3, `can-isotp-queue` 0.3,
`embedded-can-unix-socket` 0.1.2, and version 0.2 of `thincan`,
`linux-socketcan-iso-tp` and `thincan-file-transfer`.
[CHANGELOG.md](CHANGELOG.md) lists the changes.
[The migration guide](docs/migration-0.2.md) gives the upgrade steps.

The workspace builds against these versions from crates.io. Run
`cargo test --workspace --all-features`.

`can-iso-tp` depends on `can-isotp-queue`, so the root `Cargo.toml` patches
`can-isotp-queue` to the copy in this repository. Without the patch, the
example and the compat tests get two copies of the crate, and their types do
not match.

## Compatibility with 0.1 peers

`compat-tests` runs two complete nodes on a unix-socket CAN bus inside one
process. One node is built from the published 0.1 release (`thincan` 0.1.1,
`thincan-file-transfer` 0.1.1, `can-iso-tp` 0.2.0). The other is built from this
workspace. Every test sends real CAN frames in both directions, through both
ISO-TP implementations, both `thincan` versions and both file-transfer state
machines. The only injected fault is a lost message: a test drops selected
outgoing messages at a node's transport edge.

The tests cover:

- plain and back-to-back transfers
- request/response use
- lost `Complete`, progress and data messages
- repeated offers
- receiver idle aborts
- ISO-TP messages near the 4095-byte maximum

Tests named `pins_...` assert known limitations with 0.1 peers. Run the tests
with `cargo test -p thincan-compat-tests`.
