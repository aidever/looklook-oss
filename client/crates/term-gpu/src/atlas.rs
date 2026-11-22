//! 字形图集（设计稿 §7.2）：字交给浏览器光栅化——在一块小画布上用 Canvas2D 画（系统字体回退、中文、emoji 都有），
//! 按设备像素读出来放进图集，再上传 GPU。宽字符按两格宽画，不裁；比格子宽的字横向压进格子里，后面的列不受影响。
//! 框线、方块等自己画（boxdraw.rs）。

use std::collections::HashMap;

use wasm_bindgen::{JsCast, JsValue};
use web_sys::{CanvasRenderingContext2d, HtmlCanvasElement};

use crate::boxdraw::{self, PathOp, Prim};
use crate::scene::{GlyphKey, Glyphs, Metrics, Slot};

pub const ATLAS_SIZE: u32 = 2048;

pub struct Upload {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
    pub data: Vec<u8>,
}

pub struct Atlas {
    canvas: HtmlCanvasElement,
    ctx: CanvasRenderingContext2d,
    map: HashMap<GlyphKey, Slot>,
    pub uploads: Vec<Upload>,
    shelf_x: u32,
    shelf_y: u32,
    metrics: Metrics,
    family: String,
    font_px: f64,
}

fn context(canvas: &HtmlCanvasElement) -> Result<CanvasRenderingContext2d, JsValue> {
    let opts = js_sys::Object::new();
    js_sys::Reflect::set(&opts, &"willReadFrequently".into(), &JsValue::TRUE)?;
    canvas
        .get_context_with_context_options("2d", &opts)?
        .ok_or_else(|| JsValue::from_str("no 2d context"))?
        .dyn_into::<CanvasRenderingContext2d>()
        .map_err(|_| JsValue::from_str("not a 2d context"))
}

impl Atlas {
    pub fn new() -> Result<Atlas, JsValue> {
        let doc = web_sys::window().and_then(|w| w.document()).ok_or("no document")?;
        let canvas = doc.create_element("canvas")?.dyn_into::<HtmlCanvasElement>()?;
        canvas.set_width(64);
        canvas.set_height(32);
        let ctx = context(&canvas)?;
        Ok(Atlas {
            canvas,
            ctx,
            map: HashMap::new(),
            uploads: Vec::new(),
            shelf_x: 0,
            shelf_y: 0,
            metrics: Metrics::default(),
            family: "monospace".into(),
            font_px: 13.0,
        })
    }

    fn font(&self, bold: bool, italic: bool) -> String {
        format!(
            "{}{}{}px {}",
            if italic { "italic " } else { "" },
            if bold { "bold " } else { "" },
            self.font_px,
            self.family
        )
    }

    /// 换字体 / 字号（设备像素）/ 行高：量出格子尺寸，图集清空重来
    pub fn set_font(&mut self, family: &str, font_px: f64, line_height: f64, pad: u32) -> Metrics {
        self.family = family.to_string();
        self.font_px = font_px;
        self.ctx.set_font(&self.font(false, false));
        let probe: String = "W".repeat(50);
        let m = self.ctx.measure_text(&probe).ok();
        let adv = m.as_ref().map_or(font_px * 0.6 * 50.0, |m| m.width()) / 50.0;
        let (mut asc, mut desc) = m.as_ref().map_or((0.0, 0.0), |m| (m.font_bounding_box_ascent(), m.font_bounding_box_descent()));
        if asc.is_nan() || desc.is_nan() || asc <= 0.0 || desc < 0.0 {
            asc = font_px * 0.8;
            desc = font_px * 0.2;
        }
        let cw = adv.round().max(1.0) as u32;
        let ch = (font_px * line_height).round().max((asc + desc).ceil()).max(1.0) as u32;
        let baseline = (((ch as f64) - (asc + desc)) / 2.0 + asc).round() as u32;
        let thin = (font_px / 14.0).round().max(1.0) as u32;
        let underline_y = (baseline + ((desc * 0.4).round() as u32).max(1)).min(ch.saturating_sub(thin));
        let strike_y = baseline.saturating_sub((asc * 0.3).round() as u32);
        self.metrics = Metrics { cw, ch, pad_x: pad, pad_y: pad, baseline, underline_y, strike_y, thin };
        self.canvas.set_width(cw * 2 + 8);
        self.canvas.set_height(ch);
        // 画布尺寸变了，上下文的状态会被重置：重新拿一次
        if let Ok(ctx) = context(&self.canvas) {
            self.ctx = ctx;
        }
        self.clear();
        self.metrics
    }

    pub fn clear(&mut self) {
        self.map.clear();
        self.uploads.clear();
        self.shelf_x = 0;
        self.shelf_y = 0;
    }

    fn draw_prims(&self, prims: &[Prim]) {
        let ctx = &self.ctx;
        for p in prims {
            match p {
                Prim::Rect(x, y, w, h, a) => {
                    ctx.set_global_alpha(*a as f64);
                    ctx.fill_rect(*x as f64, *y as f64, *w as f64, *h as f64);
                    ctx.set_global_alpha(1.0);
                }
                Prim::Path { ops, fill, lw } => {
                    ctx.begin_path();
                    for op in ops {
                        match *op {
                            PathOp::Move(x, y) => ctx.move_to(x as f64, y as f64),
                            PathOp::Line(x, y) => ctx.line_to(x as f64, y as f64),
                            PathOp::Arc(cx, cy, r, a0, a1, ccw) => {
                                let _ = ctx.arc_with_anticlockwise(cx as f64, cy as f64, r as f64, a0 as f64, a1 as f64, ccw);
                            }
                            PathOp::Close => ctx.close_path(),
                        }
                    }
                    if *fill {
                        ctx.fill();
                    } else {
                        ctx.set_line_width(*lw as f64);
                        ctx.set_line_cap("butt");
                        ctx.stroke();
                    }
                }
            }
        }
    }

    fn rasterize(&self, key: &GlyphKey, w: u32, h: u32) -> Option<Vec<u8>> {
        let ctx = &self.ctx;
        ctx.clear_rect(0.0, 0.0, self.canvas.width() as f64, self.canvas.height() as f64);
        ctx.set_fill_style_str("#fff");
        ctx.set_stroke_style_str("#fff");
        let m = self.metrics;
        let mut chars = key.text.chars();
        let single = match (chars.next(), chars.next()) {
            (Some(c), None) => Some(c),
            _ => None,
        };
        if key.curl {
            self.draw_prims(&boxdraw::undercurl(w, h, m.underline_y as f32, m.thin as f32));
        } else if let Some(prims) = single.and_then(|c| boxdraw::prims(c, w, h)) {
            self.draw_prims(&prims);
        } else {
            ctx.set_font(&self.font(key.bold, key.italic));
            ctx.set_text_baseline("alphabetic");
            let measured = ctx.measure_text(&key.text).map(|t| t.width()).unwrap_or(w as f64);
            let wf = w as f64;
            if measured > wf * 1.05 {
                // 比格子宽（回退字体、宽图标）：横向压进格子
                ctx.save();
                let _ = ctx.scale(wf / measured, 1.0);
                let _ = ctx.fill_text(&key.text, 0.0, m.baseline as f64);
                ctx.restore();
            } else {
                let _ = ctx.fill_text(&key.text, ((wf - measured) / 2.0).max(0.0), m.baseline as f64);
            }
        }
        ctx.get_image_data(0.0, 0.0, w as f64, h as f64).ok().map(|d| d.data().0)
    }
}

impl Glyphs for Atlas {
    fn glyph(&mut self, key: &GlyphKey) -> Option<Slot> {
        if let Some(s) = self.map.get(key) {
            return Some(*s);
        }
        let m = self.metrics;
        let w = m.cw * key.cells.max(1) as u32;
        let h = m.ch;
        let data = self.rasterize(key, w, h)?;
        let mut empty = true;
        let mut color = false;
        for px in data.as_chunks::<4>().0 {
            if px[3] == 0 {
                continue;
            }
            empty = false;
            if px[3] > 160 {
                let (r, g, b) = (px[0] as i32, px[1] as i32, px[2] as i32);
                if (r - g).abs() > 24 || (g - b).abs() > 24 || r < 200 {
                    color = true;
                }
            }
        }
        if empty {
            let s = Slot { x: 0, y: 0, w, h, color: false, empty: true };
            self.map.insert(key.clone(), s);
            return Some(s);
        }
        if self.shelf_x + w > ATLAS_SIZE {
            self.shelf_x = 0;
            self.shelf_y += h;
        }
        if self.shelf_y + h > ATLAS_SIZE {
            return None;
        }
        let s = Slot { x: self.shelf_x, y: self.shelf_y, w, h, color, empty: false };
        self.shelf_x += w;
        self.map.insert(key.clone(), s);
        self.uploads.push(Upload { x: s.x, y: s.y, w, h, data });
        Some(s)
    }
}
