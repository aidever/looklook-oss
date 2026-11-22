//! Wire message definitions for `looklook-term.v1`.

use serde::{Deserialize, Serialize};

use crate::types::{
    CellData, CoreEvent, CursorState, HistorySlice, HyperlinkData, RgbColor, ScreenSnapshot,
    ScrollbackRow, Style, TermModes,
};

/// Protocol version negotiated in WebSocket / local IPC handshakes.
pub const PROTOCOL_VERSION: &str = "looklook-term.v1";

/// `Welcome.protocol` 里协议名后面跟着的能力（空格分隔）。老的 termd 只发协议名，端上据此决定能不能发新消息
/// （老 termd 收到不认识的消息会断开连接）。
pub const CAP_TRZSZ: &str = "trzsz";

/// `ClientMsg::Features` 的位：这个端能处理 trzsz（termd 认出 trzsz 后把原始字节交给它，§5.8）
pub const FEATURE_TRZSZ: u32 = 1;

/// Legacy ttyd WebSocket protocol version compat constant.
pub const LEGACY_SUBPROTOCOL: &str = "tty";

/// Viewer visibility / pacing hint (design §5.3).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum ViewState {
    /// Foreground active viewer: full adaptive 16–100 ms pacing.
    #[default]
    Active,
    /// Background tab or non-focused viewer: throttled 1 fps updates.
    Background,
    /// Minimized / hidden viewer: pause frame transmission until resumed.
    Hidden,
}

/// Structured mouse action kind when clients submit parsed pointer events.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum MouseEventKind {
    Down,
    Up,
    Drag,
    Move,
    ScrollUp,
    ScrollDown,
    ScrollLeft,
    ScrollRight,
}

/// Messages sent from client/frontend to `termd` / gateway.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ClientMsg {
    Hello {
        protocol: String,
        cols: u16,
        rows: u16,
        /// Optional stable client identifier for disconnect resume & resize ownership.
        client_id: Option<String>,
        /// Last frame acknowledged prior to reconnect (`None` for fresh attach).
        last_frame: Option<u64>,
        /// Last input sequence processed prior to reconnect.
        last_input_seq: u64,
    },
    Input {
        seq: u64,
        data: Vec<u8>,
    },
    Resize {
        cols: u16,
        rows: u16,
    },
    Ack {
        frame: u64,
    },
    View {
        state: ViewState,
    },
    Mouse {
        kind: MouseEventKind,
        button: u8,
        col: u16,
        row: u16,
        modifiers: u8,
        /// Optional pre-encoded terminal escape bytes; if empty `termd` encodes per active mode.
        raw: Vec<u8>,
    },
    HistoryReq {
        request_id: u64,
        epoch: u64,
        start_seq: u64,
        count: u32,
    },
    // 以下是后加的：只在 `Welcome.protocol` 带了对应能力时才发（新变体加在最后，老消息的编码不变）
    /// 端支持的功能（`FEATURE_*` 位）
    Features { flags: u32 },
    /// trzsz 原始字节模式（§5.8）：端已经处理了 `consumed` 个原始字节（从这一段开始算）；
    /// `write` 是它要写到屏幕上的字节（trzsz 进度条、不是传输的输出），termd 喂给内核；
    /// `done`：传完了（或者根本不是 trzsz），termd 把没处理的字节喂给内核，回到正常模式。
    Raw { id: u32, consumed: u64, write: Vec<u8>, done: bool },
}

/// Vertical scroll displacement inside `[top..bottom)` (0-based half-open row interval).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScrollRegionShift {
    pub top: u16,
    pub bottom: u16,
    /// Positive moves content upward (scroll up); negative moves content downward (scroll down).
    pub delta: i16,
}

/// Contiguous horizontal cell span replaced in a row.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CellSpan {
    pub start_col: u16,
    pub cells: Vec<CellData>,
}

/// Row-level delta patch applied after scroll shifts.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RowPatch {
    pub row: u16,
    pub wrapped: bool,
    pub spans: Vec<CellSpan>,
}

/// Incremental viewport update relative to `base_frame`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScreenDelta {
    pub frame: u64,
    pub base_frame: u64,
    pub acked_input_seq: u64,
    pub cols: u16,
    pub rows: u16,
    pub scroll_ops: Vec<ScrollRegionShift>,
    pub new_styles: Vec<Style>,
    pub new_hyperlinks: Vec<HyperlinkData>,
    pub changed_rows: Vec<RowPatch>,
    pub cursor: Option<CursorState>,
    pub modes: Option<TermModes>,
    /// Outer `Some` indicates a title change; inner `None` resets title.
    pub title: Option<Option<String>>,
    /// Outer `Some` indicates a cwd change.
    pub cwd: Option<Option<String>>,
    pub scrollback_epoch: u64,
    pub scrollback_oldest_seq: u64,
    pub scrollback_next_seq: u64,
    pub palette_overrides: Option<Vec<(u16, RgbColor)>>,
}

/// Messages sent from `termd` / gateway to clients.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ServerMsg {
    Welcome {
        protocol: String,
        session_id: String,
        client_id: String,
    },
    Snapshot {
        frame: u64,
        acked_input_seq: u64,
        screen: ScreenSnapshot,
    },
    Delta(ScreenDelta),
    Scrolled {
        epoch: u64,
        rows: Vec<ScrollbackRow>,
    },
    Event(CoreEvent),
    HistoryChunk {
        request_id: u64,
        slice: HistorySlice,
        epoch_mismatch: bool,
    },
    Error {
        code: String,
        message: String,
    },
    /// trzsz 原始字节模式里程序的输出（只发给声明了 `FEATURE_TRZSZ` 的那一个端），`id` 区分前后两段
    Raw { id: u32, data: Vec<u8> },
}
