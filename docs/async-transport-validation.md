# Async transport refactor validation

Validated locally on Linux on 2026-11-11, using the sibling
`can-isotp-interface` and `can-iso-tp` checkouts and the Cargo patches documented
in the workspace README. Coordinated registry versions remain unpublished.

## Automated checks

Passed:

- `thincan` workspace tests with all features.
- Workspace Clippy with all features/targets and `-D warnings`.
- Formatting checks for every changed Rust file across all three repositories.
  Unrelated pre-existing formatting differences were left untouched.
- Interface tests, all-feature Clippy, and default/no-default-feature and
  Embassy builds.
- User-space backend tests and Clippy with all features/targets, plus a
  no-default-feature `async-driver` build.
- `thincan` without default features, with and without Cap'n Proto.
- The `no_std_smoke` integration test, including `core::error::Error` support
  and encoding-error source preservation without `std`.
- Embassy queue tests without default features.
- File transfer without default features, separately with `embassy` and `alloc`.
- Building the Unix example/benchmark binaries.

The final feature-matrix pass caught and fixed a file-transfer build regression:
its new typed protocol error needed formatting/error support that was still
gated behind `thincan/std`. Those implementations now use `core` unconditionally.

The fixed-storage Embassy tests include 1,000 abandoned requests with late
completions, followed by fresh requests through the same single completion slot.
They check that a driver-held lease prevents slot reuse and that late results
cannot satisfy the next generation.

## Real Linux ISO-TP checks

All nine ignored `event_vcan` tests passed in an isolated network namespace:

- Concurrent multiframe transfers in both directions.
- Sequence failure (`EILSEQ`) followed by successful receive.
- Incomplete receive (`ETIMEDOUT`) followed by successful receive.
- Invalid padding (`EBADMSG`) followed by successful receive.
- Flow-control overflow (`EMSGSIZE`) followed by successful send/receive.
- Rejected send and flow-control timeout (`ECOMM`) followed by successful send.
- Pending transaction error consumed by a waiting send, followed by recovery.
- Deferred close during flow-control wait, executor heartbeat, capacity
  exhaustion while close is outstanding, and capacity return after close.
- 1,000 open/reject/close cycles, with each close permit returned before reopen.

Run the test binary with `THINCAN_TEST_VCAN_IFACE` set to an isolated interface,
`--ignored`, and `--test-threads=1`. Virtual CAN validates the real kernel socket
path; it does not substitute for physical CAN or embedded target testing.

## Unix end-to-end benchmark

Both processes exited successfully after three 1 MiB send-and-echo rounds,
using seed **20261111**, 1,024-byte application chunks, and 30-second transfer
deadlines. The initiator verified each echoed payload byte for byte.

Initiator aggregate rates:

| Direction | MiB/s |
| --- | ---: |
| Send | 1.13 |
| Echo receive | 1.20 |
| Round trip | 1.16 |

A separate responder idle sample recorded **zero process CPU ticks over three
seconds**, followed by a successful 64 KiB round trip with the same seed.
This is a short, clock-tick-resolution observation, not a throughput comparison
or a guarantee of zero CPU cost.

## Remaining boundaries

- Publish the coordinated dependency versions in dependency order and
  regenerate registry lockfiles; these checks used local patches.
- Embedded checks compiled `no_std` configurations on the host, not firmware
  for a hardware target.
- Abi application supervision, generation fencing, interface monitoring, and
  deployment remain separate downstream work. No robot deployment was changed.
