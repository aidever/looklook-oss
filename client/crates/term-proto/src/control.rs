//! Control and stream attachment IPC messages between `looklook-termd` and the host `looklook`
//! process (pure Rust `serde` types shared across `term-proto`, `termd`, and `looklook-client`).

use serde::{Deserialize, Serialize};

use crate::types::HistorySlice;

/// Daemon control IPC protocol version.
pub const TERMD_CONTROL_VERSION: u32 = 1;

/// Build identifier stamped into `looklook-termd` and `looklook` to trigger upgrades cleanly.
pub const fn default_binary_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// Session creation parameters passed from `looklook` to `looklook-termd`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreateSessionReq {
    pub id: String,
    pub cmd: String,
    pub args: Vec<String>,
    pub cwd: String,
    pub env: Vec<(String, String)>,
    #[serde(default)]
    pub env_remove: Vec<String>,
    pub cols: u16,
    pub rows: u16,
}

/// Capture formatting mode for daemon-side inspection queries.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum CaptureMode {
    /// Active viewport with SGR ANSI escapes (matching `tmux capture-pane -e -p`).
    ScreenAnsi,
    /// Trailing `lines` across scrollback + screen with SGR ANSI escapes.
    RecentAnsi { lines: usize },
    /// Entire available scrollback + screen with SGR ANSI escapes.
    FullAnsi,
    /// Plain UTF-8 text joining soft-wrapped rows (`include_scrollback` includes history).
    PlainText { include_scrollback: bool },
}

/// Summary metadata about a session managed by `termd`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionInfo {
    pub id: String,
    pub title: Option<String>,
    pub cwd: Option<String>,
    pub cols: u16,
    pub rows: u16,
    pub alive: bool,
    pub exit_code: Option<i32>,
    pub child_pid: u32,
    pub fg_pid: Option<u32>,
    pub fg_process: Option<String>,
    pub created_at_ms: i64,
    pub last_output_at_ms: i64,
    pub last_input_at_ms: i64,
    pub viewer_count: usize,
}

/// Pane process & status metadata matching `instances::Pane` fields (§4.6).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneInfo {
    pub id: String,
    pub alive: bool,
    pub exit_code: Option<i32>,
    pub pid: u32,
    pub fg_pid: Option<u32>,
    pub fg_process: Option<String>,
    pub cwd: Option<String>,
    pub title: Option<String>,
    pub cols: u16,
    pub rows: u16,
    pub last_activity_at_ms: i64,
    #[serde(default)]
    pub alt_screen: bool,
    #[serde(default)]
    pub mouse_active: bool,
}

/// Requests sent over a local Unix socket / Windows named pipe connection to `termd`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ControlRequest {
    Ping,
    ShutdownIfIdle,
    BeginHandover { takeover_socket: String },
    CreateSession(CreateSessionReq),
    StopSession { id: String },
    ListSessions,
    GetPane { id: String },
    PanePids { id: String },
    SendKeys { id: String, keys: Vec<String> },
    SendLiteral { id: String, data: String },
    Paste { id: String, data: String },
    Resize { id: String, cols: u16, rows: u16 },
    Capture { id: String, mode: CaptureMode },
    History {
        id: String,
        epoch: Option<u64>,
        start_seq: Option<u64>,
        count: Option<u32>,
        tail_lines: Option<u32>,
    },
    OutputActivity,
    ListOrphans { known_ids: Vec<String> },
    SweepOrphans { keep_ids: Vec<String> },
    /// Upgrade this IPC connection to a duplex `looklook-term.v1` stream (`ClientMsg` up / zstd
    /// `ServerMsg` chunks down) for session `id`.
    OpenTermStream { id: String },
    /// Upgrade this IPC connection to a legacy ttyd-compatible raw byte stream (`LegacyUpMsg` up /
    /// `LegacyDownMsg` down) for session `id`.
    OpenLegacyStream {
        id: String,
        cols: u16,
        rows: u16,
        client_id: String,
    },
}

/// Responses returned by `termd` for synchronous control requests and stream upgrades.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ControlResponse {
    Pong {
        control_version: u32,
        protocol_version: String,
        binary_version: String,
        pid: u32,
        session_count: usize,
        handover_supported: bool,
    },
    Ok {
        changed: bool,
    },
    Session(SessionInfo),
    Sessions(Vec<SessionInfo>),
    Pane(Option<PaneInfo>),
    Pids(Vec<u32>),
    Text(Option<String>),
    History {
        slice: Option<HistorySlice>,
        epoch_mismatch: bool,
    },
    Activity(Vec<(String, i64)>),
    Ids(Vec<String>),
    StreamReady,
    Error {
        code: String,
        message: String,
    },
}

/// Framing packets exchanged after `OpenTermStream` succeeds (`StreamReady`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum TermStreamDown {
    /// Pre-compressed zstd chunk encoding one or more `ServerMsg`s on this viewer's stream.
    ZstdChunk(Vec<u8>),
    /// Uncompressed fallback or control notice if needed.
    Closed { reason: String },
}

/// Messages sent from `looklook` gateway to `termd` on an `OpenLegacyStream` connection.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum LegacyUpMsg {
    Input(Vec<u8>),
    Resize { cols: u16, rows: u16 },
    NoteActivity,
}

/// Messages sent from `termd` to `looklook` gateway on an `OpenLegacyStream` connection.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum LegacyDownMsg {
    Init {
        backlog: Vec<u8>,
        title: Option<String>,
    },
    Output(Vec<u8>),
    Title(String),
    Exited {
        exit_code: Option<i32>,
    },
}
