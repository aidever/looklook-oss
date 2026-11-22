//! 浏览器端的 `looklook-term.v1`（设计稿 §5、§6）：编成 wasm32 给手机 DOM 渲染（src-term/dom/）和以后的电脑 wgpu 渲染共用。
//!
//! - 解压在 JS 里做（fzstd，和 `tty-zstd` 同一个库：每条消息 flush 后马上能解出来；
//!   纯 Rust 的 ruzstd 会扣着窗口里的字节不吐，见设计稿 §5）。这里收的是解压后的字节流；
//! - 拆包、bincode 解码、按 Snapshot / Delta 还原屏幕都用 `term-proto` 现成的代码（和 termd 同一份，往返测试覆盖）；
//! - 交给 JS 的是“哪些行变了”加每行按样式分好的段（JSON 字符串，JS 端 `JSON.parse`）。颜色只给编号，
//!   配色（主题）在 JS 端套，换主题不用重拉。
//!
//! 一段（run）的格式：`{"t":"文字"}`，样式不是默认时带 `f`（前景）`b`（背景）`a`（属性位，同 `StyleFlags`）
//! `l`（OSC 8 链接）；非 ASCII 的字每个格子单独一个定宽盒子（`"w":1` 或 `"w":2`，这时文字是数组 `"c":[…]`，
//! 一个元素一个格子），保证不管字体怎么回退，后面的列都对得上（设计稿 §6.2）。
//! 颜色：`-1` 默认，`0..=255` 调色板，`>= 0x1000000` 是 `0x1000000 + 0xRRGGBB`。

use std::collections::{BTreeSet, VecDeque};
use std::fmt::Write as _;

use term_proto::{
    CellData, CellSpacer, ClientMsg, ClientScreen, ColorSpec, CoreEvent, CursorShape, HistorySlice, HyperlinkData,
    MouseEventKind, MouseProtocol, NamedColorCode, PacketFramer, ProgressState, ScreenSnapshot, ScrollbackRow,
    ServerMsg, Style, StyleFlags, ViewState, PROTOCOL_VERSION,
};
use wasm_bindgen::prelude::*;

pub mod history;
pub use term_proto::predict;
pub use history::{CellRef, History, LineRef, PackedRow};
pub use predict::{Prediction, Predictor};

const DEFAULT_COLOR: i64 = -1;
const RGB_BASE: i64 = 0x100_0000;
/// 还没被 termd 确认的输入最多留这么多条（断线续上后重发；再多说明 termd 根本没在处理，丢掉旧的）
const MAX_PENDING_INPUTS: usize = 512;

// ---------- JSON（手写：只有几种固定形状，不必为此带上 serde_json） ----------

fn push_json_str(out: &mut String, s: &str) {
    out.push('"');
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 || c == '\u{2028}' || c == '\u{2029}' => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

fn push_opt_str(out: &mut String, s: Option<&str>) {
    match s {
        Some(s) => push_json_str(out, s),
        None => out.push_str("null"),
    }
}

// ---------- 颜色与样式 ----------

/// 颜色编号（见模块说明）。暗色的具名颜色（Dim*）返回对应的普通色，由调用方加上 DIM 属性。
fn color_code(c: ColorSpec) -> (i64, bool) {
    match c {
        ColorSpec::Indexed(i) => (i as i64, false),
        ColorSpec::Rgb(rgb) => (RGB_BASE + ((rgb.r as i64) << 16 | (rgb.g as i64) << 8 | rgb.b as i64), false),
        ColorSpec::Named(n) => {
            let v = n as u16;
            match n {
                NamedColorCode::Foreground
                | NamedColorCode::Background
                | NamedColorCode::Cursor
                | NamedColorCode::BrightForeground => (DEFAULT_COLOR, false),
                NamedColorCode::DimForeground => (DEFAULT_COLOR, true),
                _ if v < 16 => (v as i64, false),
                // DimBlack..DimWhite = 259..266
                _ if (259..=266).contains(&v) => ((v - 259) as i64, true),
                _ => (DEFAULT_COLOR, false),
            }
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct RunStyle {
    fg: i64,
    bg: i64,
    flags: u16,
    link: Option<u32>,
}

const PLAIN: RunStyle = RunStyle { fg: DEFAULT_COLOR, bg: DEFAULT_COLOR, flags: 0, link: None };

fn run_style(style: &Style) -> RunStyle {
    let (fg, dim_fg) = color_code(style.fg);
    let (bg, _) = color_code(style.bg);
    let mut flags = style.flags.bits();
    if dim_fg {
        flags |= StyleFlags::DIM.bits();
    }
    RunStyle { fg, bg, flags, link: style.hyperlink_id }
}

// ---------- 一行 → 段 ----------

/// 格子在段里怎么摆：0 = ASCII，和相邻的连成一串文字；1 / 2 = 单独一个定宽盒子（1 格 / 2 格宽）
fn cell_box(cell: &CellData) -> u8 {
    if cell.width >= 2 {
        2
    } else if cell.c.is_ascii() && cell.extra.is_empty() {
        0
    } else {
        1
    }
}

/// 一行格子按样式和摆法分段，写成 JSON 数组。行尾默认样式的空格不输出。
fn push_row_runs(out: &mut String, cells: &[CellData], styles: &[Style], links: &[HyperlinkData]) {
    let end = cells
        .iter()
        .rposition(|c| !c.is_blank_default() && c.spacer != CellSpacer::WideLeading)
        .map_or(0, |i| i + 1);
    out.push('[');
    let mut first = true;
    // 当前这一段：样式、摆法、文字（ASCII 串）或格子列表（盒子）
    let mut cur: Option<(RunStyle, u8)> = None;
    let mut text = String::new();
    let mut boxes: Vec<String> = Vec::new();

    let flush = |out: &mut String, first: &mut bool, cur: &Option<(RunStyle, u8)>, text: &mut String, boxes: &mut Vec<String>| {
        let Some((st, w)) = cur else { return };
        if !*first {
            out.push(',');
        }
        *first = false;
        out.push('{');
        if *w == 0 {
            out.push_str("\"t\":");
            push_json_str(out, text);
        } else {
            let _ = write!(out, "\"w\":{w},\"c\":[");
            for (i, b) in boxes.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                push_json_str(out, b);
            }
            out.push(']');
        }
        if st.fg != DEFAULT_COLOR {
            let _ = write!(out, ",\"f\":{}", st.fg);
        }
        if st.bg != DEFAULT_COLOR {
            let _ = write!(out, ",\"b\":{}", st.bg);
        }
        if st.flags != 0 {
            let _ = write!(out, ",\"a\":{}", st.flags);
        }
        if let Some(uri) = st.link.and_then(|id| links.get(id as usize)).map(|l| l.uri.as_str()) {
            out.push_str(",\"l\":");
            push_json_str(out, uri);
        }
        out.push('}');
        text.clear();
        boxes.clear();
    };

    for cell in cells.iter().take(end) {
        if cell.spacer == CellSpacer::WideTrailing {
            continue;
        }
        let st = styles.get(cell.style_id as usize).map_or(PLAIN, run_style);
        let (w, glyph) = if cell.spacer == CellSpacer::WideLeading {
            (0, String::from(" "))
        } else {
            let mut g = String::with_capacity(4);
            g.push(if cell.c == '\0' || cell.c == '\t' { ' ' } else { cell.c });
            g.push_str(&cell.extra);
            (cell_box(cell), g)
        };
        // 盒子里每个格子单独一个元素；ASCII 连成一串。摆法或样式变了就换一段
        if cur != Some((st, w)) {
            flush(out, &mut first, &cur, &mut text, &mut boxes);
            cur = Some((st, w));
        }
        if w == 0 {
            text.push_str(&glyph);
        } else {
            boxes.push(glyph);
        }
    }
    flush(out, &mut first, &cur, &mut text, &mut boxes);
    out.push(']');
}

fn row_plain_text(cells: &[CellData]) -> String {
    let end = cells
        .iter()
        .rposition(|c| !c.is_blank_default() && c.spacer != CellSpacer::WideLeading)
        .map_or(0, |i| i + 1);
    let mut s = String::new();
    for cell in cells.iter().take(end) {
        match cell.spacer {
            CellSpacer::WideTrailing => {}
            CellSpacer::WideLeading => s.push(' '),
            CellSpacer::None => {
                s.push(if cell.c == '\0' || cell.c == '\t' { ' ' } else { cell.c });
                s.push_str(&cell.extra);
            }
        }
    }
    s
}

fn push_scrollback_row(out: &mut String, row: &ScrollbackRow) {
    let _ = write!(out, "{{\"seq\":{},\"w\":{},\"r\":", row.seq, row.wrapped);
    push_row_runs(out, &row.cells, &row.styles, &row.hyperlinks);
    out.push('}');
}

fn push_history(out: &mut String, request_id: u64, slice: &HistorySlice, mismatch: bool) {
    let _ = write!(
        out,
        "{{\"t\":\"history\",\"id\":{request_id},\"mismatch\":{mismatch},\"epoch\":{},\"start\":{},\"oldest\":{},\"next\":{},\"rows\":[",
        slice.epoch, slice.start_seq, slice.oldest_seq, slice.next_seq
    );
    for (i, row) in slice.rows.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        push_scrollback_row(out, row);
    }
    out.push_str("]}");
}

fn shape_name(s: CursorShape) -> &'static str {
    match s {
        CursorShape::Block => "block",
        CursorShape::Underline => "underline",
        CursorShape::Beam => "bar",
        CursorShape::HollowBlock => "hollow",
        CursorShape::Hidden => "hidden",
    }
}

fn mouse_name(p: MouseProtocol) -> &'static str {
    // 和 xterm.js 的 modes.mouseTrackingMode 同名（term-inject.js 用它判断开没开鼠标）
    match p {
        MouseProtocol::None => "none",
        MouseProtocol::Normal => "vt200",
        MouseProtocol::Drag => "drag",
        MouseProtocol::AnyMotion => "any",
    }
}

fn push_event(out: &mut String, ev: &CoreEvent) -> bool {
    match ev {
        CoreEvent::Title(t) => {
            out.push_str("{\"t\":\"title\",\"v\":");
            push_opt_str(out, t.as_deref());
            out.push('}');
        }
        CoreEvent::Bell => out.push_str("{\"t\":\"bell\"}"),
        CoreEvent::ClipboardStore(s) => {
            out.push_str("{\"t\":\"clipboard\",\"v\":");
            push_json_str(out, s);
            out.push('}');
        }
        CoreEvent::CwdChanged(s) => {
            out.push_str("{\"t\":\"cwd\",\"v\":");
            push_json_str(out, s);
            out.push('}');
        }
        CoreEvent::Progress(p) => {
            let (state, v) = match p {
                ProgressState::Hidden => ("hidden", 0),
                ProgressState::Normal(v) => ("normal", *v),
                ProgressState::Error(v) => ("error", *v),
                ProgressState::Indeterminate => ("indeterminate", 0),
                ProgressState::Paused(v) => ("paused", *v),
            };
            let _ = write!(out, "{{\"t\":\"progress\",\"s\":\"{state}\",\"v\":{v}}}");
        }
        CoreEvent::ScrollbackCleared { epoch } => {
            let _ = write!(out, "{{\"t\":\"cleared\",\"epoch\":{epoch}}}");
        }
        CoreEvent::AltScreenChanged(v) => {
            let _ = write!(out, "{{\"t\":\"alt\",\"v\":{v}}}");
        }
        // PtyWrite 是内核要回给程序的应答（termd 自己写回 PTY），PromptMark 暂时用不上
        CoreEvent::PtyWrite(_) | CoreEvent::PromptMark(_) => return false,
    }
    true
}

// ---------- 客户端 ----------

/// 一条终端连接在浏览器里的状态：屏幕、还没被确认的输入。
#[wasm_bindgen]
pub struct TermClient {
    framer: PacketFramer,
    history_framer: PacketFramer,
    screen: ClientScreen,
    next_input_seq: u64,
    pending: VecDeque<(u64, Vec<u8>)>,
    /// 电脑 wgpu 渲染：回滚行留在这里（`keep_history`），JSON 里只给行数
    history: Option<History>,
    predict: Predictor,
    /// trzsz 原始字节（`ServerMsg::Raw`），JSON 里每条 `{"t":"raw"}` 对应一段，按顺序 `take_raw`
    raw: VecDeque<Vec<u8>>,
}

impl Default for TermClient {
    fn default() -> Self {
        Self::new()
    }
}

fn encode(msg: &ClientMsg) -> Vec<u8> {
    term_proto::encode_client_msg(msg).unwrap_or_default()
}

#[wasm_bindgen]
impl TermClient {
    #[wasm_bindgen(constructor)]
    pub fn new() -> TermClient {
        TermClient {
            framer: PacketFramer::new(),
            history_framer: PacketFramer::new(),
            screen: ClientScreen::new(),
            next_input_seq: 1,
            pending: VecDeque::new(),
            history: None,
            predict: Predictor::default(),
            raw: VecDeque::new(),
        }
    }

    /// 新连接开始（解压上下文已在 JS 端重建）。`keep_screen`：续接时保留屏幕，Hello 带上帧号，termd 能接着发差量。
    pub fn reset_stream(&mut self, keep_screen: bool) {
        self.framer = PacketFramer::new();
        self.history_framer = PacketFramer::new();
        if !keep_screen {
            self.screen = ClientScreen::new();
        }
        self.predict.reset(self.next_input_seq - 1);
        self.raw.clear();
    }

    /// 回滚行留在 WASM 里（电脑 wgpu 渲染）。之后 `scrolled` / `history` 更新里不再带行内容，只有 `n`（行数）
    pub fn keep_history(&mut self) {
        if self.history.is_none() {
            self.history = Some(History::default());
        }
    }

    /// [from, to) 里第一段本地没有、termd 还留着的回滚行：`[epoch, 起点, 行数]`，都有了返回空数组
    pub fn history_missing(&self, from: f64, to: f64) -> Vec<f64> {
        let Some(h) = &self.history else { return Vec::new() };
        match h.missing(from.max(0.0) as u64, to.max(0.0) as u64) {
            Some((s, n)) => vec![h.epoch as f64, s as f64, n as f64],
            None => Vec::new(),
        }
    }

    /// 本地最早的一行、termd 还留着的最早一行、屏幕第 0 行的行号：`[本地最早, termd 最早, 屏幕起点]`
    pub fn history_bounds(&self) -> Vec<f64> {
        match &self.history {
            Some(h) => vec![h.first_local() as f64, h.oldest as f64, h.next as f64],
            None => Vec::new(),
        }
    }

    /// 回滚通道重新连上（它有自己的解压上下文和拆包状态）
    pub fn reset_history(&mut self) {
        self.history_framer = PacketFramer::new();
    }

    /// 连接上的第一条消息。
    pub fn hello(&self, cols: u16, rows: u16, client_id: Option<String>, resume: bool) -> Vec<u8> {
        let last_frame = (resume && self.screen.snapshot().is_some()).then(|| self.screen.frame());
        encode(&ClientMsg::Hello {
            protocol: PROTOCOL_VERSION.to_string(),
            cols,
            rows,
            client_id,
            last_frame,
            last_input_seq: self.screen.acked_input_seq(),
        })
    }

    /// 按键 / 粘贴：分配输入序号，留一份直到 termd 确认处理过（断线续上后重发，termd 按序号去重）。
    pub fn input(&mut self, data: &[u8]) -> Vec<u8> {
        self.send_input(data, false)
    }

    /// 键盘打的字（可以本地预测回显，§5.7）；粘贴、编好的转义走 `input`
    pub fn input_typed(&mut self, data: &[u8]) -> Vec<u8> {
        self.send_input(data, true)
    }

    /// 按顺序取出 `{"t":"raw"}` 更新对应的字节
    pub fn take_raw(&mut self) -> Vec<u8> {
        self.raw.pop_front().unwrap_or_default()
    }

    /// 告诉 termd 这个端支持哪些功能（位见 `term_proto::FEATURE_*`；1 = trzsz）。只在 welcome 的 `caps` 里有对应能力时发
    pub fn features(&self, flags: u32) -> Vec<u8> {
        encode(&ClientMsg::Features { flags })
    }

    /// trzsz 原始字节模式的进度（见 `ClientMsg::Raw`）
    pub fn raw_progress(&self, id: u32, consumed: f64, write: &[u8], done: bool) -> Vec<u8> {
        encode(&ClientMsg::Raw { id, consumed: consumed as u64, write: write.to_vec(), done })
    }

    /// 往返够慢时打开本地预测（JS 量往返时间）
    pub fn set_predict(&mut self, on: bool) {
        self.predict.set_enabled(on, self.next_input_seq - 1);
    }

    pub fn predicting(&self) -> bool {
        self.predict.enabled()
    }

    /// 预测变了就加一（重画用）
    pub fn predict_version(&self) -> u32 {
        self.predict.version()
    }

    /// 还没确认的预测：每个 `[行, 列, 字符码]` 摊平
    pub fn predictions(&self) -> Vec<u32> {
        self.predict
            .predictions()
            .iter()
            .flat_map(|p| [p.row as u32, p.col as u32, p.c as u32])
            .collect()
    }

    /// 有预测时光标该在的位置 `[行, 列]`，没有预测为空
    pub fn predict_cursor(&self) -> Vec<u16> {
        self.predict.cursor().map_or_else(Vec::new, |(r, c)| vec![r, c])
    }

    /// 已发出的最后一个输入序号、termd 确认到的序号（JS 量往返时间）
    pub fn last_input_seq(&self) -> f64 {
        (self.next_input_seq - 1) as f64
    }

    pub fn acked_input_seq(&self) -> f64 {
        self.screen.acked_input_seq() as f64
    }
}

impl TermClient {
    fn send_input(&mut self, data: &[u8], typed: bool) -> Vec<u8> {
        let seq = self.next_input_seq;
        self.predict.on_input(seq, data, typed, self.screen.snapshot(), self.screen.acked_input_seq());
        self.next_input_seq += 1;
        self.pending.push_back((seq, data.to_vec()));
        while self.pending.len() > MAX_PENDING_INPUTS {
            self.pending.pop_front();
        }
        encode(&ClientMsg::Input { seq, data: data.to_vec() })
    }
}

#[wasm_bindgen]
impl TermClient {

    /// 还没被确认的输入条数（续接后逐条 `pending_input(i)` 重发）
    pub fn pending_inputs(&self) -> usize {
        self.pending.len()
    }

    pub fn pending_input(&self, i: usize) -> Vec<u8> {
        self.pending
            .get(i)
            .map(|(seq, data)| encode(&ClientMsg::Input { seq: *seq, data: data.clone() }))
            .unwrap_or_default()
    }

    pub fn resize(&self, cols: u16, rows: u16) -> Vec<u8> {
        encode(&ClientMsg::Resize { cols, rows })
    }

    /// 当前帧画完了
    pub fn ack(&self) -> Vec<u8> {
        encode(&ClientMsg::Ack { frame: self.screen.frame() })
    }

    /// 0 = 前台，1 = 后台（每秒 1 帧），2 = 看不见（停发）
    pub fn view(&self, state: u8) -> Vec<u8> {
        let state = match state {
            0 => ViewState::Active,
            1 => ViewState::Background,
            _ => ViewState::Hidden,
        };
        encode(&ClientMsg::View { state })
    }

    /// 鼠标：`kind` 0 按下 1 抬起 2 拖动 3 移动 4 上滚 5 下滚；行列从 0 起。termd 按程序当前的鼠标模式编码。
    pub fn mouse(&self, kind: u8, button: u8, col: u16, row: u16, modifiers: u8) -> Vec<u8> {
        let kind = match kind {
            0 => MouseEventKind::Down,
            1 => MouseEventKind::Up,
            2 => MouseEventKind::Drag,
            3 => MouseEventKind::Move,
            4 => MouseEventKind::ScrollUp,
            _ => MouseEventKind::ScrollDown,
        };
        encode(&ClientMsg::Mouse { kind, button, col, row, modifiers, raw: Vec::new() })
    }

    /// JS 的数是 f64（行号、请求号远小于 2^53）；wasm-bindgen 的 u64 参数要 BigInt，用起来容易错，这里收 f64
    pub fn history_req(&self, request_id: f64, epoch: f64, start_seq: f64, count: u32) -> Vec<u8> {
        encode(&ClientMsg::HistoryReq {
            request_id: request_id as u64,
            epoch: epoch as u64,
            start_seq: start_seq as u64,
            count,
        })
    }

    /// 实时通道解压后的字节。返回一个 JSON 数组（每项一个更新，见 `push_*`）。
    pub fn feed(&mut self, plain: &[u8]) -> String {
        self.framer.push(plain);
        let msgs: Vec<ServerMsg> = match self.framer.drain_all() {
            Ok(m) => m,
            Err(e) => return format!("[{{\"t\":\"resync\",\"why\":{:?}}}]", e.to_string()),
        };
        let mut out = String::from("[");
        let mut first = true;
        for msg in &msgs {
            let mark = out.len();
            if !first {
                out.push(',');
            }
            if self.push_msg(&mut out, msg) {
                first = false;
            } else {
                out.truncate(mark);
            }
        }
        out.push(']');
        out
    }

    /// 回滚通道（`/i/{id}/term-history`）解压后的字节。
    pub fn feed_history(&mut self, plain: &[u8]) -> String {
        self.history_framer.push(plain);
        let msgs: Vec<ServerMsg> = match self.history_framer.drain_all() {
            Ok(m) => m,
            Err(e) => return format!("[{{\"t\":\"resync\",\"why\":{:?}}}]", e.to_string()),
        };
        let mut out = String::from("[");
        let mut first = true;
        for msg in &msgs {
            let mark = out.len();
            if !first {
                out.push(',');
            }
            if self.push_msg(&mut out, msg) {
                first = false;
            } else {
                out.truncate(mark);
            }
        }
        out.push(']');
        out
    }

    pub fn cols(&self) -> u16 {
        self.screen.snapshot().map_or(0, |s| s.cols)
    }

    pub fn rows(&self) -> u16 {
        self.screen.snapshot().map_or(0, |s| s.rows)
    }

    pub fn frame(&self) -> f64 {
        self.screen.frame() as f64
    }

    /// 屏幕第 `i` 行的段（JSON 数组）
    pub fn row(&self, i: u16) -> String {
        let mut out = String::new();
        match self.screen.snapshot().and_then(|s| s.lines.get(i as usize).map(|l| (s, l))) {
            Some((s, line)) => push_row_runs(&mut out, &line.cells, &s.style_table.styles, &s.style_table.hyperlinks),
            None => out.push_str("[]"),
        }
        out
    }

    /// 屏幕第 `i` 行的纯文字（行尾空白去掉），给复制、`window.term.buffer` 兼容层用
    pub fn row_text(&self, i: u16) -> String {
        self.screen
            .snapshot()
            .and_then(|s| s.lines.get(i as usize))
            .map(|l| row_plain_text(&l.cells))
            .unwrap_or_default()
    }

    /// 第 `i` 行是不是上一行自动折过来的（这一行的 `wrapped` 表示它折到了下一行）
    pub fn row_wrapped(&self, i: u16) -> bool {
        self.screen
            .snapshot()
            .and_then(|s| s.lines.get(i as usize))
            .is_some_and(|l| l.wrapped)
    }
}

impl TermClient {
    /// 还没确认的本地预测
    pub fn predicted(&self) -> &[Prediction] {
        self.predict.predictions()
    }

    pub fn screen(&self) -> Option<&ScreenSnapshot> {
        self.screen.snapshot()
    }

    pub fn history(&self) -> Option<&History> {
        self.history.as_ref()
    }

    /// 屏幕第 0 行的行号（回滚序号接着往下数）
    pub fn screen_base(&self) -> u64 {
        self.screen.snapshot().map_or(0, |s| s.scrollback_next_seq)
    }

    /// 行号 → 一行：`>= screen_base` 是屏幕上的，小于的是回滚区里的
    pub fn line(&self, abs: u64) -> Option<LineRef<'_>> {
        let s = self.screen.snapshot()?;
        let base = s.scrollback_next_seq;
        if abs >= base {
            let r = s.lines.get((abs - base) as usize)?;
            return Some(LineRef::Screen(r, &s.style_table));
        }
        self.history.as_ref()?.get(abs).map(LineRef::Packed)
    }

    fn push_msg(&mut self, out: &mut String, msg: &ServerMsg) -> bool {
        match msg {
            ServerMsg::Welcome { client_id, session_id, protocol } => {
                out.push_str("{\"t\":\"welcome\",\"caps\":");
                // 协议名后面空格分隔的能力（老 termd 没有）
                push_json_str(out, protocol.split_once(' ').map_or("", |(_, caps)| caps));
                out.push_str(",\"client\":");
                push_json_str(out, client_id);
                out.push_str(",\"session\":");
                push_json_str(out, session_id);
                out.push('}');
                true
            }
            ServerMsg::Snapshot { .. } | ServerMsg::Delta(_) => {
                let (full, shifts, mut dirty): (bool, Vec<(u16, u16, i16)>, BTreeSet<u16>) = match msg {
                    ServerMsg::Delta(d) => {
                        let mut dirty: BTreeSet<u16> = d.changed_rows.iter().map(|p| p.row).collect();
                        // 移动后空出来的行（不一定在 changed_rows 里：差量是相对“移动后”的状态算的）
                        for op in &d.scroll_ops {
                            let n = op.delta.unsigned_abs().min(op.bottom.saturating_sub(op.top));
                            if op.delta > 0 {
                                dirty.extend(op.bottom.saturating_sub(n)..op.bottom);
                            } else {
                                dirty.extend(op.top..op.top + n);
                            }
                        }
                        (false, d.scroll_ops.iter().map(|o| (o.top, o.bottom, o.delta)).collect(), dirty)
                    }
                    _ => (true, Vec::new(), BTreeSet::new()),
                };
                if let Err(e) = self.screen.apply_server_msg(msg) {
                    // 帧对不上（丢了中间的帧、termd 换了编码器）：JS 端重连拿一个完整的 Snapshot
                    let _ = write!(out, "{{\"t\":\"resync\",\"why\":");
                    push_json_str(out, &e.to_string());
                    out.push('}');
                    return true;
                }
                let Some(s) = self.screen.snapshot() else { return false };
                if let Some(h) = self.history.as_mut() {
                    h.set_bounds(s.scrollback_epoch, s.scrollback_oldest_seq, s.scrollback_next_seq);
                }
                if full {
                    dirty = (0..s.rows).collect();
                }
                let acked = self.screen.acked_input_seq();
                while self.pending.front().is_some_and(|(seq, _)| *seq <= acked) {
                    self.pending.pop_front();
                }
                if self.next_input_seq <= acked {
                    self.next_input_seq = acked + 1;
                }
                self.predict.on_frame(s, acked, self.next_input_seq - 1);
                push_screen(out, s, self.screen.frame(), acked, full, &shifts, &dirty);
                true
            }
            ServerMsg::Scrolled { epoch, rows } if self.history.is_some() => {
                let h = self.history.as_mut().expect("checked");
                for row in rows {
                    h.insert(*epoch, row);
                }
                let _ = write!(out, "{{\"t\":\"scrolled\",\"epoch\":{epoch},\"n\":{}}}", rows.len());
                true
            }
            ServerMsg::Scrolled { epoch, rows } => {
                let _ = write!(out, "{{\"t\":\"scrolled\",\"epoch\":{epoch},\"rows\":[");
                for (i, row) in rows.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    push_scrollback_row(out, row);
                }
                out.push_str("]}");
                true
            }
            ServerMsg::Event(ev) => {
                if let (CoreEvent::ScrollbackCleared { epoch }, Some(h)) = (ev, self.history.as_mut()) {
                    h.reset(*epoch);
                }
                push_event(out, ev)
            }
            ServerMsg::HistoryChunk { request_id, slice, epoch_mismatch } => {
                if let Some(h) = self.history.as_mut() {
                    if !*epoch_mismatch {
                        for row in &slice.rows {
                            h.insert(slice.epoch, row);
                        }
                        h.set_bounds(slice.epoch, slice.oldest_seq, h.next.max(slice.next_seq));
                    }
                    let _ = write!(
                        out,
                        "{{\"t\":\"history\",\"id\":{request_id},\"mismatch\":{epoch_mismatch},\"epoch\":{},\"start\":{},\"oldest\":{},\"next\":{},\"n\":{},\"rows\":[]}}",
                        slice.epoch,
                        slice.start_seq,
                        slice.oldest_seq,
                        slice.next_seq,
                        slice.rows.len()
                    );
                } else {
                    push_history(out, *request_id, slice, *epoch_mismatch);
                }
                true
            }
            ServerMsg::Raw { id, data } => {
                let _ = write!(out, "{{\"t\":\"raw\",\"id\":{id},\"n\":{}}}", data.len());
                self.raw.push_back(data.clone());
                true
            }
            ServerMsg::Error { code, message } => {
                out.push_str("{\"t\":\"error\",\"code\":");
                push_json_str(out, code);
                out.push_str(",\"msg\":");
                push_json_str(out, message);
                out.push('}');
                true
            }
        }
    }
}

fn push_screen(
    out: &mut String,
    s: &ScreenSnapshot,
    frame: u64,
    acked: u64,
    full: bool,
    shifts: &[(u16, u16, i16)],
    dirty: &BTreeSet<u16>,
) {
    let m = &s.modes;
    let c = &s.cursor;
    let _ = write!(
        out,
        "{{\"t\":\"screen\",\"frame\":{frame},\"full\":{full},\"cols\":{},\"rows\":{},\"seq\":{acked},\"shifts\":[",
        s.cols, s.rows
    );
    for (i, (top, bottom, delta)) in shifts.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        let _ = write!(out, "[{top},{bottom},{delta}]");
    }
    out.push_str("],\"dirty\":[");
    for (i, r) in dirty.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        let _ = write!(out, "{r}");
    }
    let _ = write!(
        out,
        "],\"cursor\":{{\"r\":{},\"c\":{},\"v\":{},\"s\":\"{}\",\"blink\":{}}}",
        c.row,
        c.col,
        c.visible && m.show_cursor && c.shape != CursorShape::Hidden,
        shape_name(c.shape),
        c.blinking
    );
    let _ = write!(
        out,
        ",\"alt\":{},\"mouse\":\"{}\",\"appCursor\":{},\"bracketed\":{},\"altScroll\":{}",
        m.alt_screen,
        mouse_name(m.mouse_protocol),
        m.app_cursor,
        m.bracketed_paste,
        m.alternate_scroll
    );
    out.push_str(",\"title\":");
    push_opt_str(out, s.title.as_deref());
    let _ = write!(
        out,
        ",\"sb\":[{},{},{}]",
        s.scrollback_epoch, s.scrollback_oldest_seq, s.scrollback_next_seq
    );
    out.push_str(",\"palette\":[");
    for (i, (idx, rgb)) in s.palette_overrides.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        let _ = write!(out, "[{idx},{}]", RGB_BASE + ((rgb.r as i64) << 16 | (rgb.g as i64) << 8 | rgb.b as i64));
    }
    out.push_str("]}");
}

#[cfg(test)]
mod tests;
