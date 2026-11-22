//! Canonical terminal state types shared across the emulation core,
//! wire protocol, and daemon (wasm32-compatible, pure Rust).

use std::collections::HashMap;
use std::fmt::Write as _;
use std::hash::Hash;

use bitflags::bitflags;
use serde::{Deserialize, Serialize};

/// An 8-bit-per-channel RGB color.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct RgbColor {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

/// Named terminal color indices matching xterm / alacritty definitions.
#[repr(u16)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum NamedColorCode {
    Black = 0,
    Red = 1,
    Green = 2,
    Yellow = 3,
    Blue = 4,
    Magenta = 5,
    Cyan = 6,
    White = 7,
    BrightBlack = 8,
    BrightRed = 9,
    BrightGreen = 10,
    BrightYellow = 11,
    BrightBlue = 12,
    BrightMagenta = 13,
    BrightCyan = 14,
    BrightWhite = 15,
    Foreground = 256,
    Background = 257,
    Cursor = 258,
    DimBlack = 259,
    DimRed = 260,
    DimGreen = 261,
    DimYellow = 262,
    DimBlue = 263,
    DimMagenta = 264,
    DimCyan = 265,
    DimWhite = 266,
    BrightForeground = 267,
    DimForeground = 268,
}

/// Terminal foreground/background/underline color specification.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ColorSpec {
    Named(NamedColorCode),
    Indexed(u8),
    Rgb(RgbColor),
}

impl Default for ColorSpec {
    fn default() -> Self {
        Self::Named(NamedColorCode::Foreground)
    }
}

bitflags! {
    /// SGR visual attributes for a terminal cell.
    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
    #[serde(transparent)]
    pub struct StyleFlags: u16 {
        const BOLD             = 1 << 0;
        const DIM              = 1 << 1;
        const ITALIC           = 1 << 2;
        const UNDERLINE        = 1 << 3;
        const DOUBLE_UNDERLINE = 1 << 4;
        const UNDERCURL        = 1 << 5;
        const DOTTED_UNDERLINE = 1 << 6;
        const DASHED_UNDERLINE = 1 << 7;
        const INVERSE          = 1 << 8;
        const HIDDEN           = 1 << 9;
        const STRIKEOUT        = 1 << 10;
    }
}

/// Hyperlink target parsed from OSC 8.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct HyperlinkData {
    pub id: String,
    pub uri: String,
}

/// Visual style of a terminal cell.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Style {
    pub fg: ColorSpec,
    pub bg: ColorSpec,
    pub underline_color: Option<ColorSpec>,
    pub flags: StyleFlags,
    pub hyperlink_id: Option<u32>,
}

impl Default for Style {
    fn default() -> Self {
        Self {
            fg: ColorSpec::Named(NamedColorCode::Foreground),
            bg: ColorSpec::Named(NamedColorCode::Background),
            underline_color: None,
            flags: StyleFlags::empty(),
            hyperlink_id: None,
        }
    }
}

/// Deduplicated style and hyperlink palette for a screen frame or snapshot.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StyleTable {
    pub styles: Vec<Style>,
    pub hyperlinks: Vec<HyperlinkData>,
}

impl Default for StyleTable {
    fn default() -> Self {
        Self {
            styles: vec![Style::default()],
            hyperlinks: Vec::new(),
        }
    }
}

/// Helper for interning styles and hyperlinks with `style_id = 0` reserved for default style.
#[derive(Clone, Debug)]
pub struct StyleTableBuilder {
    table: StyleTable,
    style_map: HashMap<Style, u32>,
    link_map: HashMap<HyperlinkData, u32>,
}

impl Default for StyleTableBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl StyleTableBuilder {
    pub fn new() -> Self {
        let mut style_map = HashMap::new();
        style_map.insert(Style::default(), 0);
        Self {
            table: StyleTable::default(),
            style_map,
            link_map: HashMap::new(),
        }
    }

    pub fn from_existing(table: StyleTable) -> Self {
        let mut style_map = HashMap::with_capacity(table.styles.len());
        for (idx, s) in table.styles.iter().enumerate() {
            style_map.insert(s.clone(), idx as u32);
        }
        let mut link_map = HashMap::with_capacity(table.hyperlinks.len());
        for (idx, l) in table.hyperlinks.iter().enumerate() {
            link_map.insert(l.clone(), idx as u32);
        }
        Self {
            table,
            style_map,
            link_map,
        }
    }

    pub fn style_count(&self) -> usize {
        self.table.styles.len()
    }

    pub fn hyperlink_count(&self) -> usize {
        self.table.hyperlinks.len()
    }

    pub fn table(&self) -> &StyleTable {
        &self.table
    }

    pub fn intern_hyperlink(&mut self, link: HyperlinkData) -> u32 {
        if let Some(&id) = self.link_map.get(&link) {
            return id;
        }
        let id = self.table.hyperlinks.len() as u32;
        self.table.hyperlinks.push(link.clone());
        self.link_map.insert(link, id);
        id
    }

    pub fn intern_style(&mut self, style: Style) -> u32 {
        if let Some(&id) = self.style_map.get(&style) {
            return id;
        }
        let id = self.table.styles.len() as u32;
        self.table.styles.push(style.clone());
        self.style_map.insert(style, id);
        id
    }

    pub fn into_table(self) -> StyleTable {
        self.table
    }
}

/// Role of a cell in multi-column wide glyph layout.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum CellSpacer {
    #[default]
    None,
    /// Trailing placeholder occupying the second column of a width-2 glyph.
    WideTrailing,
    /// Leading placeholder inserted at line end when a wide glyph wrapped onto the next row.
    WideLeading,
}

/// Content and style reference of a single cell.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct CellData {
    pub c: char,
    /// Additional zero-width combining characters in this grapheme cluster (empty for single codepoints).
    pub extra: String,
    /// Display column width (0 for spacers, 1 for narrow, 2 for wide).
    pub width: u8,
    pub spacer: CellSpacer,
    pub style_id: u32,
}

impl Default for CellData {
    fn default() -> Self {
        Self {
            c: ' ',
            extra: String::new(),
            width: 1,
            spacer: CellSpacer::None,
            style_id: 0,
        }
    }
}

impl CellData {
    pub fn is_blank_default(&self) -> bool {
        (self.c == ' ' || self.c == '\t')
            && self.extra.is_empty()
            && self.style_id == 0
            && self.spacer == CellSpacer::None
    }
}

/// A visible row in the terminal viewport.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ScreenRow {
    pub cells: Vec<CellData>,
    /// True if the row soft-wrapped onto the following line.
    pub wrapped: bool,
}

/// Terminal cursor visual shape.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum CursorShape {
    #[default]
    Block,
    Underline,
    Beam,
    HollowBlock,
    Hidden,
}

/// Complete cursor state reported to viewers.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct CursorState {
    pub row: u16,
    pub col: u16,
    pub shape: CursorShape,
    pub blinking: bool,
    pub visible: bool,
}

impl Default for CursorState {
    fn default() -> Self {
        Self {
            row: 0,
            col: 0,
            shape: CursorShape::Block,
            blinking: false,
            visible: true,
        }
    }
}

/// Mouse reporting mode requested by the running program.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum MouseProtocol {
    #[default]
    None,
    /// X10 / VT200 normal button click tracking (?1000).
    Normal,
    /// Button-event drag tracking (?1002).
    Drag,
    /// Any-event motion tracking (?1003).
    AnyMotion,
}

/// Mouse report wire encoding requested by the running program.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum MouseEncoding {
    #[default]
    X10,
    Utf8,
    Sgr,
}

/// Relevant terminal modes for frontend input/rendering behavior.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TermModes {
    pub alt_screen: bool,
    pub show_cursor: bool,
    pub app_cursor: bool,
    pub app_keypad: bool,
    pub bracketed_paste: bool,
    pub focus_in_out: bool,
    pub alternate_scroll: bool,
    pub line_wrap: bool,
    pub origin: bool,
    pub insert: bool,
    pub sync_update: bool,
    pub mouse_protocol: MouseProtocol,
    pub mouse_encoding: MouseEncoding,
}

impl Default for TermModes {
    fn default() -> Self {
        Self {
            alt_screen: false,
            show_cursor: true,
            app_cursor: false,
            app_keypad: false,
            bracketed_paste: false,
            focus_in_out: false,
            alternate_scroll: true,
            line_wrap: true,
            origin: false,
            insert: false,
            sync_update: false,
            mouse_protocol: MouseProtocol::None,
            mouse_encoding: MouseEncoding::X10,
        }
    }
}

/// Progress indicator state from OSC 9;4.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProgressState {
    Hidden,
    Normal(u8),
    Error(u8),
    Indeterminate,
    Paused(u8),
}

/// Shell prompt integration marks from OSC 133.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum PromptMarkKind {
    PromptStart,
    CommandStart,
    CommandExecuted,
    CommandFinished(Option<i32>),
}

/// Asynchronous events generated by the terminal state machine while processing bytes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum CoreEvent {
    Title(Option<String>),
    Bell,
    PtyWrite(Vec<u8>),
    ClipboardStore(String),
    CwdChanged(String),
    Progress(ProgressState),
    PromptMark(PromptMarkKind),
    ScrollbackCleared { epoch: u64 },
    AltScreenChanged(bool),
}

/// A self-contained row stored in scrollback history with its own local style palette.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScrollbackRow {
    pub seq: u64,
    pub wrapped: bool,
    pub cells: Vec<CellData>,
    pub styles: Vec<Style>,
    pub hyperlinks: Vec<HyperlinkData>,
}

impl ScrollbackRow {
    pub fn style_of(&self, style_id: u32) -> &Style {
        self.styles
            .get(style_id as usize)
            .unwrap_or_else(|| &self.styles[0])
    }

    pub fn hyperlink_of(&self, style: &Style) -> Option<&HyperlinkData> {
        style
            .hyperlink_id
            .and_then(|id| self.hyperlinks.get(id as usize))
    }

    pub fn plain_text(&self) -> String {
        let mut out = String::new();
        let last_non_empty = self
            .cells
            .iter()
            .rposition(|c| !c.is_blank_default() && c.spacer != CellSpacer::WideLeading);
        let limit = match last_non_empty {
            Some(idx) => idx + 1,
            None => 0,
        };
        for cell in self.cells.iter().take(limit) {
            if cell.spacer != CellSpacer::None {
                continue;
            }
            out.push(cell.c);
            out.push_str(&cell.extra);
        }
        out
    }

    /// Render this scrollback row as SGR ANSI text (`tmux capture-pane -e` style).
    pub fn to_ansi_line(&self) -> String {
        let mut out = String::new();
        render_row_ansi(&self.cells, &self.styles, &mut out);
        out
    }
}

/// Render a row of cells and palette styles with SGR ANSI attributes (`\x1b[2m`, `\x1b[7m`, colors).
pub fn render_row_ansi(cells: &[CellData], styles: &[Style], out: &mut String) {
    let default_style = Style::default();
    let last_non_empty = cells
        .iter()
        .rposition(|c| !c.is_blank_default() && c.spacer != CellSpacer::WideLeading);
    let end = match last_non_empty {
        Some(idx) => idx + 1,
        None => return,
    };

    let mut active_id: u32 = 0;
    for cell in cells.iter().take(end) {
        if cell.spacer != CellSpacer::None {
            continue;
        }
        if cell.style_id != active_id {
            let next_style = styles.get(cell.style_id as usize).unwrap_or(&default_style);
            emit_style_sgr(next_style, out);
            active_id = cell.style_id;
        }
        out.push(cell.c);
        out.push_str(&cell.extra);
    }

    if active_id != 0 {
        out.push_str("\x1b[0m");
    }
}

fn emit_style_sgr(style: &Style, out: &mut String) {
    out.push_str("\x1b[0m");
    if *style == Style::default() {
        return;
    }

    let flags = style.flags;
    if flags.contains(StyleFlags::BOLD) {
        out.push_str("\x1b[1m");
    }
    if flags.contains(StyleFlags::DIM) {
        out.push_str("\x1b[2m");
    }
    if flags.contains(StyleFlags::ITALIC) {
        out.push_str("\x1b[3m");
    }
    if flags.contains(StyleFlags::UNDERLINE) {
        out.push_str("\x1b[4m");
    } else if flags.contains(StyleFlags::DOUBLE_UNDERLINE) {
        out.push_str("\x1b[4:2m");
    } else if flags.contains(StyleFlags::UNDERCURL) {
        out.push_str("\x1b[4:3m");
    } else if flags.contains(StyleFlags::DOTTED_UNDERLINE) {
        out.push_str("\x1b[4:4m");
    } else if flags.contains(StyleFlags::DASHED_UNDERLINE) {
        out.push_str("\x1b[4:5m");
    }
    if flags.contains(StyleFlags::INVERSE) {
        out.push_str("\x1b[7m");
    }
    if flags.contains(StyleFlags::HIDDEN) {
        out.push_str("\x1b[8m");
    }
    if flags.contains(StyleFlags::STRIKEOUT) {
        out.push_str("\x1b[9m");
    }

    emit_color_sgr(style.fg, true, out);
    emit_color_sgr(style.bg, false, out);
}

fn emit_color_sgr(color: ColorSpec, is_fg: bool, out: &mut String) {
    match color {
        ColorSpec::Named(NamedColorCode::Foreground) if is_fg => {}
        ColorSpec::Named(NamedColorCode::Background) if !is_fg => {}
        ColorSpec::Named(code) => {
            let n = code as u16;
            if n < 8 {
                let base = if is_fg { 30 } else { 40 };
                let _ = write!(out, "\x1b[{}m", base + n);
            } else if n < 16 {
                let base = if is_fg { 90 } else { 100 };
                let _ = write!(out, "\x1b[{}m", base + (n - 8));
            }
        }
        ColorSpec::Indexed(idx) => {
            let prefix = if is_fg { 38 } else { 48 };
            let _ = write!(out, "\x1b[{prefix};5;{idx}m");
        }
        ColorSpec::Rgb(rgb) => {
            let prefix = if is_fg { 38 } else { 48 };
            let _ = write!(out, "\x1b[{prefix};2;{};{};{}m", rgb.r, rgb.g, rgb.b);
        }
    }
}

/// Full viewport snapshot plus metadata.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScreenSnapshot {
    pub cols: u16,
    pub rows: u16,
    pub lines: Vec<ScreenRow>,
    pub style_table: StyleTable,
    pub cursor: CursorState,
    pub modes: TermModes,
    pub title: Option<String>,
    pub cwd: Option<String>,
    pub scrollback_epoch: u64,
    pub scrollback_oldest_seq: u64,
    pub scrollback_next_seq: u64,
    pub palette_overrides: Vec<(u16, RgbColor)>,
}

/// Resolved cell representation dereferencing local `style_id` and `hyperlink_id` for
/// canonical equivalence comparison.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedCell {
    pub c: char,
    pub extra: String,
    pub width: u8,
    pub spacer: CellSpacer,
    pub fg: ColorSpec,
    pub bg: ColorSpec,
    pub underline_color: Option<ColorSpec>,
    pub flags: StyleFlags,
    pub hyperlink: Option<HyperlinkData>,
}

impl ScreenSnapshot {
    /// Resolve a cell with its concrete style and hyperlink target for semantic equality checks.
    pub fn resolve_cell(&self, row: usize, col: usize) -> Option<ResolvedCell> {
        let cell = self.lines.get(row)?.cells.get(col)?;
        let default = Style::default();
        let style = self
            .style_table
            .styles
            .get(cell.style_id as usize)
            .unwrap_or(&default);
        let hyperlink = style
            .hyperlink_id
            .and_then(|id| self.style_table.hyperlinks.get(id as usize).cloned());
        Some(ResolvedCell {
            c: cell.c,
            extra: cell.extra.clone(),
            width: cell.width,
            spacer: cell.spacer,
            fg: style.fg,
            bg: style.bg,
            underline_color: style.underline_color,
            flags: style.flags,
            hyperlink,
        })
    }

    /// Compare two snapshots for complete visual and semantic equivalence regardless of style ID
    /// numbering order.
    pub fn semantically_eq(&self, other: &Self) -> bool {
        if self.cols != other.cols
            || self.rows != other.rows
            || self.lines.len() != other.lines.len()
            || self.cursor != other.cursor
            || self.modes != other.modes
            || self.title != other.title
            || self.cwd != other.cwd
            || self.scrollback_epoch != other.scrollback_epoch
            || self.scrollback_oldest_seq != other.scrollback_oldest_seq
            || self.scrollback_next_seq != other.scrollback_next_seq
            || self.palette_overrides != other.palette_overrides
        {
            return false;
        }
        for (r, (row_a, row_b)) in self.lines.iter().zip(other.lines.iter()).enumerate() {
            if row_a.wrapped != row_b.wrapped || row_a.cells.len() != row_b.cells.len() {
                return false;
            }
            for c in 0..row_a.cells.len() {
                if self.resolve_cell(r, c) != other.resolve_cell(r, c) {
                    return false;
                }
            }
        }
        true
    }
}

/// Slice of scrollback history lines in a specific epoch.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistorySlice {
    pub epoch: u64,
    pub start_seq: u64,
    pub oldest_seq: u64,
    pub next_seq: u64,
    pub rows: Vec<ScrollbackRow>,
}
