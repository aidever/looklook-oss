#![cfg(unix)]

use std::process::Command;
use std::time::{Duration, Instant};

#[test]
fn unix_scm_rights_handover_survives_process_a_exit() {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("handover.sock");
    let meta = dir.path().join("meta.json");
    let bin = env!("CARGO_BIN_EXE_pty-handover");

    let mut proc_a = Command::new(bin)
        .args(["unix-proc-a", "--sock"])
        .arg(&sock)
        .args(["--meta"])
        .arg(&meta)
        .args(["--shell", "/bin/sh"])
        .spawn()
        .expect("spawn unix-proc-a");

    let deadline = Instant::now() + Duration::from_secs(5);
    while !sock.exists() || !meta.exists() {
        assert!(Instant::now() < deadline, "timeout waiting for proc-a socket & meta");
        std::thread::sleep(Duration::from_millis(20));
    }

    let proc_a_pid = proc_a.id();
    let waiter = std::thread::spawn(move || proc_a.wait());

    let out_b = Command::new(bin)
        .args(["unix-proc-b", "--sock"])
        .arg(&sock)
        .args(["--wait-pid", &proc_a_pid.to_string()])
        .args(["--resize-cols", "128", "--resize-rows", "40"])
        .output()
        .expect("run unix-proc-b");

    let status_a = waiter.join().unwrap().unwrap();
    assert!(status_a.success(), "proc-a failed: {status_a}");

    let stdout_b = String::from_utf8_lossy(&out_b.stdout);
    let stderr_b = String::from_utf8_lossy(&out_b.stderr);
    assert!(out_b.status.success(), "proc-b failed: {}\nstdout:\n{stdout_b}\nstderr:\n{stderr_b}", out_b.status);

    let report: serde_json::Value = serde_json::from_str(&stdout_b).expect("valid json report");
    assert_eq!(report["proc_a_exited"].as_bool(), Some(true));
    assert_eq!(report["shell_alive_after_a_exit"].as_bool(), Some(true));
    assert_eq!(report["echo_ok"].as_bool(), Some(true));
    assert_eq!(report["resized_size"].as_array().map(|a| (a[0].as_u64(), a[1].as_u64())), Some((Some(128), Some(40))));
}
