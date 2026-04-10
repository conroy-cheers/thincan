# can-isotp-queue

`can-isotp-queue` provides queue/actor adapters for `can-isotp-interface` async endpoints.

It is intended for designs where a single concrete ISO-TP node/socket must be owned by one task,
while higher layers need split TX and RX handles that implement:

- `IsoTpAsyncEndpoint`
- `IsoTpAsyncEndpointRecvInto`

## Features

- `tokio`: tokio `mpsc`/`oneshot` adapter.
- `embassy`: `embassy-sync` channel-based adapter for static/no-alloc use.

## Tokio Usage

```rust,ignore
use core::time::Duration;
use can_isotp_queue::{ActorConfig, NoopHooks};
use can_isotp_queue::tokio::{make_endpoints, run_actor};

let (tx_node, rx_node, actor_ports) = make_endpoints::<MyErr>(2048, 8, 8);
let cfg = ActorConfig::new(Duration::from_micros(50), 8, None);
tokio::spawn(run_actor(my_socket_node, actor_ports, cfg, NoopHooks));
```

## Embassy Usage

```rust,ignore
use core::time::Duration;
use can_isotp_queue::{ActorConfig, NoopHooks};
use can_isotp_queue::embassy::{QueueResources, run_actor};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;

static RES: QueueResources<CriticalSectionRawMutex, MyErr, 1024, 8, 1, 8> = QueueResources::new();

let (tx_node, rx_node) = RES.split_endpoints();
let cfg = ActorConfig::new(Duration::from_micros(50), 8, None);
// Spawn actor task with `run_actor(node, &RES, cfg, NoopHooks)`.
```

## Reply-to Override

Set `ActorConfig::fixed_reply_to` to force received metadata reply-to when a backend cannot
reliably report source address metadata.

## Source Filtering

Set `ActorConfig::allowed_reply_from` (or use `with_allowed_reply_from`) to drop received frames
whose ISO-TP source does not match the expected peer address.
