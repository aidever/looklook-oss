//! Wire protocol definitions, delta encoding/reconstruction, and streaming zstd codec
//! for `looklook-term.v1` (supports both native hosts and `wasm32-unknown-unknown` frontends).

pub mod codec;
pub mod control;
pub mod delta;
pub mod messages;
pub mod predict;
pub mod types;

#[cfg(test)]
mod tests;

#[cfg(not(target_arch = "wasm32"))]
pub use codec::{ZstdStreamDecoder, ZstdStreamEncoder};
pub use codec::{
    CodecError, MAX_FRAME_BYTES, PacketFramer, RuzstdStreamDecoder, ZSTD_COMPRESSION_LEVEL,
    ZSTD_WINDOW_LOG, decode_client_msg, encode_client_msg, encode_raw_packet,
};
pub use control::{
    CaptureMode, ControlRequest, ControlResponse, CreateSessionReq, LegacyDownMsg, LegacyUpMsg,
    PaneInfo, SessionInfo, TERMD_CONTROL_VERSION, TermStreamDown,
};
pub use delta::{ApplyError, ClientScreen, DeltaEncoder};
pub use messages::{
    CAP_TRZSZ, ClientMsg, CellSpan, FEATURE_TRZSZ, LEGACY_SUBPROTOCOL, MouseEventKind, PROTOCOL_VERSION, RowPatch,
    ScreenDelta, ScrollRegionShift, ServerMsg, ViewState,
};
pub use types::{
    CellData, CellSpacer, ColorSpec, CoreEvent, CursorShape, CursorState, HistorySlice,
    HyperlinkData, MouseEncoding, MouseProtocol, NamedColorCode, ProgressState, PromptMarkKind,
    ResolvedCell, RgbColor, ScreenRow, ScreenSnapshot, ScrollbackRow, Style, StyleFlags,
    StyleTable, StyleTableBuilder, TermModes, render_row_ansi,
};
