//! 给 JS 的接口（src-term/gpu/）。坐标、尺寸都是画布的设备像素。

use wasm_bindgen::prelude::*;
use web_sys::HtmlCanvasElement;

use term_web::TermClient;

use crate::atlas::Atlas;
use crate::gpu::Gpu;
use crate::select::Mode;
use crate::state::State;
use crate::theme::{argb, Theme, BG};

#[wasm_bindgen]
pub struct GpuTerm {
    gpu: Gpu,
    atlas: Atlas,
    state: State,
    size: (u32, u32),
}

#[wasm_bindgen]
impl GpuTerm {
    /// `force_gl`：不试 WebGPU，直接用 WebGL2（地址上 ?gpu=gl，排查问题用）
    pub async fn create(canvas: HtmlCanvasElement, force_gl: bool) -> Result<GpuTerm, JsValue> {
        let size = (canvas.width(), canvas.height());
        let gpu = Gpu::new(canvas, force_gl).await?;
        Ok(GpuTerm { gpu, atlas: Atlas::new()?, state: State::default(), size })
    }

    /// "webgpu" 或 "webgl2"
    pub fn backend(&self) -> String {
        self.gpu.backend.to_string()
    }

    /// 字体（CSS font-family）、字号（设备像素）、行高倍数、四周留白（设备像素）。返回格子 [宽, 高]
    pub fn set_font(&mut self, family: &str, font_px: f64, line_height: f64, pad: u32) -> Vec<u32> {
        let m = self.atlas.set_font(family, font_px, line_height, pad);
        self.state.metrics = m;
        vec![m.cw, m.ch]
    }

    /// 配色表（见 theme.rs：0..256 调色板，后面是前景、背景、光标、光标上的字、选区、搜索命中、当前命中；0xAARRGGBB）
    pub fn set_theme(&mut self, table: &[u32]) {
        self.state.theme = Theme::from_table(table);
    }

    pub fn resize(&mut self, w: u32, h: u32) {
        self.size = (w, h);
        self.gpu.resize(w, h);
    }

    /// 这么大的画布能放几行几列
    pub fn grid(&self, w: u32, h: u32) -> Vec<u32> {
        let m = self.state.metrics;
        let cols = (w.saturating_sub(m.pad_x * 2) / m.cw.max(1)).max(2);
        let rows = (h.saturating_sub(m.pad_y * 2) / m.ch.max(1)).max(1);
        vec![cols, rows]
    }

    /// 画一帧。`cursor_on`：闪烁光标亮的那一半；`focused`：没聚焦时画空心光标
    pub fn render(&mut self, tc: &TermClient, cursor_on: bool, focused: bool) {
        let mut built = self.state.build(tc, cursor_on, focused, &mut self.atlas);
        if built.atlas_full {
            // 图集满了（字太多，或者换过好几次字号）：清空重来
            self.atlas.clear();
            built = self.state.build(tc, cursor_on, focused, &mut self.atlas);
        }
        self.gpu.upload(&self.atlas.uploads);
        self.atlas.uploads.clear();
        let m = self.state.metrics;
        let rows = tc.screen().map_or(0, |s| s.rows) as u32;
        let clip = [0, m.pad_y, self.size.0, rows * m.ch];
        self.gpu.draw(&built.instances, argb(self.state.theme.get(BG)), clip);
    }

    pub fn scroll_lines(&mut self, tc: &TermClient, lines: f64) {
        self.state.scroll_lines(tc, lines);
    }

    /// 触控板 / 滚轮逐像素滚
    pub fn scroll_px(&mut self, tc: &TermClient, px: f64) {
        let ch = self.state.metrics.ch.max(1) as f64;
        self.state.scroll_lines(tc, px / ch);
    }

    pub fn scroll_to_bottom(&mut self, tc: &TermClient) {
        self.state.scroll_to_bottom(tc);
    }

    pub fn pinned(&self) -> bool {
        self.state.pinned
    }

    /// 最上面一行的行号（小数）
    pub fn top(&self) -> f64 {
        self.state.top
    }

    /// 点 → [行号, 列, 屏幕上第几行（回滚区里是负数）]
    pub fn cell_at(&self, tc: &TermClient, x: f64, y: f64) -> Vec<f64> {
        let (p, row) = self.state.point_at(tc, x, y, false);
        vec![p.line as f64, p.col as f64, row as f64]
    }

    /// `mode` 0 按字 1 选词 2 选行；`block`：Alt 拖动按列选
    pub fn select_start(&mut self, tc: &TermClient, x: f64, y: f64, mode: u8, block: bool) {
        let mode = match mode {
            1 => Mode::Word,
            2 => Mode::Line,
            _ => Mode::Char,
        };
        self.state.select_start(tc, x, y, mode, block);
    }

    pub fn select_extend(&mut self, tc: &TermClient, x: f64, y: f64) {
        self.state.select_extend(tc, x, y);
    }

    pub fn select_all(&mut self, tc: &TermClient) {
        self.state.select_all(tc);
    }

    pub fn select_clear(&mut self) {
        self.state.selection = None;
    }

    pub fn has_selection(&self) -> bool {
        self.state.selection.as_ref().is_some_and(|s| !s.is_empty())
    }

    pub fn selection_text(&self, tc: &TermClient) -> String {
        self.state.selection_text(tc)
    }

    /// 搜索并跳到下一个（`older`：往上）。返回 [命中数, 当前是第几个（从 1 数，没有为 0）]
    pub fn search(&mut self, tc: &TermClient, q: &str, case_sensitive: bool, older: bool) -> Vec<f64> {
        let n = self.state.search(tc, q, case_sensitive, older);
        vec![n as f64, self.state.search.current.map_or(0.0, |i| i as f64 + 1.0)]
    }

    pub fn search_clear(&mut self) {
        self.state.search.clear();
    }

    /// 鼠标停在链接上：返回地址（同时画下划线）
    pub fn hover(&mut self, tc: &TermClient, x: f64, y: f64) -> Option<String> {
        self.state.hover_at(tc, x, y)
    }

    pub fn hover_clear(&mut self) {
        self.state.hover = None;
    }

    /// 光标格子左上角 [x, y] 和格子 [宽, 高]（输入框跟着它走，输入法候选框出在这里）
    pub fn cursor_rect(&self, tc: &TermClient) -> Vec<f64> {
        let m = self.state.metrics;
        let Some(s) = tc.screen() else { return vec![0.0, 0.0, m.cw as f64, m.ch as f64] };
        let top = if self.state.pinned { tc.screen_base() as f64 } else { self.state.top };
        // 有本地预测时光标在最后一个预测后面
        let (row, col) = tc.predicted().last().map_or((s.cursor.row, s.cursor.col), |p| (p.row, p.col + 1));
        let line = (tc.screen_base() + row as u64) as f64;
        vec![
            m.pad_x as f64 + col as f64 * m.cw as f64,
            m.pad_y as f64 + (line - top) * m.ch as f64,
            m.cw as f64,
            m.ch as f64,
        ]
    }
}
