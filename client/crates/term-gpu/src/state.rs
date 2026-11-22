//! 不碰 GPU 的那部分状态：滚动位置、选区、搜索、链接悬停、格子尺寸和配色。能在本机跑测试。

use term_web::TermClient;

use crate::scene::{Built, CursorDraw, Glyphs, Metrics, Scene};
use crate::select::{link_at, Lines, Mode, Point, Search, Selection};
use crate::theme::Theme;

pub struct State {
    pub metrics: Metrics,
    pub theme: Theme,
    /// 最上面那一行的行号（小数：逐像素滚动）
    pub top: f64,
    /// 停在最下面：有新输出跟着走
    pub pinned: bool,
    pub selection: Option<Selection>,
    pub search: Search,
    pub hover: Option<(u64, u32, u32)>,
    /// 上一帧的备用屏幕状态（切换时清掉选区、回到最下面）
    alt: bool,
}

impl Default for State {
    fn default() -> Self {
        State {
            metrics: Metrics::default(),
            theme: Theme::default(),
            top: 0.0,
            pinned: true,
            selection: None,
            search: Search::default(),
            hover: None,
            alt: false,
        }
    }
}

impl State {
    /// 能滚到的范围：[termd 还留着的最早一行, 屏幕第 0 行]。备用屏幕里没有回滚区
    pub fn bounds(&self, tc: &TermClient) -> (f64, f64) {
        let base = tc.screen_base() as f64;
        let alt = tc.screen().is_some_and(|s| s.modes.alt_screen);
        match tc.history() {
            Some(h) if !alt => ((h.oldest.min(tc.screen_base())) as f64, base),
            _ => (base, base),
        }
    }

    /// 每帧画之前：跟上新输出 / 夹在范围里
    pub fn sync(&mut self, tc: &TermClient) {
        let alt = tc.screen().is_some_and(|s| s.modes.alt_screen);
        if alt != self.alt {
            self.alt = alt;
            self.selection = None;
            self.pinned = true;
        }
        let (lo, hi) = self.bounds(tc);
        if self.pinned {
            self.top = hi;
        } else {
            self.top = self.top.clamp(lo, hi);
            if self.top >= hi {
                self.pinned = true;
            }
        }
    }

    /// 往下滚 `lines` 行（负数往上）
    pub fn scroll_lines(&mut self, tc: &TermClient, lines: f64) {
        let (lo, hi) = self.bounds(tc);
        if self.pinned {
            self.top = hi;
        }
        self.top = (self.top + lines).clamp(lo, hi);
        self.pinned = self.top >= hi - 1e-6;
        if self.pinned {
            self.top = hi;
        }
    }

    pub fn scroll_to_bottom(&mut self, tc: &TermClient) {
        self.pinned = true;
        self.sync(tc);
    }

    /// 把某一行滚到看得见（搜索跳转）：放在屏幕中间偏上
    pub fn reveal(&mut self, tc: &TermClient, line: u64) {
        let rows = tc.screen().map_or(1, |s| s.rows) as f64;
        let (lo, hi) = self.bounds(tc);
        let cur = if self.pinned { hi } else { self.top };
        let l = line as f64;
        if l >= cur && l < cur + rows {
            return;
        }
        self.top = (l - (rows / 3.0).floor()).clamp(lo, hi);
        self.pinned = self.top >= hi;
    }

    /// 画布上的点（设备像素）→ 格子：(行号, 列, 屏幕上的第几行)。`round`：选字时取最近的格子边界
    pub fn point_at(&self, tc: &TermClient, x: f64, y: f64, round: bool) -> (Point, i64) {
        let m = self.metrics;
        let cols = tc.screen().map_or(1, |s| s.cols) as f64;
        let fx = (x - m.pad_x as f64) / m.cw as f64;
        let col = if round { fx.round() } else { fx.floor() }.clamp(0.0, if round { cols } else { cols - 1.0 });
        let top = if self.pinned { self.bounds(tc).1 } else { self.top };
        let row_f = top + (y - m.pad_y as f64) / m.ch as f64;
        let line = row_f.floor().max(0.0) as u64;
        let screen_row = line as i64 - tc.screen_base() as i64;
        (Point { line, col: col as u32 }, screen_row)
    }

    pub fn select_start(&mut self, tc: &TermClient, x: f64, y: f64, mode: Mode, block: bool) {
        let (p, _) = self.point_at(tc, x, y, mode == Mode::Char);
        self.selection = Some(Selection::new(tc, p, mode, block));
    }

    pub fn select_extend(&mut self, tc: &TermClient, x: f64, y: f64) {
        let Some(mode) = self.selection.as_ref().map(|s| s.mode) else { return };
        let round = mode == Mode::Char;
        let m = self.metrics;
        let cols = tc.screen().map_or(1, |s| s.cols) as f64;
        let fx = (x - m.pad_x as f64) / m.cw as f64;
        let col = if round { fx.round() } else { fx.floor() }.clamp(0.0, cols);
        let top = if self.pinned { self.bounds(tc).1 } else { self.top };
        let line = (top + (y - m.pad_y as f64) / m.ch as f64).floor().max(0.0) as u64;
        if let Some(sel) = self.selection.as_mut() {
            sel.extend(tc, Point { line, col: col as u32 });
        }
    }

    pub fn selection_text(&self, tc: &TermClient) -> String {
        self.selection.as_ref().map(|s| s.text(tc)).unwrap_or_default()
    }

    pub fn select_all(&mut self, tc: &TermClient) {
        let first = Lines::first(tc);
        let last = tc.screen_base() + tc.screen().map_or(0, |s| s.rows) as u64;
        let a = Point { line: first, col: 0 };
        let b = Point { line: last.saturating_sub(1), col: u32::MAX };
        self.selection = Some(Selection { mode: Mode::Char, block: false, anchor: (a, a), head: (b, b) });
    }

    /// 搜索：返回命中数，跳到当前这个
    pub fn search(&mut self, tc: &TermClient, q: &str, case_sensitive: bool, older: bool) -> usize {
        let fresh = q != self.search.query || case_sensitive != self.search.case_sensitive;
        // 每次都重搜一遍（中间可能有新输出），能停在原来那个上就接着往下走
        self.search.run(tc, q, case_sensitive);
        let m = if fresh || self.search.current.is_none() {
            // 新的搜索：从最下面（最近的输出）那个开始
            self.search.current = None;
            self.search.step(true)
        } else {
            self.search.step(older)
        };
        if let Some(m) = m {
            self.reveal(tc, m.line);
        }
        self.search.matches.len()
    }

    /// 鼠标停在 (x, y)：在链接上就记下来（画下划线），返回地址
    pub fn hover_at(&mut self, tc: &TermClient, x: f64, y: f64) -> Option<String> {
        let (p, _) = self.point_at(tc, x, y, false);
        match link_at(tc, p.line, p.col) {
            Some((uri, a, b)) => {
                self.hover = Some((p.line, a, b));
                Some(uri)
            }
            None => {
                self.hover = None;
                None
            }
        }
    }

    pub fn build(&mut self, tc: &TermClient, cursor_on: bool, focused: bool, glyphs: &mut dyn Glyphs) -> Built {
        self.sync(tc);
        let scene = Scene {
            client: tc,
            top: self.top,
            metrics: self.metrics,
            theme: &self.theme,
            selection: self.selection.as_ref().filter(|s| !s.is_empty()),
            search: &self.search,
            hover: self.hover,
            cursor: CursorDraw { on: cursor_on, focused },
        };
        scene.build(glyphs)
    }
}
