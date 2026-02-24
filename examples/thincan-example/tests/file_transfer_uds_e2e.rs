#![cfg(all(feature = "uds", unix))]

use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

fn unique_socket_path() -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock before epoch")
        .as_nanos();
    PathBuf::from("/tmp").join(format!(
        "thincan-ft-e2e-{}-{}.sock",
        std::process::id(),
        nanos
    ))
}

fn wait_for_socket(path: &PathBuf, timeout: Duration) {
    let start = std::time::Instant::now();
    while start.elapsed() < timeout {
        if path.exists() {
            return;
        }
        thread::sleep(Duration::from_millis(20));
    }
    panic!("socket did not appear at {}", path.display());
}

fn kill_child(mut child: Child) {
    let _ = child.kill();
    let _ = child.wait();
}

#[test]
fn uds_backend_file_transfer_roundtrip_e2e() {
    let socket = unique_socket_path();
    let socket_str = socket.to_string_lossy().to_string();

    let server_bin = env!("CARGO_BIN_EXE_uds-server");
    let bench_a_bin = env!("CARGO_BIN_EXE_file-transfer-bench-a");
    let bench_b_bin = env!("CARGO_BIN_EXE_file-transfer-bench-b");

    let server = Command::new(server_bin)
        .arg("--socket")
        .arg(&socket_str)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn uds-server");

    wait_for_socket(&socket, Duration::from_secs(5));

    let mut responder = Command::new(bench_b_bin)
        .arg("--socket")
        .arg(&socket_str)
        .arg("--rounds")
        .arg("1")
        .arg("--timeout-ms")
        .arg("5000")
        .arg("--wait-for-socket-ms")
        .arg("5000")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn file-transfer-bench-b");

    let initiator_status = Command::new(bench_a_bin)
        .arg("--socket")
        .arg(&socket_str)
        .arg("--rounds")
        .arg("1")
        .arg("--bytes")
        .arg("4096")
        .arg("--timeout-ms")
        .arg("5000")
        .arg("--wait-for-socket-ms")
        .arg("5000")
        .arg("--startup-delay-ms")
        .arg("100")
        .arg("--offer-retries")
        .arg("4")
        .arg("--offer-retry-delay-ms")
        .arg("100")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("run file-transfer-bench-a");

    let responder_status = responder.wait().expect("wait file-transfer-bench-b");
    kill_child(server);
    let _ = std::fs::remove_file(&socket);

    assert!(
        initiator_status.success(),
        "initiator failed: {initiator_status:?}"
    );
    assert!(
        responder_status.success(),
        "responder failed: {responder_status:?}"
    );
}
