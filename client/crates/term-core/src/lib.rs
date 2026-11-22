//! Terminal emulation core abstraction (`TerminalCore`) and default `alacritty_terminal`
//! implementation (`AlacrittyCore`).

use std::fmt;
use std::time::Duration;

pub mod alacritty;
pub mod ansi;
pub mod osc;
pub mod scrollback;
pub use term_proto::types;

#[cfg(test)]
mod tests;

pub use alacritty::AlacrittyCore;
pub use scrollback::DEFAULT_SCROLLBACK_LIMIT;
pub use types::{
    CellData, CellSpacer, ColorSpec, CoreEvent, CursorShape, CursorState, HistorySlice,
    HyperlinkData, MouseEncoding, MouseProtocol, NamedColorCode, ProgressState, PromptMarkKind,
    RgbColor, ScreenRow, ScreenSnapshot, ScrollbackRow, Style, StyleFlags, StyleTable,
    StyleTableBuilder, TermModes,
};

/// Configuration for initializing a [`TerminalCore`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CoreConfig {
    pub cols: u16,
    pub rows: u16,
    pub scrollback_limit: usize,
}

impl Default for CoreConfig {
    fn default() -> Self {
        Self {
            cols: 80,
            rows: 24,
            scrollback_limit: DEFAULT_SCROLLBACK_LIMIT,
        }
    }
}

/// Errors returned by terminal core operations.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CoreError {
    Compression(String),
    Serialization(String),
}

impl fmt::Display for CoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Compression(msg) => write!(f, "compression error: {msg}"),
            Self::Serialization(msg) => write!(f, "serialization error: {msg}"),
        }
    }
}

impl std::error::Error for CoreError {}

/// Errors returned when querying a historical scrollback slice.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HistoryError {
    EpochMismatch {
        current_epoch: u64,
        oldest_seq: u64,
        next_seq: u64,
    },
    Core(CoreError),
}

impl From<CoreError> for HistoryError {
    fn from(err: CoreError) -> Self {
        Self::Core(err)
    }
}

impl fmt::Display for HistoryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EpochMismatch {
                current_epoch,
                oldest_seq,
                next_seq,
            } => write!(
                f,
                "scrollback epoch mismatch (current={current_epoch}, range={oldest_seq}..{next_seq})"
            ),
            Self::Core(err) => err.fmt(f),
        }
    }
}

impl std::error::Error for HistoryError {}

/// Swappable terminal emulation kernel abstraction (design §4.2).
pub trait TerminalCore: Send {
    fn size(&self) -> (u16, u16);

    fn feed_bytes(&mut self, bytes: &[u8]);

    fn resize(&mut self, cols: u16, rows: u16);

    fn in_sync_update(&self) -> bool;

    fn flush_sync(&mut self);

    /// Flush buffered DEC 2026 synchronized update if it has been pending longer than `max_wait`.
    fn check_sync_timeout(&mut self, max_wait: Duration) -> bool;

    fn snapshot(&self) -> ScreenSnapshot;

    fn cursor(&self) -> CursorState;

    fn modes(&self) -> TermModes;

    fn title(&self) -> Option<&str>;

    fn cwd(&self) -> Option<&str>;

    fn take_events(&mut self) -> Vec<CoreEvent>;

    /// Return scrollback rows newly appended since the previous call, alongside the active epoch.
    fn take_new_scrollback(&mut self) -> (u64, Vec<ScrollbackRow>);

    /// `(epoch, oldest_seq, next_seq, len)`
    fn scrollback_info(&self) -> (u64, u64, u64, usize);

    fn history_slice(
        &mut self,
        epoch: u64,
        start_seq: u64,
        count: usize,
    ) -> Result<HistorySlice, HistoryError>;

    fn tail_history(&mut self, count: usize) -> Result<HistorySlice, CoreError>;

    fn clear_scrollback(&mut self) -> u64;

    fn capture_screen_ansi(&self) -> String;

    fn capture_history_ansi(&mut self, tail_lines: Option<usize>) -> Result<String, CoreError>;

    fn capture_plain_text(&mut self, include_scrollback: bool) -> Result<String, CoreError>;

    fn serialize_state(&self) -> Result<Vec<u8>, CoreError>;
}
