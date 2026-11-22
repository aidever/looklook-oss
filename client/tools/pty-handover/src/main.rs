//! PTY 句柄跨进程交接原型（对应设计稿第 4.4 节 & 第 10 节第 0 步）。
//!
//! - Unix：`proc-a` 打开 PTY 并运行 shell，经 Unix Domain Socket (`SCM_RIGHTS`) 将主端 fd
//!   交给 `proc-b`；`proc-a` 退出后 `proc-b` 继续读写与 `TIOCSWINSZ`，shell 不断。
//! - Windows：`win-proc-a` 调用 `CreatePseudoConsole` 启动子进程，尝试通过 `DuplicateHandle`
//!   将输入/输出管道及 `HPCON` 交给 `win-proc-b`，验证 `HPCON` 能否跨进程、`conhost` 归属与
//!   `ResizePseudoConsole` 行为。

#[cfg(unix)]
mod unix;
#[cfg(windows)]
mod windows;

#[cfg(unix)]
use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "pty-handover", about = "PTY / ConPTY 跨进程句柄交接原型")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Unix：完整演示进程 A → SCM_RIGHTS → 进程 B 接管并验证 shell 存活
    #[cfg(unix)]
    UnixDemo {
        /// 要在 PTY 里运行的 shell
        #[arg(long, default_value = "/bin/sh")]
        shell: String,
    },
    /// Unix 进程 A：开 PTY 跑 shell，监听 socket 交出主端 fd 后退出
    #[cfg(unix)]
    UnixProcA {
        #[arg(long)]
        sock: PathBuf,
        #[arg(long)]
        meta: Option<PathBuf>,
        #[arg(long, default_value = "/bin/sh")]
        shell: String,
        #[arg(long, default_value_t = 80)]
        cols: u16,
        #[arg(long, default_value_t = 24)]
        rows: u16,
    },
    /// Unix 进程 B：接收主端 fd，等待进程 A 退出后继续读写并 resize PTY
    #[cfg(unix)]
    UnixProcB {
        #[arg(long)]
        sock: PathBuf,
        /// 进程 A 的 PID（可选；传入后会等待其彻底退出后再读写）
        #[arg(long)]
        wait_pid: Option<i32>,
        #[arg(long, default_value_t = 132)]
        resize_cols: u16,
        #[arg(long, default_value_t = 42)]
        resize_rows: u16,
    },
    /// Windows：完整演示 ConPTY 交接尝试
    #[cfg(windows)]
    WinDemo {
        #[arg(long, default_value = "cmd.exe")]
        cmd: String,
    },
    /// Windows 进程 A：CreatePseudoConsole 起子进程并尝试 DuplicateHandle 交接
    #[cfg(windows)]
    WinProcA {
        #[arg(long, default_value = r"\\.\pipe\looklook-conpty-handover-proto")]
        pipe: String,
        #[arg(long, default_value = "cmd.exe")]
        cmd: String,
        #[arg(long, default_value_t = 120)]
        cols: i16,
        #[arg(long, default_value_t = 36)]
        rows: i16,
        /// 退出前是否调用 ClosePseudoConsole（默认不调用，模拟直接交接/进程退出）
        #[arg(long)]
        close_hpcon: bool,
    },
    /// Windows 进程 B：接收管道句柄，在进程 A 退出后测试读写与 ResizePseudoConsole
    #[cfg(windows)]
    WinProcB {
        #[arg(long, default_value = r"\\.\pipe\looklook-conpty-handover-proto")]
        pipe: String,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
        #[cfg(unix)]
        Cmd::UnixDemo { shell } => unix::run_demo(&shell),
        #[cfg(unix)]
        Cmd::UnixProcA { sock, meta, shell, cols, rows } => unix::run_proc_a(&sock, meta.as_deref(), &shell, cols, rows),
        #[cfg(unix)]
        Cmd::UnixProcB { sock, wait_pid, resize_cols, resize_rows } => unix::run_proc_b(&sock, wait_pid, resize_cols, resize_rows),
        #[cfg(windows)]
        Cmd::WinDemo { cmd } => windows::run_demo(&cmd),
        #[cfg(windows)]
        Cmd::WinProcA { pipe, cmd, cols, rows, close_hpcon } => windows::run_proc_a(&pipe, &cmd, cols, rows, close_hpcon),
        #[cfg(windows)]
        Cmd::WinProcB { pipe } => windows::run_proc_b(&pipe),
    }
}
