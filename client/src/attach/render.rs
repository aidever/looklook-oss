//! `looklook attach` 的画法（设计稿 §8.3，参照 Mosh 客户端）：手上记着本地终端现在显示的画面，
//! 每来一帧算出“现在 → 新画面”要输出的最少转义序列。
//!
//! - 普通屏幕、本地尺寸和 termd 一样时，滚出顶部的行用“光标在最底行 + 换行”真的滚上去，于是进了**本地终端自己的回滚区**；
//!   帧之间滚过去、一次都没显示过的行（`Scrolled` 里带来的）也按顺序滚一遍，回滚区里不缺行；
//! - 备用屏幕（vim、htop）本地也切到备用屏幕，退出时本地终端自己恢复普通屏幕，我们记着进去之前的画面；
//! - 尺寸不一样（还有别的端在看、尺寸归它）时只按左上角对齐画，不往回滚区推；
//! - 宽字以 termd 为准：每个宽字写完都显式定位光标，本地对字宽看法不同时最多错这一个字；
//! - 每行只重写第一个变化到最后一个变化之间的格子，行尾变空用 `EL` 擦掉。

use std::collections::BTreeMap;
use std::fmt::Write as _;

use term_proto::{
    CellData, CellSpacer, ColorSpec, CursorShape, MouseEncoding, MouseProtocol, NamedColorCode, ScreenSnapshot,
    ScrollbackRow, Style, StyleFlags, TermModes,
};

/// 一格（样式已经解析出来：屏幕行和回滚行的样式表不是同一张）
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cell {
    pub c: char,
    pub extra: String,
    pub width: u8,
    pub spacer: CellSpacer,
    pub style: Style,
}

impl Cell {
    fn blank() -> Self {
        Cell { c: ' ', extra: String::new(), width: 1, spacer: CellSpacer::None, style: Style::default() }
    }

    fn is_blank(&self) -> bool {
        (self.c == ' ' || self.c == '\0' || self.c == '\t')
            && self.extra.is_empty()
            && self.spacer == CellSpacer::None
            && plain(&self.style)
    }
}

/// 样式里不看超链接（不往本地终端发 OSC 8）
fn plain(s: &Style) -> bool {
    let d = Style::default();
    s.fg == d.fg && s.bg == d.bg && s.underline_color == d.underline_color && s.flags.is_empty()
}

fn same_look(a: &Style, b: &Style) -> bool {
    a.fg == b.fg && a.bg == b.bg && a.underline_color == b.underline_color && a.flags == b.flags
}

type Line = Vec<Cell>;

fn resolve(cells: &[CellData], styles: &[Style], cols: u16) -> Line {
    let mut out: Line = cells
        .iter()
        .take(cols as usize)
        .map(|c| Cell {
            c: c.c,
            extra: c.extra.clone(),
            width: c.width,
            spacer: c.spacer,
            style: styles.get(c.style_id as usize).cloned().unwrap_or_default(),
        })
        .collect();
    out.resize(cols as usize, Cell::blank());
    out
}

fn screen_lines(s: &ScreenSnapshot) -> Vec<Line> {
    s.lines.iter().map(|l| resolve(&l.cells, &s.style_table.styles, s.cols)).collect()
}

/// 宽字的后半格被单独擦掉、前半格还在（内核的边角状态）：本地终端画出来的样子各家不一样，
/// 记成“不知道”（空行），下次这一行有变化时整行重写
fn broken(line: &Line) -> bool {
    line.iter().enumerate().any(|(i, c)| {
        (c.width >= 2 && c.spacer == CellSpacer::None && line.get(i + 1).is_none_or(|n| n.spacer != CellSpacer::WideTrailing))
            || (c.spacer == CellSpacer::WideTrailing && (i == 0 || line[i - 1].width < 2))
    })
}

fn remember(lines: Vec<Line>) -> Vec<Line> {
    lines.into_iter().map(|l| if broken(&l) { Vec::new() } else { l }).collect()
}

/// 本地终端上现在是什么样
#[derive(Clone, Debug)]
struct Shown {
    lines: Vec<Line>,
    /// 第 0 行的回滚序号（普通屏幕）
    base: u64,
    epoch: u64,
}

pub struct Painter {
    cols: u16,
    rows: u16,
    shown: Option<Shown>,
    /// 进备用屏幕前普通屏幕的样子（本地终端出来时会恢复它）
    saved_primary: Option<Shown>,
    alt: bool,
    modes: TermModes,
    title: Option<String>,
    cursor: Option<(u16, u16, bool, CursorShape, bool)>,
    /// 还没推进本地回滚区的滚出行（序号 → 行）
    pending: BTreeMap<u64, Line>,
    /// 当前输出流里的样式（`None`：不确定，下次先重置）
    pen: Option<Style>,
}

fn cup(out: &mut String, row: u16, col: u16) {
    let _ = write!(out, "\x1b[{};{}H", row + 1, col + 1);
}

fn color(out: &mut String, c: ColorSpec, base: u8, bright: u8, ext: u8) -> bool {
    // 返回是否要再加暗淡（Dim* 具名色）
    match c {
        ColorSpec::Indexed(i) => {
            let _ = write!(out, ";{ext};5;{i}");
            false
        }
        ColorSpec::Rgb(rgb) => {
            let _ = write!(out, ";{ext};2;{};{};{}", rgb.r, rgb.g, rgb.b);
            false
        }
        ColorSpec::Named(n) => {
            let v = n as u16;
            match n {
                NamedColorCode::DimForeground => true,
                _ if v < 8 => {
                    let _ = write!(out, ";{}", base as u16 + v);
                    false
                }
                _ if v < 16 => {
                    let _ = write!(out, ";{}", bright as u16 + v - 8);
                    false
                }
                _ if (259..=266).contains(&v) => {
                    let _ = write!(out, ";{}", base as u16 + v - 259);
                    true
                }
                _ => false,
            }
        }
    }
}

fn sgr(out: &mut String, s: &Style) {
    out.push_str("\x1b[0");
    let f = s.flags;
    let mut dim = f.contains(StyleFlags::DIM);
    if f.contains(StyleFlags::BOLD) {
        out.push_str(";1");
    }
    if f.contains(StyleFlags::ITALIC) {
        out.push_str(";3");
    }
    if f.contains(StyleFlags::UNDERCURL) {
        out.push_str(";4:3");
    } else if f.contains(StyleFlags::DOUBLE_UNDERLINE) {
        out.push_str(";4:2");
    } else if f.contains(StyleFlags::DOTTED_UNDERLINE) {
        out.push_str(";4:4");
    } else if f.contains(StyleFlags::DASHED_UNDERLINE) {
        out.push_str(";4:5");
    } else if f.contains(StyleFlags::UNDERLINE) {
        out.push_str(";4");
    }
    if f.contains(StyleFlags::INVERSE) {
        out.push_str(";7");
    }
    if f.contains(StyleFlags::HIDDEN) {
        out.push_str(";8");
    }
    if f.contains(StyleFlags::STRIKEOUT) {
        out.push_str(";9");
    }
    dim |= color(out, s.fg, 30, 90, 38);
    color(out, s.bg, 40, 100, 48);
    if let Some(u) = s.underline_color {
        match u {
            ColorSpec::Indexed(i) => {
                let _ = write!(out, ";58;5;{i}");
            }
            ColorSpec::Rgb(rgb) => {
                let _ = write!(out, ";58;2;{};{};{}", rgb.r, rgb.g, rgb.b);
            }
            ColorSpec::Named(n) if (n as u16) < 16 => {
                let _ = write!(out, ";58;5;{}", n as u16);
            }
            ColorSpec::Named(_) => {}
        }
    }
    if dim {
        out.push_str(";2");
    }
    out.push('m');
}

fn flag(out: &mut String, n: u16, on: bool) {
    let _ = write!(out, "\x1b[?{n}{}", if on { 'h' } else { 'l' });
}

fn mouse_mode(p: MouseProtocol) -> Option<u16> {
    match p {
        MouseProtocol::None => None,
        MouseProtocol::Normal => Some(1000),
        MouseProtocol::Drag => Some(1002),
        MouseProtocol::AnyMotion => Some(1003),
    }
}

fn mouse_enc(e: MouseEncoding) -> Option<u16> {
    match e {
        MouseEncoding::X10 => None,
        MouseEncoding::Utf8 => Some(1005),
        MouseEncoding::Sgr => Some(1006),
    }
}

/// 程序开的输入相关模式照搬到本地终端（本地终端编好的按键、鼠标字节原样发给程序）
fn sync_modes(out: &mut String, old: &TermModes, new: &TermModes) {
    if old.app_cursor != new.app_cursor {
        flag(out, 1, new.app_cursor);
    }
    if old.app_keypad != new.app_keypad {
        out.push_str(if new.app_keypad { "\x1b=" } else { "\x1b>" });
    }
    if old.bracketed_paste != new.bracketed_paste {
        flag(out, 2004, new.bracketed_paste);
    }
    if old.focus_in_out != new.focus_in_out {
        flag(out, 1004, new.focus_in_out);
    }
    if old.alternate_scroll != new.alternate_scroll {
        flag(out, 1007, new.alternate_scroll);
    }
    if old.mouse_protocol != new.mouse_protocol {
        if let Some(m) = mouse_mode(old.mouse_protocol) {
            flag(out, m, false);
        }
        if let Some(m) = mouse_mode(new.mouse_protocol) {
            flag(out, m, true);
        }
    }
    if old.mouse_encoding != new.mouse_encoding {
        if let Some(m) = mouse_enc(old.mouse_encoding) {
            flag(out, m, false);
        }
        if let Some(m) = mouse_enc(new.mouse_encoding) {
            flag(out, m, true);
        }
    }
}

impl Painter {
    pub fn new(cols: u16, rows: u16) -> Self {
        Painter {
            cols,
            rows,
            shown: None,
            saved_primary: None,
            alt: false,
            modes: TermModes::default(),
            title: None,
            cursor: None,
            pending: BTreeMap::new(),
            pen: None,
        }
    }

    /// 本地终端改了尺寸：本地终端自己会重排，我们不知道它排成什么样，下一帧整屏重画
    pub fn resize(&mut self, cols: u16, rows: u16) {
        if (cols, rows) != (self.cols, self.rows) {
            self.cols = cols;
            self.rows = rows;
            if let Some(s) = self.shown.as_mut() {
                s.lines.clear(); // 当作什么都不知道：全部重画（回滚区里已有的不动）
            }
            self.saved_primary = None;
        }
    }

    /// 滚出 termd 屏幕的行（实时的 `Scrolled`，或刚连上时取的一段回滚）
    pub fn add_scrollback(&mut self, rows: &[ScrollbackRow]) {
        let pushed = self.shown.as_ref().filter(|_| !self.alt).map_or(0, |s| s.base);
        for r in rows {
            if self.shown.is_some() && r.seq < pushed {
                continue;
            }
            let cols = (r.cells.len() as u16).max(1);
            let mut line = resolve(&r.cells, &r.styles, cols);
            while line.last().is_some_and(Cell::is_blank) {
                line.pop();
            }
            self.pending.insert(r.seq, line);
        }
    }

    /// 刚连上、还没画过
    pub fn started(&self) -> bool {
        self.shown.is_some()
    }

    fn aligned(&self, s: &ScreenSnapshot) -> bool {
        s.cols == self.cols && s.rows == self.rows
    }

    fn set_pen(&mut self, out: &mut String, s: &Style) {
        if self.pen.as_ref().is_some_and(|p| same_look(p, s)) {
            return;
        }
        if plain(s) {
            out.push_str("\x1b[0m");
        } else {
            sgr(out, s);
        }
        self.pen = Some(s.clone());
    }

    /// 从当前光标位置写 `cells[a..b]`（光标已在第 `row` 行第 `a` 列）
    fn put_cells(&mut self, out: &mut String, row: u16, line: &Line, a: usize, b: usize) {
        let mut col = a;
        while col < b {
            let cell = &line[col];
            match cell.spacer {
                CellSpacer::WideTrailing => {
                    col += 1;
                    continue;
                }
                CellSpacer::WideLeading => {
                    self.set_pen(out, &cell.style);
                    out.push(' ');
                    col += 1;
                    continue;
                }
                CellSpacer::None => {}
            }
            self.set_pen(out, &cell.style);
            let c = if cell.c == '\0' || cell.c == '\t' || (cell.c as u32) < 0x20 { ' ' } else { cell.c };
            out.push(c);
            out.push_str(&cell.extra);
            if cell.width >= 2 {
                // 后半格通常是占位；被别的字盖掉时（光标退到后半格再写）下一格照样要写
                let owns_next = line.get(col + 1).is_some_and(|n| n.spacer == CellSpacer::WideTrailing);
                col += if owns_next { 2 } else { 1 };
                // 本地终端对这个字的宽度看法可能不同：后面还要写就显式定位
                if col < b {
                    cup(out, row, col as u16);
                }
            } else {
                col += 1;
            }
        }
    }

    /// 第 `row` 行从 `old` 改成 `new`
    fn diff_line(&mut self, out: &mut String, row: u16, old: Option<&Line>, new: &Line) {
        let width = new.len().min(self.cols as usize);
        let differs = |i: usize| old.and_then(|o| o.get(i)) != Some(&new[i]);
        let Some(mut a) = (0..width).find(|&i| differs(i)) else { return };
        let b = (0..width).rev().find(|&i| differs(i)).map_or(a, |i| i + 1);
        // 从宽字的后半格开始改：退到前半格
        while a > 0 && new[a].spacer == CellSpacer::WideTrailing {
            a -= 1;
        }
        let end = new[..width].iter().rposition(|c| !c.is_blank()).map_or(0, |i| i + 1);
        cup(out, row, a as u16);
        if b >= end && end < width {
            // 后面变成空白：写到最后一个非空格子，再擦到行尾
            if a < end {
                self.put_cells(out, row, new, a, end);
                if new[end - 1].width >= 2 || new[end - 1].spacer == CellSpacer::WideTrailing {
                    cup(out, row, end as u16);
                }
            }
            self.set_pen(out, &Style::default());
            out.push_str("\x1b[K");
        } else {
            self.put_cells(out, row, new, a, b);
        }
    }

    fn redraw_all(&mut self, out: &mut String, lines: &[Line], clear: bool) {
        if clear {
            self.set_pen(out, &Style::default());
            out.push_str("\x1b[H\x1b[2J");
        }
        let n = lines.len().min(self.rows as usize);
        for (r, line) in lines.iter().take(n).enumerate() {
            self.diff_line(out, r as u16, None, line);
        }
    }

    /// 刚连上（或本地改了尺寸）：普通屏幕下先把本地屏幕上原来的东西推进回滚区，打出取到的回滚行，再打出整屏
    fn init_primary(&mut self, out: &mut String, s: &ScreenSnapshot, lines: Vec<Line>) {
        self.set_pen(out, &Style::default());
        out.push('\r');
        for _ in 0..self.rows {
            out.push('\n');
        }
        out.push_str("\x1b[H");
        let base = s.scrollback_next_seq;
        let history: Vec<Line> = std::mem::take(&mut self.pending).into_iter().filter(|(seq, _)| *seq < base).map(|(_, l)| l).collect();
        for line in &history {
            self.print_line(out, line);
            out.push_str("\r\n");
        }
        for (i, line) in lines.iter().enumerate() {
            if i > 0 {
                out.push_str("\r\n");
            }
            self.print_line(out, line);
        }
        self.shown = Some(Shown { lines: remember(lines), base, epoch: s.scrollback_epoch });
    }

    /// 从行首（光标所在位置）按顺序写一整行，行尾空白不写
    fn print_line(&mut self, out: &mut String, line: &Line) {
        let end = line.iter().rposition(|c| !c.is_blank()).map_or(0, |i| i + 1).min(self.cols as usize);
        // 写满最后一列后光标停在行尾“待换行”状态，下一个 \r\n 正常换行
        let mut col = 0;
        while col < end {
            let cell = &line[col];
            if cell.spacer == CellSpacer::WideTrailing {
                col += 1;
                continue;
            }
            self.set_pen(out, &cell.style);
            let c = if cell.spacer == CellSpacer::WideLeading || cell.c == '\0' || cell.c == '\t' || (cell.c as u32) < 0x20 {
                ' '
            } else {
                cell.c
            };
            out.push(c);
            out.push_str(&cell.extra);
            if cell.width >= 2 && cell.spacer == CellSpacer::None {
                let owns_next = line.get(col + 1).is_some_and(|n| n.spacer == CellSpacer::WideTrailing);
                col += if owns_next { 2 } else { 1 };
                if !owns_next {
                    // 被盖掉了后半格：退回去接着写（\x08 在各终端里都退一格）
                    out.push('\x08');
                }
            } else {
                col += 1;
            }
        }
        self.set_pen(out, &Style::default());
    }

    /// 普通屏幕往上滚了 `new_base - shown.base` 行：在最底行换行，把行真的滚进本地回滚区
    fn scroll_up(&mut self, out: &mut String, new_base: u64, new_lines: &[Line]) {
        let Some(mut shown) = self.shown.take() else { return };
        let rows = self.rows as u64;
        let k = new_base - shown.base;
        self.set_pen(out, &Style::default());
        cup(out, self.rows - 1, 0);
        for i in 0..k {
            // 要滚出去的最顶行在两帧之间变过（先写了内容再滚走）：先在原地改好，进回滚区的才是对的
            if let Some(mut top) = self.pending.remove(&(shown.base + i)) {
                top.resize(self.cols as usize, Cell::blank());
                let old = shown.lines.first().cloned();
                self.diff_line(out, 0, old.as_ref(), &top);
                if let Some(first) = shown.lines.first_mut() {
                    *first = top;
                }
                self.set_pen(out, &Style::default());
                cup(out, self.rows - 1, 0);
            }
            out.push('\n');
            // 新露出来的最底行是第几行
            let seq = shown.base + i + rows;
            let line = if seq < new_base {
                self.pending.remove(&seq).unwrap_or_default()
            } else {
                new_lines.get((seq - new_base) as usize).cloned().unwrap_or_default()
            };
            out.push('\r');
            self.print_line(out, &line);
            if !shown.lines.is_empty() {
                shown.lines.remove(0);
            }
            let mut full = line;
            full.resize(self.cols as usize, Cell::blank());
            shown.lines.push(if broken(&full) { Vec::new() } else { full });
        }
        self.pending = self.pending.split_off(&new_base);
        shown.base = new_base;
        self.shown = Some(shown);
    }

    /// 新的一帧 → 要写给本地终端的字节
    pub fn frame(&mut self, s: &ScreenSnapshot) -> String {
        let mut out = String::new();
        out.push_str("\x1b[?25l"); // 画的时候藏起光标，免得闪
        let lines = screen_lines(s);
        let aligned = self.aligned(s);
        let alt = s.modes.alt_screen;

        if alt != self.alt {
            if alt {
                // 进备用屏幕：记住普通屏幕现在的样子，本地也切过去
                self.saved_primary = self.shown.take();
                flag(&mut out, 1049, true);
                self.pen = None;
                self.redraw_all(&mut out, &lines, true);
                self.shown = Some(Shown { lines: remember(lines.clone()), base: s.scrollback_next_seq, epoch: s.scrollback_epoch });
            } else {
                flag(&mut out, 1049, false);
                self.pen = None;
                self.shown = self.saved_primary.take().filter(|p| !p.lines.is_empty());
            }
            self.alt = alt;
        }

        match self.shown.take() {
            None if !alt && aligned => self.init_primary(&mut out, s, lines),
            None => {
                self.redraw_all(&mut out, &lines, true);
                self.shown = Some(Shown { lines: remember(lines), base: s.scrollback_next_seq, epoch: s.scrollback_epoch });
            }
            Some(shown) if shown.lines.is_empty() => {
                drop(shown);
                // 尺寸变了：整屏重来（普通屏幕时也重新推一遍，本地回滚区里会有一份重复的旧屏幕，可以接受）
                self.shown = None;
                if !alt && aligned {
                    self.init_primary(&mut out, s, lines);
                } else {
                    self.redraw_all(&mut out, &lines, true);
                    self.shown = Some(Shown { lines: remember(lines), base: s.scrollback_next_seq, epoch: s.scrollback_epoch });
                }
            }
            Some(shown) => {
                let base = s.scrollback_next_seq;
                let scrolls = !alt && aligned && shown.epoch == s.scrollback_epoch && base > shown.base;
                self.shown = Some(shown);
                if scrolls {
                    self.scroll_up(&mut out, base, &lines);
                }
                let mut shown = self.shown.take().expect("set above");
                for (r, line) in lines.iter().enumerate().take(self.rows as usize) {
                    self.diff_line(&mut out, r as u16, shown.lines.get(r), line);
                }
                shown.lines = remember(lines);
                shown.base = base;
                shown.epoch = s.scrollback_epoch;
                self.shown = Some(shown);
                if alt || !aligned {
                    self.pending.clear();
                }
            }
        }

        // 光标、模式、标题
        sync_modes(&mut out, &self.modes.clone(), &s.modes);
        self.modes = s.modes;
        let c = &s.cursor;
        let visible = c.visible && s.modes.show_cursor && c.shape != CursorShape::Hidden;
        let shape_code = match (c.shape, c.blinking) {
            (CursorShape::Block | CursorShape::HollowBlock, true) => 1,
            (CursorShape::Block | CursorShape::HollowBlock, false) => 2,
            (CursorShape::Underline, true) => 3,
            (CursorShape::Underline, false) => 4,
            (CursorShape::Beam, true) => 5,
            (CursorShape::Beam, false) => 6,
            (CursorShape::Hidden, _) => 0,
        };
        let cur = (c.row.min(self.rows.saturating_sub(1)), c.col.min(self.cols.saturating_sub(1)), visible, c.shape, c.blinking);
        if visible && self.cursor.is_none_or(|p| (p.3, p.4) != (cur.3, cur.4)) && shape_code > 0 {
            let _ = write!(out, "\x1b[{shape_code} q");
        }
        self.set_pen(&mut out, &Style::default());
        cup(&mut out, cur.0, cur.1);
        if visible {
            out.push_str("\x1b[?25h");
        }
        self.cursor = Some(cur);
        if s.title != self.title {
            self.title = s.title.clone();
            let t: String = self.title.as_deref().unwrap_or("").chars().filter(|c| !c.is_control()).collect();
            let _ = write!(out, "\x1b]2;{t}\x07");
        }
        out
    }

    /// 退出时还原本地终端：关掉我们打开的模式，回到普通屏幕，光标放到最后一行下面
    pub fn finish(&mut self) -> String {
        let mut out = String::new();
        sync_modes(&mut out, &self.modes.clone(), &TermModes::default());
        if self.alt {
            flag(&mut out, 1049, false);
        }
        out.push_str("\x1b[0m\x1b[?25h\x1b[0 q");
        cup(&mut out, self.rows.saturating_sub(1), 0);
        out.push_str("\r\n");
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use term_core::{AlacrittyCore, CoreConfig, TerminalCore};

    fn core(cols: u16, rows: u16) -> AlacrittyCore {
        AlacrittyCore::new(CoreConfig { cols, rows, scrollback_limit: 10_000 }).unwrap()
    }

    fn text(lines: &[Line]) -> Vec<String> {
        lines
            .iter()
            .map(|l| {
                let mut s = String::new();
                for (i, c) in l.iter().enumerate() {
                    if c.spacer == CellSpacer::WideTrailing && (i == 0 || l[i - 1].width < 2) {
                        s.push(' '); // 前半格被擦掉了：看上去是空白
                    }
                    if c.spacer == CellSpacer::None {
                        s.push(if c.c == '\0' { ' ' } else { c.c });
                        s.push_str(&c.extra);
                    }
                }
                s.trim_end().to_string()
            })
            .collect()
    }

    /// 远端（termd 的内核）→ 帧 → Painter → 本地终端（另一个内核）：本地屏幕和远端一样，回滚区也一样
    struct Pair {
        remote: AlacrittyCore,
        local: AlacrittyCore,
        painter: Painter,
        local_history: Vec<String>,
        last: (String, String),
    }

    impl Pair {
        fn new(cols: u16, rows: u16) -> Self {
            Pair { remote: core(cols, rows), local: core(cols, rows), painter: Painter::new(cols, rows), local_history: Vec::new(), last: Default::default() }
        }

        fn step(&mut self, bytes: &[u8]) {
            let first = !self.painter.started();
            self.remote.feed_bytes(bytes);
            let (_, rows) = self.remote.take_new_scrollback();
            self.painter.add_scrollback(&rows);
            let out = self.painter.frame(&self.remote.snapshot());
            self.last = (String::from_utf8_lossy(bytes).into_owned(), out.clone());
            self.local.feed_bytes(out.as_bytes());
            let (_, rows) = self.local.take_new_scrollback();
            for r in rows {
                let l = resolve(&r.cells, &r.styles, r.cells.len() as u16);
                self.local_history.extend(text(&[l]));
            }
            if first {
                // 第一次画会把本地屏幕上原来的东西（这里是空行）推进回滚区
                self.local_history.clear();
            }
        }

        fn check(&self) {
            let r = self.remote.snapshot();
            let l = self.local.snapshot();
            // 宽字后半格被单独擦掉、前半格还在的行（内核的边角状态，各终端处理不一）不比
            let keep: Vec<bool> = screen_lines(&r).iter().map(|l| !super::broken(l)).collect();
            let pick = |lines: Vec<String>| -> Vec<String> { lines.into_iter().zip(&keep).filter(|(_, k)| **k).map(|(l, _)| l).collect() };
            assert_eq!(
                pick(text(&screen_lines(&l))),
                pick(text(&screen_lines(&r))),
                "screen text; input {:?}; painted {:?}; cells {:?}",
                self.last.0,
                self.last.1,
                screen_lines(&r).iter().zip(screen_lines(&l)).filter(|(a, b)| text(&[(*a).clone()]) != text(&[(*b).clone()])).map(|(a, b)| (a.iter().take(5).map(|c| (c.c, c.width, c.spacer)).collect::<Vec<_>>(), b.iter().take(5).map(|c| (c.c, c.width, c.spacer)).collect::<Vec<_>>())).collect::<Vec<_>>()
            );
            for ((lr, rr), _) in screen_lines(&l).iter().zip(screen_lines(&r)).zip(&keep).filter(|(_, k)| **k) {
                for (a, b) in lr.iter().zip(rr.iter()) {
                    assert!(same_look(&a.style, &b.style) || (a.is_blank() && b.is_blank()), "style {:?} vs {:?}", a, b);
                }
            }
            if !r.modes.alt_screen {
                assert_eq!((l.cursor.row, l.cursor.col), (r.cursor.row, r.cursor.col), "cursor");
            }
        }
    }

    #[test]
    fn mirrors_screen_and_scrollback() {
        let mut p = Pair::new(30, 6);
        p.step(b"hello\r\n\x1b[31mred\x1b[0m \x1b[1;4mbold\x1b[0m\r\n");
        p.check();
        // 一帧里滚过很多行：每一行都要进本地回滚区
        let mut big = Vec::new();
        for i in 0..40 {
            big.extend_from_slice(format!("line {i}\r\n").as_bytes());
        }
        p.step(&big);
        p.check();
        let mut want = vec!["hello".to_string(), "red bold".to_string()];
        want.extend((0..35).map(|i| format!("line {i}")));
        assert_eq!(p.local_history, want);
        // 中文宽字、行内改写、行尾擦除
        p.step("你好世界 abc\r".as_bytes());
        p.check();
        p.step(b"\x1b[2C\x1b[K");
        p.check();
    }

    /// 随机输出、随机切帧：每帧之后本地屏幕都和远端一样；不用滚动区域时回滚区也一样
    #[test]
    fn random_streams_stay_in_sync() {
        let mut seed = 0x2545_f491_4f6c_dd1du64;
        let mut rnd = move |n: u64| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed % n
        };
        for round in 0..40 {
            let regions = round % 4 == 3;
            let mut p = Pair::new(24, 8);
            p.step(b"");
            let mut all_history: Vec<String> = Vec::new();
            for _ in 0..60 {
                let mut chunk = String::new();
                for _ in 0..rnd(12) + 1 {
                    match rnd(14) {
                        0..=3 => chunk.push_str(["word ", "ab", "xyz123 ", "--"][rnd(4) as usize]),
                        4 => chunk.push_str(["你好", "表情😀", "宽字中文测试"][rnd(3) as usize]),
                        5 | 6 => chunk.push_str("\r\n"),
                        7 => chunk.push_str(&format!("\x1b[{}m", [0, 1, 31, 42, 7, 4, 38][rnd(7) as usize])),
                        8 => chunk.push_str(&format!("\x1b[{};{}H", rnd(8) + 1, rnd(24) + 1)),
                        9 => chunk.push_str(["\x1b[K", "\x1b[1K", "\x1b[2K"][rnd(3) as usize]),
                        10 => chunk.push_str(&format!("\x1b[{}D", rnd(5) + 1)),
                        11 if regions => chunk.push_str(&format!("\x1b[{};{}r", rnd(3) + 1, rnd(3) + 6)),
                        11 => chunk.push_str("\x1b[J"),
                        12 => chunk.push_str(&"long line that wraps around ".repeat(rnd(3) as usize + 1)),
                        _ => chunk.push_str("\x1b[2J"),
                    }
                }
                p.step(chunk.as_bytes());
                p.check();
                all_history = p.local_history.clone();
            }
            if !regions {
                let mut broken = Vec::new();
                let remote: Vec<String> = {
                    let mut out = Vec::new();
                    let (epoch, oldest, next, _) = p.remote.scrollback_info();
                    if let Ok(slice) = p.remote.history_slice(epoch, oldest, (next - oldest) as usize) {
                        for r in slice.rows {
                            let line = resolve(&r.cells, &r.styles, r.cells.len() as u16);
                            broken.push(super::broken(&line));
                            out.extend(text(&[line]));
                        }
                    }
                    out
                };
                assert_eq!(all_history.len(), remote.len(), "round {round}");
                let ok = |v: &[String]| -> Vec<String> { v.iter().zip(&broken).filter(|(_, b)| !**b).map(|(l, _)| l.clone()).collect() };
                assert_eq!(ok(&all_history), ok(&remote), "round {round}");
            }
        }
    }

    /// 排查用：把 `looklook attach` 的原始输出（LL_REPLAY 文件）喂给内核，打印回滚区和屏幕
    #[test]
    #[ignore]
    fn replay_file() {
        let Ok(path) = std::env::var("LL_REPLAY") else { return };
        let bytes = std::fs::read(path).unwrap();
        let (cols, rows) = (80, 24);
        let mut t = core(cols, rows);
        t.feed_bytes(&bytes);
        let (_, hist) = t.take_new_scrollback();
        for r in hist {
            println!("H|{}", text(&[resolve(&r.cells, &r.styles, r.cells.len() as u16)])[0]);
        }
        let s = t.snapshot();
        for l in text(&screen_lines(&s)) {
            println!("S|{l}");
        }
        println!("cursor {},{} alt={}", s.cursor.row, s.cursor.col, s.modes.alt_screen);
    }

    #[test]
    fn alt_screen_roundtrip() {
        let mut p = Pair::new(20, 5);
        p.step(b"$ vim\r\n");
        p.step(b"\x1b[?1049h\x1b[H\x1b[2J~\r\n~\r\n\x1b[7mstatus\x1b[0m");
        p.check();
        p.step(b"\x1b[2;1Hxx");
        p.check();
        p.step(b"\x1b[?1049l");
        p.check();
        p.step(b"after\r\n");
        p.check();
        assert!(p.local_history.is_empty(), "nothing scrolled: {:?}", p.local_history);
    }

    #[test]
    fn modes_follow_program() {
        let mut p = Pair::new(20, 5);
        p.step(b"\x1b[?2004h\x1b[?1h\x1b[?1000h\x1b[?1006h");
        let l = p.local.snapshot();
        assert!(l.modes.bracketed_paste && l.modes.app_cursor);
        assert_eq!(l.modes.mouse_protocol, MouseProtocol::Normal);
        assert_eq!(l.modes.mouse_encoding, MouseEncoding::Sgr);
        let fin = p.painter.finish();
        p.local.feed_bytes(fin.as_bytes());
        let l = p.local.snapshot();
        assert!(!l.modes.bracketed_paste && !l.modes.app_cursor);
        assert_eq!(l.modes.mouse_protocol, MouseProtocol::None);
    }

    #[test]
    fn size_mismatch_draws_top_left() {
        let mut p = Pair::new(20, 5);
        p.painter = Painter::new(20, 3);
        p.local = core(20, 3);
        p.step(b"a\r\nb\r\nc\r\nd");
        let l = p.local.snapshot();
        assert_eq!(text(&screen_lines(&l)), vec!["a", "b", "c"]);
    }
}
