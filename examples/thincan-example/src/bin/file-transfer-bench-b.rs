#![cfg(feature = "uds")]

#[path = "../file_transfer_bench_common.rs"]
mod common;

use anyhow::{Context, Result};
use clap::Parser;
use std::path::PathBuf;
use std::time::{Duration, Instant};

#[derive(Parser, Debug)]
#[command(
    author,
    version,
    about = "Benchmark responder: receive file from peer and send it back"
)]
struct Args {
    #[arg(long, default_value = "/tmp/embedded-can-unix-socket.sock")]
    socket: PathBuf,

    #[arg(long, default_value_t = 3)]
    rounds: u32,

    #[arg(long, default_value_t = 5000)]
    timeout_ms: u64,

    #[arg(long, default_value_t = 30000)]
    wait_for_socket_ms: u64,

    /// Receive budget per round, including waiting for the initiator's offer
    /// (it may wait for the socket, then sleep before its first file). A peer
    /// that stalls mid-transfer is caught sooner by the receiver's idle timeout.
    #[arg(long, default_value_t = 40000)]
    listen_timeout_ms: u64,
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    run(Args::parse()).await
}

async fn run(args: Args) -> Result<()> {
    let timeout = Duration::from_millis(args.timeout_ms);

    println!(
        "responder local=0x{:02X} peer=0x{:02X} socket={} rounds={} chunk={}",
        common::NODE_B_ADDR,
        common::NODE_A_ADDR,
        args.socket.display(),
        args.rounds,
        common::SEND_CHUNK_SIZE
    );

    let options = common::NodeOptions {
        wait_for_socket: Duration::from_millis(args.wait_for_socket_ms),
        serve: true,
    };
    common::run_node(&args.socket, common::NODE_B_ADDR, options, |tx| tx, async |bundles| {
        let mut total_recv = Duration::ZERO;
        let mut total_send = Duration::ZERO;
        let mut total_bytes = 0usize;

        for round in 0..args.rounds {
            let mut store = common::MemoryStore::default();
            let recv_started = Instant::now();
            let recv_out = bundles
                .file_transfer
                .recv_file(
                    common::NODE_A_ADDR,
                    &mut store,
                    common::deadline(Duration::from_millis(args.listen_timeout_ms))?,
                    common::default_receiver_config(),
                )
                .await
                .map_err(|e| anyhow::anyhow!("recv_file failed: {e:?}"))?;
            let recv_elapsed = recv_started.elapsed();

            let payload = store.bytes;
            let send_started = Instant::now();
            let send_out = bundles
                .file_transfer
                .send_file(
                    common::NODE_A_ADDR,
                    &payload,
                    common::deadline(timeout)?,
                    common::default_send_config(),
                )
                .await
                .context("send_file failed")?;
            let send_elapsed = send_started.elapsed();

            total_recv += recv_elapsed;
            total_send += send_elapsed;
            total_bytes += payload.len();

            println!(
                "round {:02} rx_id={} tx_id={} bytes={} recv={:.3}s ({:.2} MiB/s) send={:.3}s ({:.2} MiB/s)",
                round + 1,
                recv_out.transfer_id,
                send_out.transfer_id,
                payload.len(),
                recv_elapsed.as_secs_f64(),
                common::mib_per_sec(payload.len(), recv_elapsed),
                send_elapsed.as_secs_f64(),
                common::mib_per_sec(payload.len(), send_elapsed),
            );
        }

        println!(
            "summary recv={:.2} MiB/s send={:.2} MiB/s total={:.2} MiB/s",
            common::mib_per_sec(total_bytes, total_recv),
            common::mib_per_sec(total_bytes, total_send),
            common::mib_per_sec(total_bytes * 2, total_recv + total_send),
        );

        Ok::<(), anyhow::Error>(())
    })
    .await?
}
