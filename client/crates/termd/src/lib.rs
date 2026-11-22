//! `looklook-termd`: persistent per-user terminal multiplexer daemon replacing tmux/ttyd.

pub mod handover;
pub mod keys;
pub mod pty;
pub mod server;
pub mod session;
pub mod transport;

pub use server::{DaemonOptions, run_daemon};

/// 这一份 termd 的版本标识：包版本 + 自己可执行文件内容的哈希（`0.1.0+1a2b…`）。
///
/// 主进程拿“磁盘上的 termd `--version`”和“正在跑的 termd 报的版本”比，不一样才换成新版本。只用包版本时，
/// 包版本一直是 0.1.0，换了新的 termd 也认不出来（Windows 上又没有“文件被替换了”的检查），旧 termd 一直跑着，
/// 1.89.1 修的问题就没生效。启动时（[`init_build_id`]）就读，之后磁盘上的文件被换掉也还是自己的。
pub fn build_id() -> &'static str {
    BUILD_ID.get_or_init(compute_build_id)
}

/// 进程一开始就调用，记下自己文件的哈希
pub fn init_build_id() {
    let _ = build_id();
}

static BUILD_ID: std::sync::OnceLock<String> = std::sync::OnceLock::new();

fn compute_build_id() -> String {
    let ver = env!("CARGO_PKG_VERSION");
    let Some(bytes) = std::env::current_exe().ok().and_then(|p| std::fs::read(p).ok()) else {
        return ver.to_string();
    };
    // FNV-1a 64：只用来区分不同的构建，不需要抗碰撞
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{ver}+{h:016x}")
}
pub use transport::default_socket_endpoint;
