//! A node built from this workspace: the current benchmark's node (`run_node`:
//! the `can-iso-tp` 0.3 driver behind an Embassy queue, `ReceivePump`, and
//! optionally `serve`), with a transmit handle that can drop selected messages.

#[path = "../../examples/thincan-example/src/file_transfer_bench_common.rs"]
#[allow(dead_code)]
mod bench;

use crate::Faults;
use can_isotp_interface::{Destination, IsoTpAsyncWrite, TransmitError};
use std::path::Path;
use std::time::Duration;

pub use thincan_file_transfer::{ReceiverConfig, SendConfig};

/// Sender settings like the benchmark's, with the library's offer retry.
pub fn send_config() -> SendConfig {
    SendConfig {
        sender_max_chunk_size: crate::CHUNK_SIZE,
        window_chunks: 8,
        max_retries: 5,
        ack_timeout: Duration::from_secs(1),
        ..Default::default()
    }
}

pub fn receiver_config() -> ReceiverConfig {
    ReceiverConfig {
        max_chunk_size: crate::CHUNK_SIZE as u32,
        ..Default::default()
    }
}

/// An absolute deadline `duration` from now.
pub fn deadline(duration: Duration) -> tokio::time::Instant {
    tokio::time::Instant::now() + duration
}

/// The queue's transmit handle, dropping outgoing messages that `faults` selects.
pub struct Endpoint {
    tx: bench::BenchNode<'static>,
    faults: Faults,
}

impl IsoTpAsyncWrite for Endpoint {
    type Error = <bench::BenchNode<'static> as IsoTpAsyncWrite>::Error;
    type Deadline = tokio::time::Instant;
    async fn send(
        &mut self,
        destination: Destination,
        payload: &[u8],
        deadline: Self::Deadline,
    ) -> Result<(), TransmitError<Self::Error>> {
        if self.faults.drops(payload) {
            return Ok(());
        }
        self.tx.send(destination, payload, deadline).await
    }
}

pub type Bundles<'a> = bench::Bundles<'a, Endpoint>;

/// How to run a current node.
#[derive(Clone)]
pub struct Options {
    pub faults: Faults,
    /// Run the file-transfer `serve` future next to the scenario.
    pub serve: bool,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            faults: Faults::none(),
            serve: true,
        }
    }
}

/// Run `scenario` on a current node at `local`, panicking if the node fails.
pub async fn run<T>(
    bus: &Path,
    local: u8,
    options: Options,
    scenario: impl AsyncFnOnce(&Bundles<'_>) -> T,
) -> T {
    let node = bench::NodeOptions {
        wait_for_socket: Duration::from_secs(1),
        serve: options.serve,
    };
    let faults = options.faults;
    // Boxed: the node's future holds its interface and buffers.
    Box::pin(bench::run_node(
        bus,
        local,
        node,
        |tx| Endpoint { tx, faults },
        scenario,
    ))
    .await
    .unwrap_or_else(|error| panic!("current node failed: {error:#}"))
}
