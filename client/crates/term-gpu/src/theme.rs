//! 配色：JS 端把主题算成一张颜色表交过来（`set_theme`），这里按格子的样式解出前景 / 背景色。
//! 规则和手机 DOM 渲染（src-term/dom/style.js）一致：粗体 + 前 8 色用亮色、反显互换、暗淡往背景色混一半、隐藏 = 背景色。

use term_proto::{ColorSpec, NamedColorCode, RgbColor, Style, StyleFlags};

/// 0xAARRGGBB → 线性无关的 0..1（终端颜色本来就是在 sRGB 里混的，和浏览器画文字一致，不转线性）
pub fn argb(c: u32) -> [f32; 4] {
    [
        ((c >> 16) & 0xff) as f32 / 255.0,
        ((c >> 8) & 0xff) as f32 / 255.0,
        (c & 0xff) as f32 / 255.0,
        ((c >> 24) & 0xff) as f32 / 255.0,
    ]
}

fn rgb(c: RgbColor) -> u32 {
    0xff00_0000 | (c.r as u32) << 16 | (c.g as u32) << 8 | c.b as u32
}

/// `set_theme` 收的表：0..256 调色板，后面依次是这些（都是 0xAARRGGBB）
pub const FG: usize = 256;
pub const BG: usize = 257;
pub const CURSOR: usize = 258;
pub const CURSOR_TEXT: usize = 259;
pub const SELECTION: usize = 260;
pub const SEARCH: usize = 261;
pub const SEARCH_CURRENT: usize = 262;
pub const THEME_LEN: usize = 263;

#[derive(Clone)]
pub struct Theme {
    pub colors: Vec<u32>,
}

/// xterm 的 256 色：前 16 色是 xterm.js 的默认值，后面是 6×6×6 色块和灰阶
pub fn default_palette() -> Vec<u32> {
    const BASE16: [u32; 16] = [
        0x2e3436, 0xcc0000, 0x4e9a06, 0xc4a000, 0x3465a4, 0x75507b, 0x06989a, 0xd3d7cf, 0x555753, 0xef2929, 0x8ae234,
        0xfce94f, 0x729fcf, 0xad7fa8, 0x34e2e2, 0xeeeeec,
    ];
    let mut out = Vec::with_capacity(THEME_LEN);
    out.extend(BASE16.iter().map(|c| 0xff00_0000 | c));
    for i in 0..216u32 {
        let c = |x: u32| if x == 0 { 0 } else { x * 40 + 55 };
        out.push(0xff00_0000 | c(i / 36) << 16 | c((i / 6) % 6) << 8 | c(i % 6));
    }
    for i in 0..24u32 {
        let g = i * 10 + 8;
        out.push(0xff00_0000 | g << 16 | g << 8 | g);
    }
    out.extend([0xffff_ffff, 0xff00_0000, 0xffff_ffff, 0xff00_0000, 0x7378_8cff, 0x99ff_d54a, 0xccff_9632]);
    out
}

impl Default for Theme {
    fn default() -> Self {
        Theme { colors: default_palette() }
    }
}

impl Theme {
    pub fn from_table(table: &[u32]) -> Theme {
        let mut colors = default_palette();
        for (i, c) in table.iter().take(THEME_LEN).enumerate() {
            colors[i] = *c;
        }
        Theme { colors }
    }

    pub fn get(&self, i: usize) -> u32 {
        self.colors.get(i).copied().unwrap_or(0xff00_0000)
    }

    /// 程序用 OSC 4 改过的颜色套上去（每帧画面里带着）
    pub fn with_overrides(&self, overrides: &[(u16, RgbColor)]) -> Theme {
        if overrides.is_empty() {
            return self.clone();
        }
        let mut t = self.clone();
        for (i, c) in overrides {
            if (*i as usize) < 256 {
                t.colors[*i as usize] = rgb(*c);
            }
        }
        t
    }
}

/// 一个格子最后的颜色（0xAARRGGBB）。`bg` 是 None：默认背景（不用画）
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CellColors {
    pub fg: u32,
    pub bg: Option<u32>,
    pub underline: u32,
    pub hidden: bool,
}

enum Resolved {
    Default,
    Color(u32),
}

fn spec(theme: &Theme, c: ColorSpec, bold: bool, dim: &mut bool) -> Resolved {
    match c {
        ColorSpec::Rgb(c) => Resolved::Color(rgb(c)),
        ColorSpec::Indexed(i) => {
            let i = if bold && i < 8 { i + 8 } else { i };
            Resolved::Color(theme.get(i as usize))
        }
        ColorSpec::Named(n) => {
            let v = n as u16;
            match n {
                NamedColorCode::Foreground | NamedColorCode::Background | NamedColorCode::Cursor | NamedColorCode::BrightForeground => {
                    Resolved::Default
                }
                NamedColorCode::DimForeground => {
                    *dim = true;
                    Resolved::Default
                }
                _ if v < 16 => {
                    let i = if bold && v < 8 { v + 8 } else { v };
                    Resolved::Color(theme.get(i as usize))
                }
                _ if (259..=266).contains(&v) => {
                    *dim = true;
                    Resolved::Color(theme.get((v - 259) as usize))
                }
                _ => Resolved::Default,
            }
        }
    }
}

/// 两色按比例混（暗淡、组字中的文字用）
pub fn mix(a: u32, b: u32, t: f32) -> u32 {
    let ch = |s: u32| {
        let x = ((a >> s) & 0xff) as f32;
        let y = ((b >> s) & 0xff) as f32;
        ((x + (y - x) * t).round() as u32) << s
    };
    0xff00_0000 | ch(16) | ch(8) | ch(0)
}

pub fn resolve(theme: &Theme, style: &Style) -> CellColors {
    let f = style.flags;
    let bold = f.contains(StyleFlags::BOLD);
    let mut dim = f.contains(StyleFlags::DIM);
    let default_fg = theme.get(FG);
    let default_bg = theme.get(BG);
    let fg = match spec(theme, style.fg, bold, &mut dim) {
        Resolved::Default => None,
        Resolved::Color(c) => Some(c),
    };
    let mut ignore = false;
    let bg = match spec(theme, style.bg, false, &mut ignore) {
        Resolved::Default => None,
        Resolved::Color(c) => Some(c),
    };
    let (mut fg, bg) = if f.contains(StyleFlags::INVERSE) {
        (bg.unwrap_or(default_bg), Some(fg.unwrap_or(default_fg)))
    } else {
        (fg.unwrap_or(default_fg), bg)
    };
    let back = bg.unwrap_or(default_bg);
    if dim {
        fg = mix(fg, back, 0.5);
    }
    let hidden = f.contains(StyleFlags::HIDDEN);
    let underline = match style.underline_color {
        Some(c) => match spec(theme, c, false, &mut ignore) {
            Resolved::Color(c) => c,
            Resolved::Default => fg,
        },
        None => fg,
    };
    CellColors { fg, bg, underline, hidden }
}
