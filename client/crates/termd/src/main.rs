//! `looklook-termd` daemon executable entrypoint.

use std::path::PathBuf;

use anyhow::Result;
use termd::{DaemonOptions, default_socket_endpoint, run_daemon};
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .init();

    // 先记下自己文件的哈希（之后磁盘上的文件可能被升级换掉）
    termd::init_build_id();
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "--version" || a == "-V") {
        println!("looklook-termd {}", termd::build_id());
        return Ok(());
    }

    let mut home: Option<PathBuf> = None;
    let mut socket: Option<String> = None;
    let mut parent_pid: Option<u32> = None;
    let mut idle_exit_secs: u64 = 15;
    let mut takeover = false;

    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--home" if i + 1 < args.len() => {
                home = Some(PathBuf::from(&args[i + 1]));
                i += 2;
            }
            "--socket" if i + 1 < args.len() => {
                socket = Some(args[i + 1].clone());
                i += 2;
            }
            "--parent-pid" if i + 1 < args.len() => {
                parent_pid = args[i + 1].parse().ok();
                i += 2;
            }
            "--idle-exit-secs" if i + 1 < args.len() => {
                idle_exit_secs = args[i + 1].parse().unwrap_or(15);
                i += 2;
            }
            "--no-idle-exit" => {
                idle_exit_secs = 0;
                i += 1;
            }
            "--takeover" => {
                takeover = true;
                i += 1;
            }
            other => {
                tracing::warn!("ignoring unknown argument: {other}");
                i += 1;
            }
        }
    }

    let endpoint = socket.unwrap_or_else(|| default_socket_endpoint(home.as_deref()));
    run_daemon(DaemonOptions {
        endpoint,
        parent_pid,
        idle_exit_secs,
        takeover,
    })
    .await
}
