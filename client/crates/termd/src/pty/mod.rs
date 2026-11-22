//! Cross-platform PTY / ConPTY spawning, resizing, reading, writing, and process tree
//! inspection.

#[cfg(unix)]
mod unix;
#[cfg(windows)]
mod windows;

#[cfg(unix)]
pub use unix::{PlatformPty, is_pid_alive};
#[cfg(windows)]
pub use windows::{PlatformPty, is_pid_alive};

/// Events pushed by the PTY reader thread.
#[derive(Debug)]
pub enum PtyEvent {
    Output(Vec<u8>),
    Closed { exit_code: Option<i32> },
}

/// Process & foreground metadata returned by platform PTY inspection.
#[derive(Clone, Debug, Default)]
pub struct ProcessInspect {
    pub alive: bool,
    pub pids: Vec<u32>,
    pub fg_pid: Option<u32>,
    pub fg_process: Option<String>,
    pub cwd: Option<String>,
}
