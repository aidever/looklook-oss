// 电脑上的终端：wgpu 画在一块画布上（设计稿 §7，WASM 在 crates/term-gpu）。
//
// - 屏幕、回滚行都在 WASM 里（TermClient.keep_history），这里只管事件、尺寸、取回滚行、什么时候画；
// - 格子按设备像素对齐（画布按 devicePixelContentBox 设尺寸，字号乘以 devicePixelRatio），125% / 150% 缩放不糊不裁；
// - 滚轮 / 触控板逐像素滚动本地回滚区；程序在备用屏幕或开了鼠标时滚轮交给程序（按住 Shift 强制本地）；
// - 选字自己做：拖选、双击选词、三击选行、Alt 拖动按列选；程序开了鼠标时按住 Shift 拖动强制本地选字；
// - Ctrl+Alt+F（Mac ⌘F）搜索回滚区和屏幕；输入法组字跟着光标走；另挂一份隐藏的文字镜像给读屏器。

import { colorCss } from '../dom/style.js';
import { pasteBytes } from '../dom/keys.js';
import { attachTextInput, IS_MAC } from '../dom/input.js';

const PAD = 5; // 四周留白（CSS 像素）
const FIRST_HISTORY = 2000; // 连上后先取这么多回滚行
const CHUNK = 1000; // 一次最多取这么多
const BLINK_MS = 530;

let probeCtx = null;
/** CSS 颜色 → 0xAARRGGBB（用画布把各种写法规整成 #rrggbb / rgba(...)） */
export function cssToArgb(css, fallback) {
  if (!css) return fallback >>> 0;
  probeCtx ||= document.createElement('canvas').getContext('2d');
  probeCtx.fillStyle = '#010203';
  probeCtx.fillStyle = css;
  const v = probeCtx.fillStyle;
  if (v === '#010203' && css.trim().toLowerCase() !== '#010203') return fallback >>> 0;
  if (v[0] === '#') return (0xff000000 | parseInt(v.slice(1, 7), 16)) >>> 0;
  const m = v.match(/rgba?\(\s*(\d+),\s*(\d+),\s*(\d+)(?:,\s*([\d.]+))?/);
  if (!m) return fallback >>> 0;
  const a = m[4] === undefined ? 255 : Math.round(Number(m[4]) * 255);
  return ((a << 24) | (Number(m[1]) << 16) | (Number(m[2]) << 8) | Number(m[3])) >>> 0;
}

/** 主题 → term-gpu 的配色表（crates/term-gpu/src/theme.rs） */
export function themeTable(theme) {
  const t = new Uint32Array(263);
  for (let i = 0; i < 256; i++) t[i] = cssToArgb(colorCss(i, theme, null), 0xff000000);
  const fg = cssToArgb(theme.foreground, 0xffffffff);
  const bg = cssToArgb(theme.background, 0xff000000);
  t[256] = fg;
  t[257] = bg;
  t[258] = cssToArgb(theme.cursor, fg);
  t[259] = cssToArgb(theme.cursorAccent, bg);
  t[260] = cssToArgb(theme.selectionBackground, 0x73788cff);
  t[261] = 0x99ffd54a;
  t[262] = 0xccff9632;
  return t;
}

export class GpuTerminal {
  /**
   * @param {HTMLElement} parent
   * @param {object} prefs 网关给的首选项（字体、配色）
   * @param {import('../dom/conn.js').TermConnection} conn
   */
  constructor(parent, prefs, conn) {
    this.parent = parent;
    this.conn = conn;
    this.fontFamily = prefs.fontFamily || 'Consolas,Liberation Mono,Menlo,Courier,monospace';
    this.fontSize = prefs.fontSize || 13;
    this.lineHeight = prefs.lineHeight || 1.15;
    this.theme = prefs.theme || {};
    this.disableStdin = false;

    this.cols = 0;
    this.rowsN = 0;
    this.histCount = 0; // window.term.buffer 只给屏幕上的行
    this.modes = { alt: false, mouse: 'none', appCursor: false, bracketed: false, altScroll: true };
    this.cursor = { r: 0, c: 0, v: true, s: 'block', blink: false };
    this.title = null;
    this.renderHooks = new Set();
    this.oscHandlers = new Map();

    this.dpr = window.devicePixelRatio || 1;
    this.cellW = 8;
    this.lineH = 16;
    this.raf = 0;
    this.frameDirty = false; // 有新画面没 Ack
    this.focused = false;
    this.cursorOn = true;
    this.blinkTimer = 0;
    this.hoverUrl = null;
    this.histBusy = false;
    this.histDead = '';
    this.wheelAcc = 0;
  }

  get tc() {
    return this.conn.tc;
  }

  // ---------- 搭建 ----------

  async init(mod, forceGl) {
    const root = (this.root = document.createElement('div'));
    root.className = 'llg';
    const canvas = (this.canvas = document.createElement('canvas'));
    canvas.className = 'llg-canvas';
    const ta = (this.ta = document.createElement('textarea'));
    ta.className = 'llg-ta';
    for (const [k, v] of [['autocapitalize', 'off'], ['autocomplete', 'off'], ['autocorrect', 'off'], ['spellcheck', 'false'], ['aria-label', 'Terminal input']]) {
      ta.setAttribute(k, v);
    }
    this.ime = document.createElement('span');
    this.ime.className = 'llg-ime';
    this.mirror = document.createElement('div');
    this.mirror.className = 'llg-a11y';
    this.mirror.setAttribute('role', 'document');
    this.mirror.setAttribute('aria-label', 'Terminal output');
    this.pill = document.createElement('button');
    this.pill.type = 'button';
    this.pill.className = 'llg-bottom';
    this.pill.innerHTML = '<svg viewBox="0 0 24 24" width="18" height="18" aria-hidden="true"><path d="M12 5v14M5 12l7 7 7-7" fill="none" stroke="currentColor" stroke-width="2.2" stroke-linecap="round" stroke-linejoin="round"/></svg>';
    this.pill.addEventListener('mousedown', (e) => e.preventDefault());
    this.pill.addEventListener('click', () => this.toBottom());
    root.append(canvas, this.ime, this.pill, this.mirror);
    this.parent.append(root, ta);
    this.buildSearch();

    const r = root.getBoundingClientRect();
    canvas.width = Math.max(1, Math.round(r.width * this.dpr));
    canvas.height = Math.max(1, Math.round(r.height * this.dpr));
    this.gpu = await mod.GpuTerm.create(canvas, Boolean(forceGl));
    document.documentElement.dataset.renderer = this.gpu.backend();
    this.applyFont();
    this.applyTheme();

    this.textInput = attachTextInput(ta, {
      modes: () => this.modes,
      sendRaw: (d, keep) => this.sendRaw(d, keep),
      sendTyped: (t) => this.sendTyped(t),
      paste: (t) => this.paste(t),
      onKey: (e) => this.onKey(e),
      onCompose: (t) => this.onCompose(t),
    });
    ta.addEventListener('focus', () => this.setFocused(true));
    ta.addEventListener('blur', () => this.setFocused(false));
    document.addEventListener('copy', (e) => this.onCopy(e));
    this.setupPointer();

    const ro = new ResizeObserver((entries) => this.onResize(entries[entries.length - 1]));
    try {
      ro.observe(root, { box: 'device-pixel-content-box' });
    } catch {
      ro.observe(root);
    }
    document.addEventListener('visibilitychange', () => this.conn.setView(document.visibilityState === 'visible' ? 0 : 2));
  }

  applyFont() {
    this.dpr = window.devicePixelRatio || 1;
    const [cw, ch] = this.gpu.set_font(this.fontFamily, this.fontSize * this.dpr, this.lineHeight, Math.round(PAD * this.dpr));
    this.cellW = cw / this.dpr;
    this.lineH = ch / this.dpr;
    this.redraw();
  }

  measure() {
    this.applyFont();
  }

  applyTheme() {
    this.gpu.set_theme(themeTable(this.theme));
    const bg = this.theme.background || '#000';
    this.root.style.background = bg;
    document.body.style.background = bg;
    this.ime.style.background = bg;
    this.ime.style.color = this.theme.foreground || '#fff';
    this.redraw();
  }

  setTheme(th) {
    this.theme = th || {};
    this.applyTheme();
  }

  // ---------- 尺寸 ----------

  onResize(entry) {
    const dpr = window.devicePixelRatio || 1;
    const r = this.root.getBoundingClientRect();
    let w = Math.round(r.width * dpr);
    let h = Math.round(r.height * dpr);
    // devicePixelContentBox 是浏览器实际的设备像素（小数缩放下比自己乘更准）；和估算差太多（如开发者工具模拟缩放时）就不用它
    const box = entry?.devicePixelContentBoxSize?.[0];
    if (box && Math.abs(box.inlineSize - w) <= 2 && Math.abs(box.blockSize - h) <= 2) {
      w = box.inlineSize;
      h = box.blockSize;
    }
    if (w < 1 || h < 1) return;
    this.canvas.width = w;
    this.canvas.height = h;
    this.gpu.resize(w, h);
    if (dpr !== this.dpr) this.applyFont(); // 浏览器缩放、拖到另一块屏幕
    this.fit();
    // 改了画布尺寸画面会被清掉：马上补一帧，不闪
    this.draw();
  }

  proposed() {
    if (this.root.clientWidth < 40 || this.root.clientHeight < 20) return null; // 藏起来了
    const [cols, rows] = this.gpu.grid(this.canvas.width, this.canvas.height);
    return { cols, rows };
  }

  proposedOrDefault() {
    return this.proposed() || { cols: 80, rows: 24 };
  }

  fit() {
    const p = this.proposed();
    if (p) this.conn.resize(p.cols, p.rows);
  }

  // ---------- 更新与绘制 ----------

  push(list) {
    for (const u of list) {
      switch (u.t) {
        case 'screen':
          this.frameDirty = true;
          this.cols = u.cols;
          this.rowsN = u.rows;
          this.cursor = u.cursor;
          Object.assign(this.modes, { alt: u.alt, mouse: u.mouse, appCursor: u.appCursor, bracketed: u.bracketed, altScroll: u.altScroll });
          if (u.title !== this.title) this.setTitle(u.title);
          this.cursorOn = true;
          this.restartBlink();
          break;
        case 'title':
          this.setTitle(u.v);
          break;
        case 'clipboard':
          this.onClipboard(u.v);
          break;
        case 'cleared':
          this.histDead = '';
          break;
        case 'progress':
          this.onProgress?.(u);
          break;
        case 'error':
          console.warn('[looklook] termd:', u.code, u.msg);
          break;
      }
    }
    this.redraw();
  }

  redraw() {
    if (!this.raf) this.raf = requestAnimationFrame(() => this.draw());
  }

  draw() {
    if (this.raf) cancelAnimationFrame(this.raf);
    this.raf = 0;
    const tc = this.tc;
    if (!tc || !this.gpu || !tc.cols()) return;
    this.gpu.render(tc, this.cursorOn, this.focused);
    if (this.frameDirty) {
      this.frameDirty = false;
      this.conn.ack();
      for (const fn of this.renderHooks) {
        try {
          fn();
        } catch (e) {
          console.error(e);
        }
      }
      this.scheduleMirror();
    }
    this.pill.classList.toggle('on', !this.gpu.pinned());
    this.placeTextarea();
    this.fetchHistory();
  }

  setTitle(t) {
    this.title = t;
    this.onTitle?.(t);
  }

  onClipboard(text) {
    const fn = this.oscHandlers.get(52);
    if (!fn) return;
    const b64 = btoa(String.fromCharCode(...new TextEncoder().encode(text)));
    fn('c;' + b64);
  }

  setFocused(f) {
    this.focused = f;
    this.cursorOn = true;
    this.restartBlink();
    this.redraw();
  }

  restartBlink() {
    clearInterval(this.blinkTimer);
    this.blinkTimer = 0;
    if (!this.focused || !this.cursor.blink || matchMedia('(prefers-reduced-motion: reduce)').matches) return;
    this.blinkTimer = setInterval(() => {
      this.cursorOn = !this.cursorOn;
      this.redraw();
    }, BLINK_MS);
  }

  /** 输入框跟着光标（输入法候选框出在光标处） */
  placeTextarea() {
    const [x, y, w, h] = this.gpu.cursor_rect(this.tc);
    const cx = x / this.dpr, cy = y / this.dpr;
    this.ta.style.transform = `translate(${cx}px,${cy}px)`;
    this.ta.style.height = h / this.dpr + 'px';
    this.ime.style.transform = `translate(${cx}px,${cy}px)`;
    this.ime.style.height = this.ime.style.lineHeight = h / this.dpr + 'px';
    void w;
  }

  onCompose(text) {
    if (text) {
      this.ime.textContent = text;
      this.ime.style.font = `${this.fontSize}px ${this.fontFamily}`;
      this.ime.classList.add('on');
    } else {
      this.ime.textContent = '';
      this.ime.classList.remove('on');
    }
  }

  // ---------- 回滚区 ----------

  /** 看得见的范围（上下各多两屏）里缺的回滚行，一段一段找 termd 要 */
  fetchHistory() {
    const tc = this.tc;
    if (this.histBusy || this.modes.alt || !tc) return;
    const b = tc.history_bounds();
    if (!b.length) return;
    const base = b[2];
    const top = this.gpu.top();
    const rows = this.rowsN || 24;
    const from = Math.min(top - rows * 2, base - FIRST_HISTORY);
    const to = top + rows;
    const miss = tc.history_missing(Math.max(0, from), to);
    if (!miss.length) return;
    let [epoch, start, count] = miss;
    if (count > CHUNK) {
      // 先取离看的地方最近的那一段
      start = start + count - CHUNK;
      count = CHUNK;
    }
    const key = epoch + ':' + start + ':' + count;
    if (key === this.histDead) return;
    this.histBusy = true;
    this.conn.history(epoch, start, count).then((u) => {
      this.histBusy = false;
      if (!u || u.mismatch || !u.n) {
        this.histDead = key; // termd 给不了：别一直要
        return;
      }
      this.redraw();
    });
  }

  // ---------- 输入 ----------

  sendTyped(text) {
    if (this.disableStdin || !text) return;
    const mods = window.__llApplyMods;
    const out = typeof mods === 'function' ? mods(text) : text;
    // 按键条的 Ctrl / Alt 把字变成了控制键：终端里没有这个字，输入框也清空
    if (out !== text) this.textInput?.reset();
    this.conn.input(out, true);
    this.afterInput();
  }

  /** keepInput：输入框自己按差补发的退格，不清空输入框；别的（按键条、回车、方向键、粘贴）都清空，跟终端重新对齐 */
  sendRaw(data, keepInput) {
    if (this.disableStdin) return;
    if (!keepInput) this.textInput?.reset();
    this.conn.input(data);
    this.afterInput();
  }

  afterInput() {
    if (this.gpu.has_selection()) this.clearSelection();
    if (!this.gpu.pinned()) this.toBottom();
    this.cursorOn = true;
    this.restartBlink();
  }

  paste(text) {
    if (!text) return;
    this.sendRaw(pasteBytes(text, this.modes.bracketed));
  }

  /** 快捷键（在发给终端之前）：返回 true 已处理，'pass' 交给浏览器 */
  onKey(e) {
    const k = e.key;
    const accel = IS_MAC ? e.metaKey && !e.ctrlKey : e.ctrlKey && e.shiftKey;
    if (accel && (k === 'c' || k === 'C')) return 'pass'; // copy 事件里给选区的文字
    if (accel && (k === 'v' || k === 'V')) return 'pass'; // paste 事件
    // Windows / Linux：有选区时 Ctrl+C 复制（不发 ^C），和 Windows Terminal 一样
    if (!IS_MAC && e.ctrlKey && !e.shiftKey && !e.altKey && (k === 'c' || k === 'C') && this.gpu.has_selection()) return 'pass';
    // 搜索：Mac ⌘F；其他 Ctrl+Alt+F（Ctrl+Shift+F 是管理台的“文件”，Ctrl+F 是 shell 的光标右移）
    const find = IS_MAC ? e.metaKey && !e.ctrlKey && !e.shiftKey && !e.altKey : e.ctrlKey && e.altKey && !e.shiftKey && !e.metaKey;
    if (find && e.code === 'KeyF') {
      this.openSearch();
      return true;
    }
    if (accel && (k === 'a' || k === 'A')) {
      this.gpu.select_all(this.tc);
      this.redraw();
      return true;
    }
    if (e.shiftKey && !e.ctrlKey && !e.altKey && !e.metaKey && !this.modes.alt) {
      const page = Math.max(1, this.rowsN - 1);
      const step = { PageUp: -page, PageDown: page, Home: -1e9, End: 1e9 }[k];
      if (step !== undefined) {
        this.gpu.scroll_lines(this.tc, step);
        this.redraw();
        return true;
      }
    }
    if (k === 'Escape' && this.gpu.has_selection()) {
      this.clearSelection();
      return true;
    }
    return false;
  }

  onCopy(e) {
    if (!this.gpu?.has_selection()) return;
    const text = this.gpu.selection_text(this.tc);
    if (!text) return;
    e.preventDefault();
    e.clipboardData?.setData('text/plain', text);
    // 复制完选区留着（再按一次还能复制）；Ctrl+C 复制后清掉，下一次 Ctrl+C 就是 ^C
    if (!IS_MAC) setTimeout(() => this.clearSelection(), 0);
  }

  focus() {
    this.ta.focus({ preventScroll: true });
  }

  // ---------- 选区 ----------

  selectionText() {
    return this.gpu?.has_selection() ? this.gpu.selection_text(this.tc) : '';
  }

  clearSelection() {
    this.gpu.select_clear();
    this.redraw();
  }

  // ---------- 滚动 ----------

  appMode() {
    return this.modes.alt || this.modes.mouse !== 'none';
  }

  toBottom() {
    this.gpu.scroll_to_bottom(this.tc);
    this.redraw();
  }

  /** 点（CSS 像素，相对页面）→ 画布设备像素 */
  devXY(e) {
    const r = this.canvas.getBoundingClientRect();
    return [(e.clientX - r.left) * this.dpr, (e.clientY - r.top) * this.dpr];
  }

  cellOf(e) {
    const [x, y] = this.devXY(e);
    const [, col, row] = this.gpu.cell_at(this.tc, x, y);
    return { col, row: Math.max(0, Math.min(this.rowsN - 1, row)) };
  }

  mouseMods(e) {
    return (e.shiftKey ? 4 : 0) | (e.altKey || e.metaKey ? 8 : 0) | (e.ctrlKey ? 16 : 0);
  }

  // ---------- 指针 ----------

  setupPointer() {
    const cv = this.canvas;
    cv.addEventListener('wheel', (e) => this.onWheel(e), { passive: false });
    let drag = null; // { x, y, local, moved, button }
    let autoscroll = 0;
    const stopAuto = () => {
      clearInterval(autoscroll);
      autoscroll = 0;
    };
    cv.addEventListener('mousedown', (e) => {
      e.preventDefault();
      this.focus();
      const reportToApp = this.modes.mouse !== 'none' && !e.shiftKey;
      if (reportToApp) {
        const { col, row } = this.cellOf(e);
        this.conn.mouse(0, e.button, col, row, this.mouseMods(e));
        drag = { local: false, button: e.button };
        return;
      }
      if (e.button !== 0) return;
      const [x, y] = this.devXY(e);
      const mode = e.detail >= 3 ? 2 : e.detail === 2 ? 1 : 0;
      this.gpu.select_start(this.tc, x, y, mode, e.altKey);
      drag = { local: true, x: e.clientX, y: e.clientY, moved: mode > 0 };
      this.redraw();
    });
    window.addEventListener('mousemove', (e) => {
      if (!drag) {
        if (e.target === cv) this.onHover(e);
        return;
      }
      if (!drag.local) {
        if (this.modes.mouse === 'drag' || this.modes.mouse === 'any') {
          const { col, row } = this.cellOf(e);
          this.conn.mouse(2, drag.button, col, row, this.mouseMods(e));
        }
        return;
      }
      if (Math.abs(e.clientX - drag.x) + Math.abs(e.clientY - drag.y) > 3) drag.moved = true;
      if (!drag.moved) return;
      const [x, y] = this.devXY(e);
      this.gpu.select_extend(this.tc, x, y);
      // 拖到画布外：自动滚
      const r = cv.getBoundingClientRect();
      const dir = e.clientY < r.top ? -1 : e.clientY > r.bottom ? 1 : 0;
      stopAuto();
      if (dir && !this.modes.alt) {
        autoscroll = setInterval(() => {
          this.gpu.scroll_lines(this.tc, dir);
          this.gpu.select_extend(this.tc, x, dir < 0 ? 0 : cv.height - 1);
          this.redraw();
        }, 50);
      }
      this.redraw();
    });
    window.addEventListener('mouseup', (e) => {
      stopAuto();
      const d = drag;
      drag = null;
      if (!d) return;
      if (!d.local) {
        const { col, row } = this.cellOf(e);
        this.conn.mouse(1, d.button, col, row, this.mouseMods(e));
        return;
      }
      if (!d.moved) {
        // 单击：清掉选区；点在链接上就打开
        this.gpu.select_clear();
        this.redraw();
        if (this.hoverUrl && e.target === cv) window.open(this.hoverUrl, '_blank', 'noopener');
      }
    });
    cv.addEventListener('mouseleave', () => {
      if (this.hoverUrl) {
        this.hoverUrl = null;
        this.gpu.hover_clear();
        cv.style.cursor = '';
        this.redraw();
      }
    });
    cv.addEventListener('contextmenu', (e) => {
      if (this.modes.mouse !== 'none' && !e.shiftKey) {
        e.preventDefault();
        return;
      }
      // 右键：有选区复制，没有就粘贴（剪贴板读不到时让浏览器出自己的菜单）
      if (this.gpu.has_selection()) {
        e.preventDefault();
        const text = this.gpu.selection_text(this.tc);
        navigator.clipboard?.writeText(text).catch(() => {});
        this.clearSelection();
      } else if (navigator.clipboard?.readText) {
        e.preventDefault();
        navigator.clipboard.readText().then((t) => this.paste(t)).catch(() => {});
      }
    });
  }

  onHover(e) {
    if (this.modes.mouse === 'any') {
      const { col, row } = this.cellOf(e);
      this.conn.mouse(3, 0, col, row, this.mouseMods(e));
    }
    const [x, y] = this.devXY(e);
    const url = this.gpu.hover(this.tc, x, y) || null;
    if (url !== this.hoverUrl) {
      this.hoverUrl = url;
      this.canvas.style.cursor = url ? 'pointer' : '';
      this.canvas.title = url || '';
      this.redraw();
    }
  }

  onWheel(e) {
    e.preventDefault();
    const px = e.deltaMode === 1 ? e.deltaY * this.lineH : e.deltaMode === 2 ? e.deltaY * this.lineH * this.rowsN : e.deltaY;
    if (this.appMode() && !e.shiftKey) {
      // 交给程序：攒够一行发一格
      this.wheelAcc += px;
      const steps = Math.trunc(this.wheelAcc / this.lineH);
      if (!steps) return;
      this.wheelAcc -= steps * this.lineH;
      const up = steps < 0;
      const { col, row } = this.cellOf(e);
      for (let i = 0; i < Math.min(Math.abs(steps), 10); i++) {
        if (this.modes.mouse !== 'none') this.conn.mouse(up ? 4 : 5, 0, col, row, this.mouseMods(e));
        else if (this.modes.altScroll) this.sendRaw(this.modes.appCursor ? (up ? '\x1bOA' : '\x1bOB') : up ? '\x1b[A' : '\x1b[B');
      }
      return;
    }
    this.gpu.scroll_px(this.tc, px * this.dpr);
    this.redraw();
  }

  // ---------- 搜索 ----------

  buildSearch() {
    const box = (this.searchBox = document.createElement('div'));
    box.className = 'llg-search';
    box.innerHTML =
      '<input type="text" spellcheck="false" aria-label="Search">' +
      '<span class="llg-count"></span>' +
      '<button type="button" data-a="case" title="Match case">Aa</button>' +
      '<button type="button" data-a="up" title="Previous (Enter)">↑</button>' +
      '<button type="button" data-a="down" title="Next (Shift+Enter)">↓</button>' +
      '<button type="button" data-a="close" title="Close (Esc)">×</button>';
    this.root.appendChild(box);
    const input = (this.searchInput = box.querySelector('input'));
    this.searchCount = box.querySelector('.llg-count');
    this.searchCase = false;
    input.addEventListener('input', () => this.runSearch(true, true));
    input.addEventListener('keydown', (e) => {
      if (e.key === 'Enter') {
        e.preventDefault();
        this.runSearch(!e.shiftKey);
      } else if (e.key === 'Escape') {
        e.preventDefault();
        this.closeSearch();
      }
    });
    box.addEventListener('mousedown', (e) => {
      if (e.target.tagName === 'BUTTON') e.preventDefault();
    });
    box.addEventListener('click', (e) => {
      const a = e.target.dataset?.a;
      if (a === 'close') this.closeSearch();
      else if (a === 'up') this.runSearch(true);
      else if (a === 'down') this.runSearch(false);
      else if (a === 'case') {
        this.searchCase = !this.searchCase;
        e.target.classList.toggle('on', this.searchCase);
        this.runSearch(true, true);
      }
    });
  }

  openSearch() {
    this.searchBox.classList.add('on');
    const sel = this.selectionText();
    if (sel && !sel.includes('\n')) this.searchInput.value = sel;
    this.searchInput.select();
    this.searchInput.focus();
    if (this.searchInput.value) this.runSearch(true, true);
  }

  closeSearch() {
    this.searchBox.classList.remove('on');
    this.gpu.search_clear();
    this.redraw();
    this.focus();
  }

  runSearch(older, fresh = false) {
    const q = this.searchInput.value;
    if (fresh) this.gpu.search_clear();
    if (!q) {
      this.searchCount.textContent = '';
      this.redraw();
      return;
    }
    const [n, cur] = this.gpu.search(this.tc, q, this.searchCase, older);
    this.searchCount.textContent = n ? `${cur}/${n}` : '0/0';
    this.redraw();
  }

  // ---------- 读屏 ----------

  scheduleMirror() {
    if (this.mirrorTimer) return;
    this.mirrorTimer = setTimeout(() => {
      this.mirrorTimer = 0;
      const lines = [];
      for (let i = 0; i < this.rowsN; i++) lines.push(this.tc.row_text(i));
      while (lines.length && !lines[lines.length - 1]) lines.pop();
      this.mirror.textContent = lines.join('\n');
    }, 500);
  }

  // ---------- window.term 兼容 ----------

  bufferLine(i) {
    if (i < 0 || i >= this.rowsN || !this.tc) return null;
    return { text: this.tc.row_text(i), wrappedFromPrev: i > 0 && this.tc.row_wrapped(i - 1) };
  }
}
