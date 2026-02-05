#![cfg(feature = "uds")]

use anyhow::Result;
use clap::Parser;
use embedded_can_unix_socket::BusServer;
use std::path::PathBuf;
use std::thread;

#[derive(Parser)]
#[command(
    author,
    version,
    about = "embedded-can-unix-socket server for thincan-example"
)]
struct Args {
    /// Socket path to bind.
    #[arg(long, default_value = "/tmp/embedded-can-unix-socket.sock")]
    socket: PathBuf,
}

fn main() -> Result<()> {
    let args = Args::parse();
    let _server = BusServer::start(&args.socket)?;

    println!(
        "embedded-can-unix-socket server listening on {}",
        args.socket.display()
    );
    println!("press Ctrl+C to stop");
    loop {
        thread::park();
    }

    #[allow(unreachable_code)]
    Ok(())
}
