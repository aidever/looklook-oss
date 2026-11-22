// 段的样式 → CSS 类。每种（前景, 背景, 属性）组合一个类，规则写进同一张样式表：
// 行的 HTML 里只有类名，换配色只重写样式表，不用重画任何一行。颜色编号见 crates/term-web/src/lib.rs。

const NAMES = ['black', 'red', 'green', 'yellow', 'blue', 'magenta', 'cyan', 'white',
  'brightBlack', 'brightRed', 'brightGreen', 'brightYellow', 'brightBlue', 'brightMagenta', 'brightCyan', 'brightWhite'];
// xterm.js 的默认 16 色（配色里没给时用）
const DEFAULT16 = ['#2e3436', '#cc0000', '#4e9a06', '#c4a000', '#3465a4', '#75507b', '#06989a', '#d3d7cf',
  '#555753', '#ef2929', '#8ae234', '#fce94f', '#729fcf', '#ad7fa8', '#34e2e2', '#eeeeec'];
const RGB_BASE = 0x1000000;

// StyleFlags（crates/term-proto/src/types.rs）
export const F = {
  BOLD: 1, DIM: 2, ITALIC: 4, UNDERLINE: 8, DOUBLE_UNDERLINE: 16, UNDERCURL: 32,
  DOTTED_UNDERLINE: 64, DASHED_UNDERLINE: 128, INVERSE: 256, HIDDEN: 512, STRIKEOUT: 1024,
};

function hex2(n) {
  return n.toString(16).padStart(2, '0');
}

/** 颜色编号 → CSS 颜色；-1（默认）返回 null。palette：程序用 OSC 4 改过的颜色 { 编号: 0x1RRGGBB } */
export function colorCss(code, theme, palette) {
  if (code < 0) return null;
  if (code >= RGB_BASE) return '#' + (code - RGB_BASE).toString(16).padStart(6, '0');
  if (palette && palette[code] !== undefined) return colorCss(palette[code], theme, null);
  if (code < 16) return (theme && theme[NAMES[code]]) || DEFAULT16[code];
  if (code < 232) {
    const v = code - 16;
    const c = (x) => (x ? x * 40 + 55 : 0);
    return '#' + hex2(c(Math.floor(v / 36))) + hex2(c(Math.floor(v / 6) % 6)) + hex2(c(v % 6));
  }
  const g = (code - 232) * 10 + 8;
  return '#' + hex2(g) + hex2(g) + hex2(g);
}

/** 一种样式的 CSS 声明 */
export function styleDecl(f, b, a, theme, palette) {
  let fg = colorCss(f, theme, palette);
  let bg = colorCss(b, theme, palette);
  // 粗体 + 前 8 色：和 xterm 一样用亮色
  if (a & F.BOLD && f >= 0 && f < 8) fg = colorCss(f + 8, theme, palette);
  if (a & F.INVERSE) {
    const dfg = (theme && theme.foreground) || '#ffffff';
    const dbg = (theme && theme.background) || '#000000';
    [fg, bg] = [bg || dbg, fg || dfg];
  }
  let css = '';
  if (fg) css += 'color:' + fg + ';';
  if (bg) css += 'background:' + bg + ';';
  if (a & F.BOLD) css += 'font-weight:bold;';
  if (a & F.ITALIC) css += 'font-style:italic;';
  if (a & F.DIM) css += 'opacity:.6;';
  const lines = [];
  if (a & (F.UNDERLINE | F.DOUBLE_UNDERLINE | F.UNDERCURL | F.DOTTED_UNDERLINE | F.DASHED_UNDERLINE)) lines.push('underline');
  if (a & F.STRIKEOUT) lines.push('line-through');
  if (lines.length) {
    css += 'text-decoration-line:' + lines.join(' ') + ';';
    const kind = a & F.DOUBLE_UNDERLINE ? 'double' : a & F.UNDERCURL ? 'wavy' : a & F.DOTTED_UNDERLINE ? 'dotted' : a & F.DASHED_UNDERLINE ? 'dashed' : '';
    if (kind) css += 'text-decoration-style:' + kind + ';';
  }
  if (a & F.HIDDEN) css += 'color:transparent;';
  return css;
}

export class StyleSheet {
  constructor(doc = document) {
    this.el = doc.createElement('style');
    doc.head.appendChild(this.el);
    this.classes = new Map(); // "f,b,a" → 类名
    this.rules = []; // [类名, f, b, a]
    this.theme = null;
    this.palette = null;
  }

  /** 段的类名；默认样式返回空串 */
  cls(f = -1, b = -1, a = 0) {
    if (f === -1 && b === -1 && a === 0) return '';
    const key = f + ',' + b + ',' + a;
    let c = this.classes.get(key);
    if (c) return c;
    c = 's' + this.classes.size.toString(36);
    this.classes.set(key, c);
    this.rules.push([c, f, b, a]);
    try {
      this.el.sheet.insertRule('.lld .' + c + '{' + styleDecl(f, b, a, this.theme, this.palette) + '}', this.el.sheet.cssRules.length);
    } catch {
      this.rewrite();
    }
    return c;
  }

  /** 换配色、程序改了调色板：重写整张表（类名不变，行不用重画） */
  setTheme(theme, palette) {
    this.theme = theme || null;
    this.palette = palette || null;
    this.rewrite();
  }

  rewrite() {
    this.el.textContent = this.rules.map(([c, f, b, a]) => '.lld .' + c + '{' + styleDecl(f, b, a, this.theme, this.palette) + '}').join('\n');
  }
}

const ESC = { '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;' };
export function escapeHtml(t) {
  return t.replace(/[&<>"]/g, (c) => ESC[c]);
}

/**
 * 一行的段 → HTML。非 ASCII 的格子每个包成定宽盒子（.w1 / .w2，宽度由 CSS 变量 --cw 定），
 * 字体里这个字宽一点窄一点都不影响后面的列（设计稿 §6.2）。
 */
export function runsHtml(runs, sheet) {
  let out = '';
  for (const r of runs) {
    const c = sheet.cls(r.f, r.b, r.a);
    let body;
    if (r.w) {
      const tag = r.w === 2 ? '<i class="w2">' : '<i class="w1">';
      body = '';
      for (const g of r.c) body += tag + escapeHtml(g) + '</i>';
    } else {
      body = escapeHtml(r.t);
    }
    if (r.l) out += '<span class="lk' + (c ? ' ' + c : '') + '" data-l="' + escapeHtml(r.l) + '">' + body + '</span>';
    else out += c ? '<span class="' + c + '">' + body + '</span>' : body;
  }
  return out;
}

/** 一行的段 → 纯文字（复制用） */
export function runsText(runs) {
  let s = '';
  for (const r of runs) s += r.w ? r.c.join('') : r.t;
  return s;
}
