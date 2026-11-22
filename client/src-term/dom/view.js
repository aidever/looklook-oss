// 手机上的终端：一层真正的网页文字（设计稿 §6）。没有画布，也没有两层对齐的问题：
//
//   .lld（原生滚动容器）
//     .lld-hist  回滚区：termd 推来的行，按 64 行一块，content-visibility 让屏幕外的块不排版
//     .lld-live  实时屏幕：rows 行，每帧只改有变化的行；整块上移时直接挪 DOM 节点，不重画
//
// - 宽度由 termd 定：ASCII 用等宽字体自然排，非 ASCII 的字每个格子一个定宽盒子（--cw 的 1 / 2 倍），字体回退不影响后面的列；
// - 选字用浏览器自己的（长按、选择把手、系统菜单）。有选区时画面停住（新的行攒着，选区消失时一次补上），选着选着内容不会变；
// - 普通屏幕：上滑是本地原生滚动，带惯性，回滚区就是程序真正的回滚区（中间没有 tmux）；
//   备用屏幕（vim、less、htop）或程序开了鼠标：滑动变成滚轮上报（开了鼠标）或 ↑ / ↓，点一下变成鼠标点击；
// - 键盘弹出不改行列数：容器让出键盘的高度，保证光标那一行露在键盘上面（§6.4）。

import { StyleSheet, runsHtml } from './style.js';
import { pasteBytes } from './keys.js';
import { attachTextInput } from './input.js';

const PAD = 5; // 四周留白（和 xterm 的页面一样）
const BUCKET = 64; // 回滚区一块的行数
const MAX_HIST = 20000; // 浏览器里最多留这么多回滚行
const FIRST_HISTORY = 2000; // 连上后先取这么多
const OLDER_BATCH = 1000; // 往上翻到头时再取这么多

export const isTouch = () => Boolean(window.matchMedia && window.matchMedia('(hover: none) and (pointer: coarse)').matches);

export class DomTerminal {
  /**
   * @param {HTMLElement} parent
   * @param {object} prefs 网关给的首选项（字体、配色，见 gateway::termd_ws::term_boot_tag）
   * @param {import('./conn.js').TermConnection} conn
   */
  constructor(parent, prefs, conn) {
    this.parent = parent;
    this.conn = conn;
    this.touch = isTouch();
    this.fontFamily = prefs.fontFamily || 'Consolas,Liberation Mono,Menlo,Courier,monospace';
    this.fontSize = prefs.fontSize || 13;
    this.lineHeight = prefs.lineHeight || 1.15;
    this.theme = prefs.theme || {};
    this.palette = null;
    this.disableStdin = false;

    this.sheet = new StyleSheet();
    this.cellW = 8;
    this.lineH = 15;
    this.cols = 0; // 当前画面（termd 定的）的行列数
    this.rowsN = 0;
    this.rowEls = [];
    this.modes = { alt: false, mouse: 'none', appCursor: false, bracketed: false, altScroll: true };
    this.cursor = { r: 0, c: 0, v: true, s: 'block', blink: false };
    this.title = null;

    // 回滚区
    this.epoch = null;
    this.buckets = new Map(); // 块号 → 元素
    this.bucketKeys = []; // 块号，从小到大
    this.histCount = 0;
    this.minSeq = Infinity;
    this.maxSeq = -1;
    this.sbInfo = [0, 0, 0]; // termd 那边的 [epoch, 最早, 下一个]
    this.loadingOlder = false;
    this.filling = false;

    // 画面状态
    this.stick = true; // 停在最下面：有新输出时跟着走
    this.frozen = false; // 有选区：画面停住
    this.frozenDirty = false;
    this.frozenScrolled = [];
    this.queue = []; // 还没画的更新
    this.raf = 0;
    this.renderHooks = new Set();
    this.oscHandlers = new Map();
    this.keyboardInset = 0;

    this.build();
  }

  // ---------- 搭建 ----------

  build() {
    const root = (this.root = document.createElement('div'));
    root.className = 'lld';
    root.tabIndex = -1;
    const pad = document.createElement('div');
    pad.className = 'lld-pad';
    this.hist = document.createElement('div');
    this.hist.className = 'lld-hist';
    this.live = document.createElement('div');
    this.live.className = 'lld-live';
    this.cursorEl = document.createElement('div');
    this.cursorEl.className = 'lld-cur';
    this.live.appendChild(this.cursorEl);
    // 本地预测回显（§5.7）：还没确认的字盖在那一格上
    this.predEl = document.createElement('div');
    this.predEl.className = 'lld-pred';
    this.live.insertBefore(this.predEl, this.cursorEl);
    this.predKey = '';
    pad.append(this.hist, this.live);
    root.appendChild(pad);

    const ta = (this.ta = document.createElement('textarea'));
    ta.className = 'lld-ta';
    ta.setAttribute('autocapitalize', 'off');
    ta.setAttribute('autocomplete', 'off');
    ta.setAttribute('autocorrect', 'off');
    ta.setAttribute('spellcheck', 'false');
    ta.setAttribute('aria-label', 'Terminal input');

    const pill = (this.pill = document.createElement('button'));
    pill.type = 'button';
    pill.className = 'lld-bottom';
    pill.innerHTML = '<svg viewBox="0 0 24 24" width="18" height="18" aria-hidden="true"><path d="M12 5v14M5 12l7 7 7-7" fill="none" stroke="currentColor" stroke-width="2.2" stroke-linecap="round" stroke-linejoin="round"/></svg>';
    pill.addEventListener('pointerdown', (e) => e.preventDefault());
    pill.addEventListener('click', () => this.toBottom());

    this.parent.append(root, ta, pill);
    this.applyFont();
    this.applyTheme();

    root.addEventListener('scroll', () => this.onScroll(), { passive: true });
    document.addEventListener('selectionchange', () => this.onSelection());
    root.addEventListener('copy', (e) => this.onCopy(e));
    this.setupInput();
    this.setupPointer();
    window.addEventListener('resize', () => this.fit());
    window.visualViewport?.addEventListener('resize', () => this.onViewport());
    window.visualViewport?.addEventListener('scroll', () => this.onViewport());
    document.addEventListener('visibilitychange', () => this.conn.setView(document.visibilityState === 'visible' ? 0 : 2));
  }

  applyFont() {
    const s = this.root.style;
    s.fontFamily = this.fontFamily;
    s.fontSize = this.fontSize + 'px';
    this.measure();
  }

  /** 量格子：等宽英文字体里一个字的宽度（小数照用），行高取整数像素（行与行之间不会出现缝） */
  measure() {
    const probe = document.createElement('span');
    probe.textContent = 'W'.repeat(100);
    probe.style.cssText = 'position:absolute;visibility:hidden;white-space:pre;left:0;top:0';
    this.live.appendChild(probe);
    const w = probe.getBoundingClientRect().width / 100;
    probe.remove();
    this.cellW = w > 0 ? w : this.fontSize * 0.6;
    this.lineH = Math.max(8, Math.round(this.fontSize * this.lineHeight));
    this.root.style.setProperty('--cw', this.cellW + 'px');
    this.root.style.setProperty('--lh', this.lineH + 'px');
    if (this.cols) this.live.style.width = this.cols * this.cellW + 'px';
    this.placeCursor();
  }

  applyTheme() {
    const th = this.theme || {};
    const s = this.root.style;
    s.setProperty('--bg', th.background || '#000');
    s.setProperty('--fg', th.foreground || '#fff');
    s.setProperty('--cursor', th.cursor || th.foreground || '#fff');
    s.setProperty('--sel', th.selectionBackground || 'rgba(120,140,255,.45)');
    document.body.style.background = th.background || '#000';
    this.sheet.setTheme(th, this.palette);
  }

  setTheme(th) {
    this.theme = th || {};
    this.applyTheme();
  }

  // ---------- 尺寸 ----------

  /** 按容器大小算出的行列数 */
  proposed() {
    const w = this.parent.clientWidth - PAD * 2;
    const h = this.parent.clientHeight - PAD * 2;
    if (w < 40 || h < 20) return null; // 藏起来了（iframe display:none），不报一个很小的尺寸
    return { cols: Math.max(2, Math.floor(w / this.cellW)), rows: Math.max(1, Math.floor(h / this.lineH)) };
  }

  /** 键盘弹出（输入框聚焦着、只是高度变了）不改行列数，只让出键盘的高度 */
  fit() {
    if (this.fitRaf) return;
    let last = null, tries = 0;
    const check = () => {
      this.fitRaf = 0;
      const p = this.proposed();
      if (!p) return;
      // 连着两帧量到一样的才算数（WKWebView 里 iframe 刚显示时头一两帧尺寸不对）
      if ((!last || last.cols !== p.cols || last.rows !== p.rows) && tries++ < 10) {
        last = p;
        this.fitRaf = requestAnimationFrame(check);
        return;
      }
      const typing = this.touch && document.activeElement === this.ta;
      if (typing && this.cols === p.cols && p.rows < this.conn.size.rows) return;
      this.conn.resize(p.cols, p.rows);
    };
    this.fitRaf = requestAnimationFrame(check);
  }

  proposedOrDefault() {
    return this.proposed() || { cols: 80, rows: 24 };
  }

  /** 键盘挡住的高度：容器底部让出来，光标那一行露在键盘上面 */
  onViewport() {
    const vv = window.visualViewport;
    if (!vv) return;
    const rect = this.parent.getBoundingClientRect();
    const inset = Math.max(0, Math.round(rect.bottom - (vv.offsetTop + vv.height)));
    if (inset === this.keyboardInset) return;
    this.keyboardInset = inset;
    this.root.style.bottom = inset + 'px';
    this.keepCursorVisible();
  }

  // ---------- 更新 ----------

  /** 收到一批更新：攒到下一帧一起画，画完告诉 termd（它据此发下一帧） */
  push(list) {
    for (const u of list) this.queue.push(u);
    if (!this.raf) this.raf = requestAnimationFrame(() => this.flush());
  }

  flush() {
    this.raf = 0;
    const list = this.queue;
    this.queue = [];
    const screens = list.filter((u) => u.t === 'screen');
    let screenDone = false;
    for (const u of list) {
      switch (u.t) {
        case 'screen':
          if (!screenDone) {
            this.onScreen(u, screens.length > 1 ? screens[screens.length - 1] : null);
            screenDone = true;
          }
          break;
        case 'scrolled':
          this.onScrolled(u);
          break;
        case 'title':
          this.setTitle(u.v);
          break;
        case 'clipboard':
          this.onClipboard(u.v);
          break;
        case 'cleared':
          this.clearHist(u.epoch);
          break;
        case 'bell':
          break;
        case 'progress':
          this.onProgress?.(u);
          break;
        case 'error':
          console.warn('[looklook] termd:', u.code, u.msg);
          break;
      }
    }
    if (screenDone) {
      this.conn.ack();
      for (const fn of this.renderHooks) {
        try {
          fn();
        } catch (e) {
          console.error(e);
        }
      }
    }
  }

  /**
   * 一帧画面。一批里有好几帧时（很少：termd 等确认才发下一帧）不挪节点，整屏按最新状态重画。
   * @param {object} u 第一帧
   * @param {object|null} latest 同一批里最后一帧（有多帧时）
   */
  onScreen(u, latest) {
    const last = latest || u;
    const resized = last.rows !== this.rowsN || last.cols !== this.cols;
    if (resized) this.setGrid(last.cols, last.rows);
    const full = resized || latest !== null || u.full;
    if (this.frozen) {
      this.frozenDirty = true;
    } else if (full) {
      this.renderRows(null);
    } else {
      for (const [top, bottom, delta] of u.shifts) this.shiftRows(top, bottom, delta);
      this.renderRows(u.dirty);
    }
    const m = this.modes;
    const altChanged = m.alt !== last.alt;
    m.alt = last.alt;
    m.mouse = last.mouse;
    m.appCursor = last.appCursor;
    m.bracketed = last.bracketed;
    m.altScroll = last.altScroll;
    this.cursor = last.cursor;
    if (last.title !== this.title) this.setTitle(last.title);
    if (this.paletteKey !== JSON.stringify(last.palette)) {
      this.paletteKey = JSON.stringify(last.palette);
      this.palette = last.palette.length ? Object.fromEntries(last.palette) : null;
      this.sheet.setTheme(this.theme, this.palette);
    }
    if (altChanged) this.root.classList.toggle('alt', m.alt);
    this.root.classList.toggle('app', this.appMode());
    this.placeCursor();
    this.renderPredictions();
    this.syncHistory(last.sb);
    this.follow();
  }

  setGrid(cols, rows) {
    this.cols = cols;
    this.rowsN = rows;
    while (this.rowEls.length < rows) {
      const el = document.createElement('div');
      el.className = 'r';
      this.live.insertBefore(el, this.cursorEl);
      this.rowEls.push(el);
    }
    while (this.rowEls.length > rows) this.rowEls.pop().remove();
    this.live.style.width = cols * this.cellW + 'px';
  }

  /** 区域 [top, bottom) 整块上移（delta > 0）或下移：挪节点，空出来的行在 dirty 里会重画 */
  shiftRows(top, bottom, delta) {
    const n = Math.min(Math.abs(delta), bottom - top);
    if (!n || bottom > this.rowEls.length) return;
    const els = this.rowEls;
    if (delta > 0) {
      const moved = els.splice(top, n);
      const ref = els[bottom - n] || this.cursorEl;
      for (const el of moved) this.live.insertBefore(el, ref);
      els.splice(bottom - n, 0, ...moved);
    } else {
      const moved = els.splice(bottom - n, n);
      const ref = els[top];
      for (const el of moved) this.live.insertBefore(el, ref);
      els.splice(top, 0, ...moved);
    }
  }

  /** 按 WASM 里的最新状态重画这些行（null = 全部） */
  renderRows(rows) {
    const tc = this.conn.tc;
    const list = rows || this.rowEls.map((_, i) => i);
    for (const i of list) {
      const el = this.rowEls[i];
      if (!el) continue;
      el.innerHTML = runsHtml(JSON.parse(tc.row(i)), this.sheet);
      el.dataset.w = tc.row_wrapped(i) ? '1' : '';
    }
  }

  /** 预测的字（WASM 里的状态）画出来，光标挪到最后一个预测后面 */
  renderPredictions() {
    const p = this.conn.tc?.predictions() || [];
    const key = p.join(',');
    if (key === this.predKey) return;
    this.predKey = key;
    let html = '';
    for (let i = 0; i < p.length; i += 3) {
      const ch = String.fromCharCode(p[i + 2]);
      const esc = ch === '<' ? '&lt;' : ch === '&' ? '&amp;' : ch;
      html += '<span style="transform:translate(' + p[i + 1] * this.cellW + 'px,' + p[i] * this.lineH + 'px)">' + esc + '</span>';
    }
    this.predEl.innerHTML = html;
    this.placeCursor();
  }

  /** 光标位置：有预测时在最后一个预测后面 */
  cursorPos() {
    const pc = this.conn.tc?.predict_cursor();
    return pc && pc.length ? { r: pc[0], c: pc[1] } : this.cursor;
  }

  placeCursor() {
    const c = { ...this.cursor, ...this.cursorPos() };
    const el = this.cursorEl;
    if (!c.v || !this.rowsN) {
      el.style.display = 'none';
      return;
    }
    el.style.display = '';
    el.className = 'lld-cur ' + c.s + (c.blink ? ' blink' : '');
    // 光标那一格的实际位置：前面有宽字、非 ASCII 盒子时按格子数算正好对得上（都是定宽）
    el.style.transform = 'translate(' + c.c * this.cellW + 'px,' + c.r * this.lineH + 'px)';
    this.placeTextarea();
  }

  /** 输入框放在光标处：输入法的候选框跟着光标走 */
  placeTextarea() {
    const r = this.live.getBoundingClientRect();
    const pos = this.cursorPos();
    const x = r.left + pos.c * this.cellW;
    const y = r.top + pos.r * this.lineH;
    this.ta.style.transform = 'translate(' + Math.max(0, Math.round(x)) + 'px,' + Math.max(0, Math.round(y)) + 'px)';
  }

  setTitle(t) {
    this.title = t;
    this.onTitle?.(t);
  }

  onClipboard(text) {
    const fn = this.oscHandlers.get(52);
    if (!fn) return;
    // term-inject.js 的 OSC 52 处理：`c;<base64>`
    const b64 = btoa(String.fromCharCode(...new TextEncoder().encode(text)));
    fn('c;' + b64);
  }

  // ---------- 滚动 ----------

  /** 备用屏幕或程序开了鼠标：滑动、点按交给程序 */
  appMode() {
    return this.modes.alt || this.modes.mouse !== 'none';
  }

  atBottom() {
    const r = this.root;
    return r.scrollTop + r.clientHeight >= r.scrollHeight - this.lineH / 2;
  }

  onScroll() {
    if (this.programScroll) {
      this.programScroll = false;
      return;
    }
    this.stick = this.atBottom();
    this.pill.classList.toggle('on', !this.stick && !this.appMode());
    if (this.root.scrollTop < this.root.clientHeight * 2) this.loadOlder();
  }

  follow() {
    if (this.keyboardInset > 0 || this.appMode()) return this.keepCursorVisible();
    if (this.stick) this.scrollToBottom();
  }

  scrollToBottom() {
    const r = this.root;
    const top = r.scrollHeight - r.clientHeight;
    if (Math.abs(r.scrollTop - top) > 0.5) {
      this.programScroll = true;
      r.scrollTop = top;
    }
  }

  toBottom() {
    this.stick = true;
    this.pill.classList.remove('on');
    this.scrollToBottom();
  }

  /** 光标那一行在容器里看得见（键盘弹出、备用屏幕里屏幕比容器高时） */
  keepCursorVisible() {
    const r = this.root;
    const liveTop = this.live.offsetTop;
    const y = liveTop + this.cursor.r * this.lineH;
    if (y + this.lineH > r.scrollTop + r.clientHeight) {
      this.programScroll = true;
      r.scrollTop = y + this.lineH - r.clientHeight + PAD;
    } else if (y < r.scrollTop) {
      this.programScroll = true;
      r.scrollTop = Math.max(0, y - PAD);
    } else if (this.stick && !this.appMode()) {
      this.scrollToBottom();
    }
  }

  // ---------- 回滚区 ----------

  clearHist(epoch) {
    this.hist.textContent = '';
    this.buckets.clear();
    this.bucketKeys = [];
    this.histCount = 0;
    this.minSeq = Infinity;
    this.maxSeq = -1;
    this.epoch = epoch;
  }

  rowEl(row) {
    const el = document.createElement('div');
    el.className = 'r';
    el.dataset.seq = row.seq;
    if (row.w) el.dataset.w = '1';
    el.innerHTML = runsHtml(row.r, this.sheet);
    return el;
  }

  bucket(key) {
    let b = this.buckets.get(key);
    if (b) return b;
    b = document.createElement('div');
    b.className = 'b';
    this.buckets.set(key, b);
    // 按块号插到对的位置
    let i = this.bucketKeys.length;
    while (i > 0 && this.bucketKeys[i - 1] > key) i--;
    this.bucketKeys.splice(i, 0, key);
    const next = this.bucketKeys[i + 1];
    this.hist.insertBefore(b, next === undefined ? null : this.buckets.get(next));
    return b;
  }

  /** 加一批回滚行（任意顺序、可能和已有的重复） */
  addHistRows(rows) {
    for (const row of rows) {
      if (row.seq >= this.minSeq && row.seq <= this.maxSeq && this.hasSeq(row.seq)) continue;
      const b = this.bucket(Math.floor(row.seq / BUCKET));
      const el = this.rowEl(row);
      let ref = null;
      for (const c of b.children) {
        if (Number(c.dataset.seq) > row.seq) {
          ref = c;
          break;
        }
      }
      b.insertBefore(el, ref);
      this.histCount++;
      if (row.seq < this.minSeq) this.minSeq = row.seq;
      if (row.seq > this.maxSeq) this.maxSeq = row.seq;
    }
    // 太多了丢掉最早的块
    while (this.histCount > MAX_HIST && this.bucketKeys.length > 1) {
      const k = this.bucketKeys.shift();
      const b = this.buckets.get(k);
      this.histCount -= b.children.length;
      b.remove();
      this.buckets.delete(k);
      const first = this.buckets.get(this.bucketKeys[0]);
      this.minSeq = first?.firstElementChild ? Number(first.firstElementChild.dataset.seq) : Infinity;
    }
  }

  hasSeq(seq) {
    const b = this.buckets.get(Math.floor(seq / BUCKET));
    if (!b) return false;
    for (const c of b.children) if (Number(c.dataset.seq) === seq) return true;
    return false;
  }

  onScrolled(u) {
    if (this.epoch !== u.epoch) this.clearHist(u.epoch);
    if (this.frozen) {
      this.frozenScrolled.push(u);
      return;
    }
    this.addHistRows(u.rows);
  }

  /** 每帧带着 termd 回滚区的范围：缺的（刚连上、断线期间滚出去的）补上 */
  syncHistory(sb) {
    const [epoch, oldest, next] = sb;
    this.sbInfo = sb;
    if (this.epoch !== epoch) this.clearHist(epoch);
    if (this.filling || next <= oldest) return;
    const have = this.maxSeq + 1;
    let start;
    if (this.histCount === 0) start = Math.max(oldest, next - FIRST_HISTORY);
    else if (have < next) start = Math.max(oldest, have, next - FIRST_HISTORY);
    else return;
    if (this.histCount && start > have) this.clearHist(epoch); // 缺得太多：从头来
    this.fetchHistory(epoch, start, next - start);
  }

  async fetchHistory(epoch, start, count) {
    this.filling = true;
    try {
      for (let s = start; s < start + count; s += OLDER_BATCH) {
        const res = await this.conn.history(epoch, s, Math.min(OLDER_BATCH, start + count - s));
        if (!res || res.mismatch || res.epoch !== this.epoch) break;
        this.addRowsKeepingView(res.rows);
      }
    } finally {
      this.filling = false;
    }
  }

  /** 往上翻到快到头时取更早的 */
  async loadOlder() {
    if (this.loadingOlder || this.appMode() || !this.histCount) return;
    const [epoch, oldest] = this.sbInfo;
    if (this.minSeq <= oldest || epoch !== this.epoch) return;
    this.loadingOlder = true;
    try {
      const start = Math.max(oldest, this.minSeq - OLDER_BATCH);
      const res = await this.conn.history(epoch, start, this.minSeq - start);
      if (res && !res.mismatch && res.epoch === this.epoch) this.addRowsKeepingView(res.rows);
    } finally {
      this.loadingOlder = false;
    }
  }

  addRowsKeepingView(rows) {
    if (this.frozen) {
      this.frozenScrolled.push({ epoch: this.epoch, rows });
      return;
    }
    // 在最下面时跟着走；翻看中插在上面的行由浏览器的滚动锚定保持位置不跳
    this.addHistRows(rows);
    if (this.stick && !this.appMode()) this.scrollToBottom();
  }

  // ---------- 选字 ----------

  selectionInside() {
    const sel = document.getSelection();
    return Boolean(sel && !sel.isCollapsed && sel.rangeCount && this.root.contains(sel.anchorNode));
  }

  onSelection() {
    const inside = this.selectionInside();
    if (inside === this.frozen) return;
    this.frozen = inside;
    if (inside) return;
    // 选区没了：攒着的一次补上
    const scrolled = this.frozenScrolled;
    this.frozenScrolled = [];
    for (const u of scrolled) if (u.epoch === this.epoch) this.addHistRows(u.rows);
    if (this.frozenDirty) {
      this.frozenDirty = false;
      this.renderRows(null);
    }
    this.follow();
  }

  /** 选中的文字：行尾空白去掉，自动折行的行接回一行 */
  selectionText() {
    const sel = document.getSelection();
    if (!sel || sel.isCollapsed || !sel.rangeCount) return '';
    const range = sel.getRangeAt(0);
    if (!this.root.contains(range.commonAncestorContainer)) return '';
    const rows = [...this.root.querySelectorAll('.r')].filter((r) => range.intersectsNode(r));
    if (!rows.length) return sel.toString();
    let out = '';
    rows.forEach((row, i) => {
      const r = document.createRange();
      r.selectNodeContents(row);
      if (i === 0 && row.contains(range.startContainer)) r.setStart(range.startContainer, range.startOffset);
      if (i === rows.length - 1 && row.contains(range.endContainer)) r.setEnd(range.endContainer, range.endOffset);
      const text = r.toString();
      const prevWrapped = i > 0 && rows[i - 1].dataset.w === '1';
      out += (i === 0 ? '' : prevWrapped ? '' : '\n') + (row.dataset.w === '1' ? text : text.replace(/[ \t]+$/, ''));
    });
    return out;
  }

  onCopy(e) {
    const text = this.selectionText();
    if (!text) return;
    e.preventDefault();
    e.clipboardData?.setData('text/plain', text);
  }

  // ---------- 输入 ----------

  sendTyped(text) {
    if (this.disableStdin || !text) return;
    const mods = window.__llApplyMods;
    this.conn.input(typeof mods === 'function' ? mods(text) : text, true);
    this.toBottomOnInput();
  }

  sendRaw(data) {
    if (this.disableStdin) return;
    this.conn.input(data);
    this.toBottomOnInput();
  }

  toBottomOnInput() {
    if (!this.stick && !this.appMode()) this.toBottom();
  }

  paste(text) {
    if (!text) return;
    this.sendRaw(pasteBytes(text, this.modes.bracketed));
  }

  setupInput() {
    attachTextInput(this.ta, {
      modes: () => this.modes,
      sendRaw: (d) => this.sendRaw(d),
      sendTyped: (t) => this.sendTyped(t),
      paste: (t) => this.paste(t),
    });
  }

  focus() {
    this.ta.focus({ preventScroll: true });
  }

  // ---------- 指针（点按、滑动） ----------

  cellAt(x, y) {
    const r = this.live.getBoundingClientRect();
    const col = Math.max(0, Math.min(this.cols - 1, Math.floor((x - r.left) / this.cellW)));
    const row = Math.max(0, Math.min(this.rowsN - 1, Math.floor((y - r.top) / this.lineH)));
    return { col, row };
  }

  /** 滚轮一格：开了鼠标就上报给程序，备用屏幕里没开鼠标就发 ↑ / ↓ */
  wheelStep(up, x, y) {
    if (this.modes.mouse !== 'none') {
      const { col, row } = this.cellAt(x, y);
      this.conn.mouse(up ? 4 : 5, 0, col, row);
    } else if (this.modes.alt && this.modes.altScroll) {
      const k = up ? 'A' : 'B';
      this.conn.input((this.modes.appCursor ? '\x1bO' : '\x1b[') + k);
    }
  }

  setupPointer() {
    const root = this.root;
    let start = null, acc = 0, lastY = 0, lastT = 0, vel = 0, fling = 0, moved = 0;
    const step = () => this.lineH * 1.5;
    const drain = (x, y) => {
      const s = step();
      while (Math.abs(acc) >= s) {
        // 手指往下 = 往回看（滚轮上滚）
        this.wheelStep(acc > 0, x, y);
        acc -= Math.sign(acc) * s;
      }
    };
    root.addEventListener('touchstart', (e) => {
      cancelAnimationFrame(fling);
      if (e.touches.length !== 1) {
        start = null;
        return;
      }
      const t = e.touches[0];
      start = { x: t.clientX, y: t.clientY, at: Date.now(), app: this.appMode() };
      lastY = t.clientY;
      lastT = performance.now();
      vel = 0;
      acc = 0;
      moved = 0;
    }, { passive: true });
    root.addEventListener('touchmove', (e) => {
      if (!start || e.touches.length !== 1) return;
      const t = e.touches[0];
      moved = Math.max(moved, Math.abs(t.clientX - start.x) + Math.abs(t.clientY - start.y));
      if (!start.app) return; // 普通屏幕：浏览器原生滚动
      e.preventDefault();
      const now = performance.now();
      const dy = t.clientY - lastY;
      vel = dy / Math.max(1, now - lastT);
      lastY = t.clientY;
      lastT = now;
      acc += dy;
      drain(start.x, start.y);
    }, { passive: false });
    root.addEventListener('touchend', (e) => {
      if (!start) return;
      const s = start;
      start = null;
      if (s.app && moved > 10 && Math.abs(vel) > 0.3) {
        // 松手后带惯性再滚一会儿
        let v = vel * 16;
        const tick = () => {
          v *= 0.92;
          if (Math.abs(v) < 0.5) return;
          acc += v;
          drain(s.x, s.y);
          fling = requestAnimationFrame(tick);
        };
        fling = requestAnimationFrame(tick);
        return;
      }
      if (moved > 10 || Date.now() - s.at > 450) return; // 滑动、长按（选字）不算点按
      this.tap(s.x, s.y, e);
    });
    // 电脑上（?renderer=dom）：滚轮、点击
    root.addEventListener('wheel', (e) => {
      if (!this.appMode()) return;
      e.preventDefault();
      acc -= e.deltaMode === 1 ? e.deltaY * this.lineH : e.deltaY;
      drain(e.clientX, e.clientY);
    }, { passive: false });
    root.addEventListener('mousedown', (e) => {
      if (this.touch || e.button !== 0) return;
      if (this.modes.mouse !== 'none' && !e.shiftKey) {
        e.preventDefault();
        const { col, row } = this.cellAt(e.clientX, e.clientY);
        this.conn.mouse(0, 0, col, row);
        const up = (ev) => {
          const c = this.cellAt(ev.clientX, ev.clientY);
          this.conn.mouse(1, 0, c.col, c.row);
          window.removeEventListener('mouseup', up);
        };
        window.addEventListener('mouseup', up);
      }
    });
    root.addEventListener('click', (e) => {
      if (this.touch) return;
      const link = e.target.closest?.('.lk');
      if (link && !this.selectionInside()) {
        window.open(link.dataset.l, '_blank');
        return;
      }
      if (!this.selectionInside()) this.focus();
    });
  }

  /** 点一下：链接 → 打开（term-inject.js 弹“打开 / 复制链接”）；程序开了鼠标 → 一次鼠标点击。不弹键盘 */
  tap(x, y, e) {
    const target = document.elementFromPoint(x, y);
    const link = target?.closest?.('.lk');
    if (link) {
      e.preventDefault();
      window.open(link.dataset.l, '_blank');
      return;
    }
    if (this.modes.mouse !== 'none' && this.live.contains(target)) {
      e.preventDefault();
      const { col, row } = this.cellAt(x, y);
      this.conn.mouse(0, 0, col, row);
      this.conn.mouse(1, 0, col, row);
    }
  }

  // ---------- 给兼容层（window.term）用 ----------

  /** 回滚区 + 屏幕，第 i 行的文字和是否接着上一行 */
  bufferLine(i) {
    if (i < this.histCount) {
      let n = i;
      for (const k of this.bucketKeys) {
        const b = this.buckets.get(k);
        if (n < b.children.length) {
          const el = b.children[n];
          const prev = el.previousElementSibling || this.buckets.get(this.bucketKeys[this.bucketKeys.indexOf(k) - 1])?.lastElementChild;
          return { text: el.textContent, wrappedFromPrev: prev?.dataset.w === '1' };
        }
        n -= b.children.length;
      }
      return null;
    }
    const r = i - this.histCount;
    if (r >= this.rowsN) return null;
    const tc = this.conn.tc;
    const prevWrapped = r > 0 ? tc.row_wrapped(r - 1) : this.hist.lastElementChild?.lastElementChild?.dataset.w === '1';
    return { text: tc.row_text(r), wrappedFromPrev: Boolean(prevWrapped) };
  }
}
