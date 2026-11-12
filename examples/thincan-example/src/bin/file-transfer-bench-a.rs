#![cfg(feature = "uds")]

#[path = "../file_transfer_bench_common.rs"]
mod common;

use anyhow::{Context, Result, bail};
use clap::Parser;
use rand::{Rng, SeedableRng, rngs::StdRng};
use std::path::PathBuf;
use std::time::{Duration, Instant};

#[derive(Parser, Debug)]
#[command(
    author,
    version,
    about = "Benchmark initiator: send random file to peer and receive it back"
)]
struct Args {
    #[arg(long, default_value = "/tmp/embedded-can-unix-socket.sock")]
    socket: PathBuf,

    #[arg(long, default_value_t = 1024 * 1024)]
    bytes: usize,

    #[arg(long, default_value_t = 3)]
    rounds: u32,

    #[arg(long, default_value_t = 5000)]
    timeout_ms: u64,

    #[arg(long, default_value_t = 30000)]
    wait_for_socket_ms: u64,

    #[arg(long, default_value_t = 500)]
    startup_delay_ms: u64,

    #[arg(long)]
    seed: Option<u64>,

    #[arg(long, default_value_t = false)]
    start_bus_server: bool,
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    run(Args::parse()).await
}

async fn run(args: Args) -> Result<()> {
    let _bus_server = if args.start_bus_server {
        let _ = std::fs::remove_file(&args.socket);
        Some(
            embedded_can_unix_socket::BusServer::start(&args.socket)
                .with_context(|| format!("start bus server at {}", args.socket.display()))?,
        )
    } else {
        None
    };

    let timeout = Duration::from_millis(args.timeout_ms);
    let seed = args.seed.unwrap_or_else(rand::random);

    println!(
        "initiator local=0x{:02X} peer=0x{:02X} socket={} rounds={} bytes={} seed={} chunk={}",
        common::NODE_A_ADDR,
        common::NODE_B_ADDR,
        args.socket.display(),
        args.rounds,
        args.bytes,
        seed,
        common::SEND_CHUNK_SIZE
    );

    if args.startup_delay_ms > 0 {
        tokio::time::sleep(Duration::from_millis(args.startup_delay_ms)).await;
    }

    let options = common::NodeOptions {
        wait_for_socket: Duration::from_millis(args.wait_for_socket_ms),
        serve: true,
    };
    common::run_node(&args.socket, common::NODE_A_ADDR, options, |tx| tx, async |bundles| {
        let mut total_send = Duration::ZERO;
        let mut total_echo = Duration::ZERO;

        for round in 0..args.rounds {
            let mut payload = vec![0u8; args.bytes];
            let mut rng = StdRng::seed_from_u64(seed.wrapping_add(round as u64));
            rng.fill_bytes(&mut payload);

            let send_started = Instant::now();
            let send_out = bundles
                .file_transfer
                .send_file(
                    common::NODE_B_ADDR,
                    &payload,
                    common::deadline(timeout)?,
                    common::default_send_config(),
                )
                .await
                .context("send_file failed")?;
            let send_elapsed = send_started.elapsed();

            let mut store = common::MemoryStore::default();
            let echo_started = Instant::now();
            let recv_out = bundles
                .file_transfer
                .recv_file(
                    common::NODE_B_ADDR,
                    &mut store,
                    common::deadline(timeout)?,
                    common::default_receiver_config(),
                )
                .await
                .map_err(|e| anyhow::anyhow!("recv_file failed: {e:?}"))?;
            let echo_elapsed = echo_started.elapsed();

            if store.bytes != payload {
                bail!(
                    "round {} echo mismatch ({} bytes)",
                    round + 1,
                    payload.len()
                );
            }

            total_send += send_elapsed;
            total_echo += echo_elapsed;

            println!(
                "round {:02} tx_id={} rx_id={} send={:.3}s ({:.2} MiB/s) echo={:.3}s ({:.2} MiB/s)",
                round + 1,
                send_out.transfer_id,
                recv_out.transfer_id,
                send_elapsed.as_secs_f64(),
                common::mib_per_sec(payload.len(), send_elapsed),
                echo_elapsed.as_secs_f64(),
                common::mib_per_sec(payload.len(), echo_elapsed),
            );
        }

        let total_bytes = args.bytes * args.rounds as usize;
        println!(
            "summary send={:.2} MiB/s echo={:.2} MiB/s roundtrip={:.2} MiB/s",
            common::mib_per_sec(total_bytes, total_send),
            common::mib_per_sec(total_bytes, total_echo),
            common::mib_per_sec(total_bytes * 2, total_send + total_echo),
        );

        Ok::<(), anyhow::Error>(())
    })
    .await?
}
