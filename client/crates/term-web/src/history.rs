//! 回滚区留在 WASM 里（电脑 wgpu 渲染用，设计稿 §7）：termd 推来 / 拉来的行直接存在这里，渲染、选字、搜索都在 Rust 里读，
//! 不再编成 JSON 交给 JS。手机 DOM 渲染不开这个（行变成网页文字后就不需要再留）。
//!
//! 一行按格子紧凑存放（每格 8 字节，组合字符另放），2 万行常见内容只占几 MB。
//! 行号就是 termd 的回滚序号 `seq`；屏幕第 `r` 行的行号是 `scrollback_next_seq + r`，两边连成一条，
//! 选区、搜索结果在滚动、新输出时不会错位。

use std::collections::VecDeque;

use term_proto::{CellData, CellSpacer, HyperlinkData, ScreenRow, ScrollbackRow, Style, StyleTable};

/// 浏览器里最多留这么多回滚行（更早的不留，往上翻到时再找 termd 要）
pub const MAX_ROWS: usize = 50_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PackedCell {
    pub c: char,
    pub style: u16,
    /// 低 2 位：宽度（1 / 2；0 表示宽字符的后半格）；第 2 位：有组合字符（在 `extras` 里）
    pub bits: u8,
}

const BIT_EXTRA: u8 = 4;

impl PackedCell {
    pub fn width(&self) -> u8 {
        self.bits & 3
    }
}

#[derive(Clone, Debug, Default)]
pub struct PackedRow {
    pub cells: Vec<PackedCell>,
    pub extras: Vec<(u16, String)>,
    pub styles: Vec<Style>,
    pub links: Vec<HyperlinkData>,
    pub wrapped: bool,
}

impl PackedRow {
    pub fn from_scrollback(row: &ScrollbackRow) -> Self {
        let end = row
            .cells
            .iter()
            .rposition(|c| !c.is_blank_default() && c.spacer != CellSpacer::WideLeading)
            .map_or(0, |i| i + 1);
        let mut out = PackedRow {
            cells: Vec::with_capacity(end),
            extras: Vec::new(),
            styles: row.styles.clone(),
            links: row.hyperlinks.clone(),
            wrapped: row.wrapped,
        };
        for (i, cell) in row.cells.iter().take(end).enumerate() {
            let mut bits = match cell.spacer {
                CellSpacer::WideTrailing => 0,
                _ if cell.width >= 2 => 2,
                _ => 1,
            };
            if !cell.extra.is_empty() {
                bits |= BIT_EXTRA;
                out.extras.push((i as u16, cell.extra.clone()));
            }
            out.cells.push(PackedCell { c: cell.c, style: cell.style_id.min(u16::MAX as u32) as u16, bits });
        }
        out
    }
}

/// 读一格时看到的样子（屏幕行和回滚行同一个接口）
#[derive(Clone, Copy, Debug)]
pub struct CellRef<'a> {
    pub c: char,
    pub extra: &'a str,
    /// 1 / 2；0 = 宽字符的后半格（不画）
    pub width: u8,
    pub style: &'a Style,
}

static DEFAULT_STYLE: std::sync::LazyLock<Style> = std::sync::LazyLock::new(Style::default);

/// 一行：屏幕上的，或回滚区里的
#[derive(Clone, Copy)]
pub enum LineRef<'a> {
    Screen(&'a ScreenRow, &'a StyleTable),
    Packed(&'a PackedRow),
}

impl<'a> LineRef<'a> {
    pub fn len(&self) -> usize {
        match self {
            LineRef::Screen(r, _) => r.cells.len(),
            LineRef::Packed(r) => r.cells.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn wrapped(&self) -> bool {
        match self {
            LineRef::Screen(r, _) => r.wrapped,
            LineRef::Packed(r) => r.wrapped,
        }
    }

    pub fn cell(&self, i: usize) -> Option<CellRef<'a>> {
        match *self {
            LineRef::Screen(r, t) => {
                let c: &'a CellData = r.cells.get(i)?;
                let width = match c.spacer {
                    CellSpacer::WideTrailing => 0,
                    _ if c.width >= 2 => 2,
                    _ => 1,
                };
                Some(CellRef {
                    c: c.c,
                    extra: &c.extra,
                    width,
                    style: t.styles.get(c.style_id as usize).unwrap_or(&DEFAULT_STYLE),
                })
            }
            LineRef::Packed(r) => {
                let c = r.cells.get(i)?;
                let extra = if c.bits & BIT_EXTRA != 0 {
                    r.extras.iter().find(|(k, _)| *k as usize == i).map_or("", |(_, s)| s.as_str())
                } else {
                    ""
                };
                Some(CellRef {
                    c: c.c,
                    extra,
                    width: c.width(),
                    style: r.styles.get(c.style as usize).unwrap_or(&DEFAULT_STYLE),
                })
            }
        }
    }

    pub fn link(&self, style: &Style) -> Option<&'a str> {
        let id = style.hyperlink_id? as usize;
        match self {
            LineRef::Screen(_, t) => t.hyperlinks.get(id).map(|l| l.uri.as_str()),
            LineRef::Packed(r) => r.links.get(id).map(|l| l.uri.as_str()),
        }
    }

    /// 格子对应的文字（不画的后半格是空串，空格子是空格）
    pub fn text(&self) -> String {
        let mut s = String::new();
        for i in 0..self.len() {
            let Some(c) = self.cell(i) else { break };
            if c.width == 0 {
                continue;
            }
            s.push(if c.c == '\0' || c.c == '\t' { ' ' } else { c.c });
            s.push_str(c.extra);
        }
        s
    }
}

/// 回滚行：`rows[i]` 是序号 `base + i` 的那一行（没拿到的是 None）
#[derive(Default)]
pub struct History {
    pub epoch: u64,
    pub base: u64,
    pub rows: VecDeque<Option<PackedRow>>,
    /// termd 那边还留着的最早一行（再早的已经丢了，不用去拿）
    pub oldest: u64,
    /// termd 那边下一行的序号（= 屏幕第 0 行的行号）
    pub next: u64,
}

impl History {
    pub fn reset(&mut self, epoch: u64) {
        *self = History { epoch, ..Default::default() };
    }

    pub fn get(&self, seq: u64) -> Option<&PackedRow> {
        if seq < self.base {
            return None;
        }
        self.rows.get((seq - self.base) as usize)?.as_ref()
    }

    pub fn has(&self, seq: u64) -> bool {
        self.get(seq).is_some()
    }

    /// termd 的回滚区信息（每帧画面里都有）：更早的已经被它丢掉的不用留
    pub fn set_bounds(&mut self, epoch: u64, oldest: u64, next: u64) {
        if epoch != self.epoch {
            self.reset(epoch);
        }
        self.oldest = oldest;
        self.next = next;
        while self.base < oldest && !self.rows.is_empty() {
            self.rows.pop_front();
            self.base += 1;
        }
        if self.rows.is_empty() && self.base < oldest {
            self.base = oldest;
        }
    }

    pub fn insert(&mut self, epoch: u64, row: &ScrollbackRow) {
        if epoch != self.epoch {
            self.reset(epoch);
        }
        let seq = row.seq;
        if self.rows.is_empty() {
            self.base = seq;
        }
        if seq < self.base {
            // 往前补：太远（中间隔了很多没拿的）就不要了
            let gap = (self.base - seq) as usize;
            if gap + self.rows.len() > MAX_ROWS {
                return;
            }
            for _ in 0..gap {
                self.rows.push_front(None);
            }
            self.base = seq;
        }
        let i = (seq - self.base) as usize;
        if i >= self.rows.len() {
            if i - self.rows.len() > MAX_ROWS {
                // 跳得太远：前面的全扔掉
                self.rows.clear();
                self.base = seq;
            } else {
                self.rows.resize(i, None);
            }
        }
        let i = (seq - self.base) as usize;
        if i == self.rows.len() {
            self.rows.push_back(Some(PackedRow::from_scrollback(row)));
        } else {
            self.rows[i] = Some(PackedRow::from_scrollback(row));
        }
        while self.rows.len() > MAX_ROWS {
            self.rows.pop_front();
            self.base += 1;
        }
        self.next = self.next.max(seq + 1);
    }

    /// 最早一行本地有的 `seq`（再往上就要去 termd 拿）
    pub fn first_local(&self) -> u64 {
        self.rows.iter().position(|r| r.is_some()).map_or(self.next, |i| self.base + i as u64)
    }

    /// [from, to) 里第一段没拿到的行：返回 (起点, 行数)，只在 termd 还留着的范围里找
    pub fn missing(&self, from: u64, to: u64) -> Option<(u64, u64)> {
        let from = from.max(self.oldest);
        let to = to.min(self.next);
        let mut s = from;
        while s < to && self.has(s) {
            s += 1;
        }
        if s >= to {
            return None;
        }
        let mut e = s;
        while e < to && !self.has(e) {
            e += 1;
        }
        Some((s, e - s))
    }
}
