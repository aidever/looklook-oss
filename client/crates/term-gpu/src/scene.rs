//! 一帧画什么（设计稿 §7.2）：屏幕 + 回滚区 → 一串实例（矩形或字形），一次 draw call 画完。
//! 先后顺序：背景色块 → 选区 / 搜索高亮 → 字和装饰线 → 光标。坐标全是设备像素，格子起点取整，字不糊不裁。

use bytemuck::{Pod, Zeroable};
use term_proto::{CursorShape, StyleFlags};
use term_web::TermClient;

use crate::select::{Search, Selection};
use crate::theme::{argb, mix, resolve, Theme, BG, CURSOR, CURSOR_TEXT, FG, SEARCH, SEARCH_CURRENT, SELECTION};

/// 一个实例：`kind` 0 = 纯色矩形，1 = 字形（单色，按 alpha 上前景色），2 = 彩色字形（emoji）
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Pod, Zeroable)]
pub struct Instance {
    pub rect: [f32; 4],
    pub uv: [f32; 2],
    pub kind: u32,
    pub pad: u32,
    pub color: [f32; 4],
}

pub const KIND_RECT: u32 = 0;
pub const KIND_MASK: u32 = 1;
pub const KIND_COLOR: u32 = 2;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct GlyphKey {
    pub text: String,
    /// 占几格（1 / 2）
    pub cells: u8,
    pub bold: bool,
    pub italic: bool,
    /// 波浪下划线（不是字）
    pub curl: bool,
    /// 字比底色暗（浅色主题）：按黑字白底光栅化，否则按白字黑底。浏览器画字的灰度 / 对比度补偿跟深浅有关，
    /// 同一张白字透明底的遮罩拿去画深色字会发虚发粗
    pub dark: bool,
}

/// 字的颜色比底色暗
pub fn darker(fg: u32, bg: u32) -> bool {
    fn luma(c: u32) -> u32 {
        ((c >> 16) & 0xff) * 299 + ((c >> 8) & 0xff) * 587 + (c & 0xff) * 114
    }
    luma(fg) < luma(bg)
}

/// 图集里的一个位置（像素）
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Slot {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
    /// 彩色字形（emoji）：按原色画
    pub color: bool,
    /// 空白（这个字在字体里什么都没画）：不用出实例
    pub empty: bool,
}

/// 字形从哪来：浏览器里是画布 + GPU 图集（atlas.rs），测试里是假的
pub trait Glyphs {
    /// None：图集满了（这一帧先不画，清空图集下一帧重来）
    fn glyph(&mut self, key: &GlyphKey) -> Option<Slot>;
}

/// 格子尺寸（设备像素，都是整数）
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Metrics {
    pub cw: u32,
    pub ch: u32,
    pub pad_x: u32,
    pub pad_y: u32,
    /// 字的基线（离格子顶）
    pub baseline: u32,
    pub underline_y: u32,
    pub strike_y: u32,
    pub thin: u32,
}

impl Default for Metrics {
    fn default() -> Self {
        Metrics { cw: 8, ch: 16, pad_x: 5, pad_y: 5, baseline: 12, underline_y: 14, strike_y: 9, thin: 1 }
    }
}

pub struct CursorDraw {
    /// 闪烁的“亮”的那一半（不闪时一直是 true）
    pub on: bool,
    pub focused: bool,
}

pub struct Scene<'a> {
    pub client: &'a TermClient,
    /// 最上面那一行的行号（可以是小数：逐像素平滑滚动）
    pub top: f64,
    pub metrics: Metrics,
    pub theme: &'a Theme,
    pub selection: Option<&'a Selection>,
    pub search: &'a Search,
    /// 鼠标停在链接上：(行号, 起列, 止列) 画下划线
    pub hover: Option<(u64, u32, u32)>,
    pub cursor: CursorDraw,
}

fn rect(x: f32, y: f32, w: f32, h: f32, color: [f32; 4]) -> Instance {
    Instance { rect: [x, y, w, h], uv: [0.0, 0.0], kind: KIND_RECT, pad: 0, color }
}

fn glyph_inst(x: f32, y: f32, slot: Slot, color: [f32; 4]) -> Instance {
    Instance {
        rect: [x, y, slot.w as f32, slot.h as f32],
        uv: [slot.x as f32, slot.y as f32],
        kind: if slot.color { KIND_COLOR } else { KIND_MASK },
        pad: 0,
        color: if slot.color { [1.0, 1.0, 1.0, color[3]] } else { color },
    }
}

fn blank(c: char) -> bool {
    c == ' ' || c == '\0' || c == '\t'
}

pub struct Built {
    pub instances: Vec<Instance>,
    /// 有字形没放进图集（满了）：清空图集再画一遍
    pub atlas_full: bool,
}

impl Scene<'_> {
    pub fn build(&self, glyphs: &mut dyn Glyphs) -> Built {
        let mut bgs: Vec<Instance> = Vec::new();
        let mut marks: Vec<Instance> = Vec::new();
        let mut fgs: Vec<Instance> = Vec::new();
        let mut tops: Vec<Instance> = Vec::new();
        let mut atlas_full = false;
        let Some(s) = self.client.screen() else { return Built { instances: Vec::new(), atlas_full } };
        let m = self.metrics;
        let theme = self.theme.with_overrides(&s.palette_overrides);
        let (cw, ch) = (m.cw as f32, m.ch as f32);
        let cols = s.cols as u32;
        let base = self.client.screen_base();
        let top = self.top.max(0.0);
        let first = top.floor() as u64;
        let y_off = -((top - top.floor()) * m.ch as f64).round() as f32;
        let n = s.rows as u64 + u64::from(y_off != 0.0);
        let sel_spans = self.selection.map(|sel| sel.spans(cols)).unwrap_or_default();
        let sel_color = argb(theme.get(SELECTION));
        let thin = m.thin as f32;

        for i in 0..n {
            let abs = first + i;
            let y = m.pad_y as f32 + i as f32 * ch + y_off;
            // 选区、搜索命中
            for &(l, a, b) in &sel_spans {
                if l == abs {
                    marks.push(rect(m.pad_x as f32 + a as f32 * cw, y, (b - a) as f32 * cw, ch, sel_color));
                }
            }
            let ms = &self.search.matches;
            let lo = ms.partition_point(|x| x.line < abs);
            for (k, mt) in ms[lo..].iter().enumerate().take_while(|(_, x)| x.line == abs) {
                let current = self.search.current == Some(lo + k);
                let c = argb(theme.get(if current { SEARCH_CURRENT } else { SEARCH }));
                marks.push(rect(m.pad_x as f32 + mt.from as f32 * cw, y, (mt.to - mt.from) as f32 * cw, ch, c));
            }
            let Some(line) = self.client.line(abs) else { continue };
            // 背景：连着的同色格子并成一块
            let mut run: Option<(u32, u32, u32)> = None; // (起列, 止列, 颜色)
            let flush = |run: &mut Option<(u32, u32, u32)>, bgs: &mut Vec<Instance>| {
                if let Some((a, b, c)) = run.take() {
                    bgs.push(rect(m.pad_x as f32 + a as f32 * cw, y, (b - a) as f32 * cw, ch, argb(c)));
                }
            };
            for col in 0..line.len().min(cols as usize) {
                let Some(cell) = line.cell(col) else { break };
                if cell.width == 0 {
                    continue;
                }
                let colors = resolve(&theme, cell.style);
                let w = cell.width as u32;
                let col = col as u32;
                match (colors.bg, run) {
                    (Some(c), Some((a, b, rc))) if rc == c && b == col => run = Some((a, col + w, rc)),
                    (Some(c), _) => {
                        flush(&mut run, &mut bgs);
                        run = Some((col, col + w, c));
                    }
                    (None, _) => flush(&mut run, &mut bgs),
                }
                let x = m.pad_x as f32 + col as f32 * cw;
                let fg = argb(colors.fg);
                let flags = cell.style.flags;
                if !colors.hidden && !blank(cell.c) {
                    let mut text = String::with_capacity(4);
                    text.push(cell.c);
                    text.push_str(cell.extra);
                    let key = GlyphKey {
                        text,
                        cells: cell.width,
                        bold: flags.contains(StyleFlags::BOLD),
                        italic: flags.contains(StyleFlags::ITALIC),
                        curl: false,
                        dark: darker(colors.fg, colors.bg.unwrap_or(theme.get(BG))),
                    };
                    match glyphs.glyph(&key) {
                        Some(slot) if !slot.empty => fgs.push(glyph_inst(x, y, slot, fg)),
                        Some(_) => {}
                        None => atlas_full = true,
                    }
                }
                // 装饰线
                let wpx = w as f32 * cw;
                let ul = argb(colors.underline);
                let uy = y + m.underline_y as f32;
                let hovered = self.hover.is_some_and(|(l, a, b)| l == abs && col >= a && col < b);
                if flags.contains(StyleFlags::UNDERLINE) || hovered {
                    fgs.push(rect(x, uy, wpx, thin, ul));
                }
                if flags.contains(StyleFlags::DOUBLE_UNDERLINE) {
                    fgs.push(rect(x, uy, wpx, thin, ul));
                    fgs.push(rect(x, (uy - thin * 2.0).max(y), wpx, thin, ul));
                }
                if flags.intersects(StyleFlags::DOTTED_UNDERLINE | StyleFlags::DASHED_UNDERLINE) {
                    let seg = if flags.contains(StyleFlags::DOTTED_UNDERLINE) { thin } else { (cw / 2.0 - thin).floor().max(thin) };
                    let mut sx = 0.0;
                    while sx < wpx {
                        fgs.push(rect(x + sx, uy, seg.min(wpx - sx), thin, ul));
                        sx += seg * 2.0;
                    }
                }
                if flags.contains(StyleFlags::UNDERCURL) {
                    for k in 0..w {
                        let key = GlyphKey {
                            text: String::new(),
                            cells: 1,
                            bold: false,
                            italic: false,
                            curl: true,
                            dark: darker(colors.underline, colors.bg.unwrap_or(theme.get(BG))),
                        };
                        match glyphs.glyph(&key) {
                            Some(slot) if !slot.empty => fgs.push(glyph_inst(x + k as f32 * cw, y, slot, ul)),
                            Some(_) => {}
                            None => atlas_full = true,
                        }
                    }
                }
                if flags.contains(StyleFlags::STRIKEOUT) {
                    fgs.push(rect(x, y + m.strike_y as f32, wpx, thin, fg));
                }
            }
            flush(&mut run, &mut bgs);
        }

        // 本地预测回显（§5.7）：盖住那一格，画默认颜色的字加一条淡下划线
        let preds = self.client.predicted();
        if !preds.is_empty() {
            let fg = theme.get(FG);
            let bg = argb(theme.get(BG) | 0xff00_0000);
            let faint = argb(mix(fg, theme.get(BG), 0.5));
            for p in preds {
                let abs = base + p.row as u64;
                if abs < first || abs >= first + n {
                    continue;
                }
                let x = m.pad_x as f32 + p.col as f32 * cw;
                let y = m.pad_y as f32 + (abs - first) as f32 * ch + y_off;
                tops.push(rect(x, y, cw, ch, bg));
                let key = GlyphKey {
                    text: p.c.to_string(),
                    cells: 1,
                    bold: false,
                    italic: false,
                    curl: false,
                    dark: darker(fg, theme.get(BG)),
                };
                match glyphs.glyph(&key) {
                    Some(slot) if !slot.empty => tops.push(glyph_inst(x, y, slot, argb(fg))),
                    Some(_) => {}
                    None => atlas_full = true,
                }
                tops.push(rect(x, y + m.underline_y as f32, cw, thin, faint));
            }
        }

        // 光标（有预测时画在最后一个预测后面）
        let mut c = s.cursor;
        if let Some(p) = preds.last() {
            c.row = p.row;
            c.col = p.col + 1;
        }
        let c = &c;
        let visible = c.visible && s.modes.show_cursor && c.shape != CursorShape::Hidden;
        let abs = base + c.row as u64;
        if visible && abs >= first && abs < first + n && (self.cursor.on || !self.cursor.focused) {
            let y = m.pad_y as f32 + (abs - first) as f32 * ch + y_off;
            let x = m.pad_x as f32 + c.col as f32 * cw;
            let line = self.client.line(abs);
            let cell = line.and_then(|l| l.cell(c.col as usize));
            let cells = cell.map_or(1, |c| c.width.max(1)) as f32;
            let w = cells * cw;
            let cur = argb(theme.get(CURSOR));
            let shape = if self.cursor.focused { c.shape } else { CursorShape::HollowBlock };
            match shape {
                CursorShape::Block => {
                    tops.push(rect(x, y, w, ch, cur));
                    if let Some(cell) = cell.filter(|cell| !blank(cell.c)) {
                        let tc = theme.get(CURSOR_TEXT);
                        let tc = if tc >> 24 == 0 { theme.get(BG) } else { tc };
                        let key = GlyphKey {
                            text: format!("{}{}", cell.c, cell.extra),
                            cells: cell.width.max(1),
                            bold: cell.style.flags.contains(StyleFlags::BOLD),
                            italic: cell.style.flags.contains(StyleFlags::ITALIC),
                            curl: false,
                            dark: darker(tc, theme.get(CURSOR)),
                        };
                        if let Some(slot) = glyphs.glyph(&key).filter(|s| !s.empty) {
                            tops.push(glyph_inst(x, y, slot, argb(tc | 0xff00_0000)));
                        }
                    }
                }
                CursorShape::HollowBlock => {
                    tops.push(rect(x, y, w, thin, cur));
                    tops.push(rect(x, y + ch - thin, w, thin, cur));
                    tops.push(rect(x, y, thin, ch, cur));
                    tops.push(rect(x + w - thin, y, thin, ch, cur));
                }
                CursorShape::Beam => tops.push(rect(x, y, (thin * 2.0).max(2.0), ch, cur)),
                CursorShape::Underline => tops.push(rect(x, y + ch - thin * 2.0, w, thin * 2.0, cur)),
                CursorShape::Hidden => {}
            }
        }

        let mut instances = bgs;
        instances.append(&mut marks);
        instances.append(&mut fgs);
        instances.append(&mut tops);
        Built { instances, atlas_full }
    }
}
