// 手机、平板（触屏为主的设备）上的终端：在画出来的终端（WebGL / DOM 绘制）上面盖一层真正的网页文字，
// 让翻看和选择文字跟普通网页一样。电脑上不加载（见 main.js）。
//
// 为什么这样做：xterm.js 在手机上选不了字（它的选择靠鼠标事件，没有选择把手、系统菜单）；
// 终端又跑在 tmux 里（备用屏幕、开了鼠标），xterm 自己的回滚区是空的，以前滑动只能变成滚轮事件发给 tmux，
// 每一下都要经网络往返、tmux 一次滚 5 行再整屏重画发回来——不跟手、一顿一顿，也没有惯性。
// xterm.js 本身没有现成的方案：DOM 绘制层的文字被设成不可选（user-select: none），它的无障碍层（screenReaderMode）
// 只有屏幕上那几行、不能滚动，也不按格子对齐。所以这里自己做一层“透明文字层”：
//
// - 文字层 = tmux 回滚区（从网关 /i/{id}/ll-history 取，带颜色）+ 当前屏幕（从 xterm 的缓冲区读），
//   一行一行和终端的格子对齐，是浏览器自己的滚动容器。平时停在最下面、文字透明，看到的还是下面画出来的终端；
// - 往上滑就是浏览器原生滚动（合成线程上跟着手指走、自带惯性），一离开最下面文字层就显示出来（不透明，是一份快照），
//   回到最下面又变回透明、露出实时的终端。离开时右下角有“回到底部”按钮；
// - 选择文字全用浏览器自己的：长按出选择把手和系统菜单（复制、全选、分享），选区跟着滚动；
//   长按选中按住的那一整行（保留以前“长按选行”的意思，把手拖动可以扩大缩小）；点两下选词、点三下选行，
//   这两种是脚本选的，系统不一定给菜单，旁边会出一个“复制”按钮。有选区时文字层显示出来、停在那一刻，
//   免得终端还在输出、画面变了，选中的和看到的对不上；
// - 程序在备用屏幕里（vim、less、htop）或自己开了鼠标时，回滚区不是程序的内容：滑动照旧变成滚轮事件交给程序
//   （和以前一样）；程序没开鼠标时滚轮它收不到，改发 ↑ / ↓。文字层只有当前屏幕；
// - 点一下（不动、不长按）照旧交给终端（tmux 里是一次鼠标点击、点链接），但不让终端的输入框聚焦——
//   手机上只有按键条的键盘按钮才弹输入法（term-inject.js 的 setupIme 也拦着）。
//
// 没有 tmux（网关取不到回滚区）时，回滚区用 xterm 自己的（普通屏幕的回滚行）。

const MEDIA = '(hover: none) and (pointer: coarse)';
const HISTORY_LINES = 2000;
const L = {
  'zh-CN': { copy: '复制', copied: '已复制', copy_failed: '浏览器不允许复制', bottom: '回到底部' },
  'en-US': { copy: 'Copy', copied: 'Copied', copy_failed: 'The browser blocked copying', bottom: 'Back to bottom' },
};

export function isTouchDevice() {
  return Boolean(window.matchMedia && window.matchMedia(MEDIA).matches);
}

function tr(k) {
  let lang = 'zh-CN';
  try {
    lang = localStorage.getItem('looklook.lang') || navigator.language || 'zh-CN';
  } catch {
    lang = navigator.language || 'zh-CN';
  }
  const t = /^zh/i.test(lang) ? L['zh-CN'] : L['en-US'];
  return t[k];
}

// ---------- 颜色 ----------

const NAMES = ['black', 'red', 'green', 'yellow', 'blue', 'magenta', 'cyan', 'white',
  'brightBlack', 'brightRed', 'brightGreen', 'brightYellow', 'brightBlue', 'brightMagenta', 'brightCyan', 'brightWhite'];
// xterm.js 的默认 16 色
const DEFAULT16 = ['#2e3436', '#cc0000', '#4e9a06', '#c4a000', '#3465a4', '#75507b', '#06989a', '#d3d7cf',
  '#555753', '#ef2929', '#8ae234', '#fce94f', '#729fcf', '#ad7fa8', '#34e2e2', '#eeeeec'];

function hex(n) {
  return n.toString(16).padStart(2, '0');
}

function palette(theme, i) {
  if (i < 16) return (theme && theme[NAMES[i]]) || DEFAULT16[i];
  if (i < 232) {
    const v = i - 16;
    const c = (x) => (x ? x * 40 + 55 : 0);
    return '#' + hex(c(Math.floor(v / 36))) + hex(c(Math.floor(v / 6) % 6)) + hex(c(v % 6));
  }
  const g = (i - 232) * 10 + 8;
  return '#' + hex(g) + hex(g) + hex(g);
}

/** 一段文字的样式 → CSS（默认样式为空串） */
function styleCss(s, theme) {
  if (!s) return '';
  let fg = s.fg, bg = s.bg;
  if (s.inverse) {
    const dfg = (theme && theme.foreground) || '#ffffff';
    const dbg = (theme && theme.background) || '#000000';
    [fg, bg] = [bg || dbg, fg || dfg];
  }
  let css = '';
  if (fg) css += 'color:' + fg + ';';
  if (bg) css += 'background:' + bg + ';';
  if (s.bold) css += 'font-weight:bold;';
  if (s.italic) css += 'font-style:italic;';
  if (s.dim) css += 'opacity:.6;';
  const deco = (s.underline ? 'underline ' : '') + (s.strike ? 'line-through' : '');
  if (deco) css += 'text-decoration:' + deco.trim() + ';';
  if (s.invisible) css += 'visibility:hidden;';
  return css;
}

function escapeHtml(t) {
  return t.replace(/[&<>]/g, (c) => (c === '&' ? '&amp;' : c === '<' ? '&lt;' : '&gt;'));
}

// ---------- 字符宽度 ----------

function makeWidth(term) {
  const svc = term._core && term._core.unicodeService;
  if (svc && typeof svc.wcwidth === 'function') return (cp) => svc.wcwidth(cp);
  return (cp) =>
    cp >= 0x1100 && (cp <= 0x115f || (cp >= 0x2e80 && cp <= 0xa4cf) || (cp >= 0xac00 && cp <= 0xd7a3) || (cp >= 0xf900 && cp <= 0xfaff) ||
      (cp >= 0xfe30 && cp <= 0xfe4f) || (cp >= 0xff00 && cp <= 0xff60) || (cp >= 0xffe0 && cp <= 0xffe6) || (cp >= 0x1f300 && cp <= 0x1faff) ||
      (cp >= 0x20000 && cp <= 0x3fffd)) ? 2 : 1;
}

/**
 * 一行 → HTML。runs：[{ text, style, wide }]，wide 的字占两格，包在定宽的 inline-block 里，
 * 后面的字才和画出来的终端对得上（手机字体里中文不一定正好两个英文字宽）。
 */
function runsHtml(runs, theme) {
  let out = '';
  for (const r of runs) {
    const css = styleCss(r.style, theme);
    const body = r.wide ? '<i class="w">' + escapeHtml(r.text) + '</i>' : escapeHtml(r.text);
    out += css ? '<span style="' + css + '">' + body + '</span>' : body;
  }
  return out;
}

function pushRun(runs, text, style, wide) {
  const last = runs[runs.length - 1];
  if (last && !wide && !last.wide && last.style === style) last.text += text;
  else runs.push({ text, style, wide });
}

/** 去掉行尾默认样式的空白（复制时不带一串空格） */
function trimRuns(runs) {
  while (runs.length) {
    const last = runs[runs.length - 1];
    if (last.wide || (last.style && (last.style.bg || last.style.inverse))) break;
    const t = last.text.replace(/\s+$/, '');
    if (t) {
      last.text = t;
      break;
    }
    runs.pop();
  }
  return runs;
}

// xterm 缓冲区的一行
const styleCache = new Map();
function cellStyle(cell, theme) {
  if (cell.isAttributeDefault()) return null;
  const color = (rgb, pal, get) => (rgb ? '#' + get().toString(16).padStart(6, '0') : pal ? palette(theme, get()) : null);
  const s = {
    fg: color(cell.isFgRGB(), cell.isFgPalette(), () => cell.getFgColor()),
    bg: color(cell.isBgRGB(), cell.isBgPalette(), () => cell.getBgColor()),
    bold: Boolean(cell.isBold()), italic: Boolean(cell.isItalic()), dim: Boolean(cell.isDim()), underline: Boolean(cell.isUnderline()),
    inverse: Boolean(cell.isInverse()), invisible: Boolean(cell.isInvisible()), strike: Boolean(cell.isStrikethrough()),
  };
  const key = JSON.stringify(s);
  let v = styleCache.get(key);
  if (!v) {
    if (styleCache.size > 500) styleCache.clear();
    styleCache.set(key, (v = s));
  }
  return v;
}

function bufferRuns(line, cols, cell, theme) {
  const runs = [];
  if (!line) return runs;
  for (let x = 0; x < cols; x++) {
    line.getCell(x, cell);
    const w = cell.getWidth();
    if (w === 0) continue; // 宽字的后半格
    const ch = cell.getChars() || ' ';
    pushRun(runs, ch, cellStyle(cell, theme), w === 2);
  }
  return trimRuns(runs);
}

// tmux capture-pane -e 的一行（带 SGR 转义）
function sgrRuns(text, theme, width) {
  const runs = [];
  let st = {};
  let style = null;
  const fix = () => {
    const keys = Object.keys(st).filter((k) => st[k]);
    if (!keys.length) return (style = null);
    const key = JSON.stringify(st);
    style = styleCache.get(key);
    if (!style) styleCache.set(key, (style = { ...st }));
    return style;
  };
  let i = 0;
  let plain = '';
  const flush = () => {
    if (!plain) return;
    for (const ch of plain) {
      const cp = ch.codePointAt(0);
      pushRun(runs, ch, style, width(cp) === 2);
    }
    plain = '';
  };
  while (i < text.length) {
    const c = text.charCodeAt(i);
    if (c !== 0x1b) {
      const n = text.indexOf('\x1b', i);
      plain += n < 0 ? text.slice(i) : text.slice(i, n);
      i = n < 0 ? text.length : n;
      continue;
    }
    flush();
    const next = text[i + 1];
    if (next === '[') {
      let j = i + 2;
      while (j < text.length && !/[@-~]/.test(text[j])) j++;
      const fin = text[j];
      const params = text.slice(i + 2, j);
      i = j + 1;
      if (fin !== 'm') continue;
      const p = params.split(/[;:]/).map((v) => (v === '' ? 0 : Number(v)));
      if (!p.length) p.push(0);
      for (let k = 0; k < p.length; k++) {
        const v = p[k];
        if (v === 0) st = {};
        else if (v === 1) st.bold = true;
        else if (v === 2) st.dim = true;
        else if (v === 3) st.italic = true;
        else if (v === 4) st.underline = true;
        else if (v === 7) st.inverse = true;
        else if (v === 8) st.invisible = true;
        else if (v === 9) st.strike = true;
        else if (v === 21 || v === 24) st.underline = false;
        else if (v === 22) st.bold = st.dim = false;
        else if (v === 23) st.italic = false;
        else if (v === 27) st.inverse = false;
        else if (v === 28) st.invisible = false;
        else if (v === 29) st.strike = false;
        else if (v >= 30 && v <= 37) st.fg = palette(theme, v - 30);
        else if (v >= 90 && v <= 97) st.fg = palette(theme, v - 90 + 8);
        else if (v >= 40 && v <= 47) st.bg = palette(theme, v - 40);
        else if (v >= 100 && v <= 107) st.bg = palette(theme, v - 100 + 8);
        else if (v === 39) st.fg = null;
        else if (v === 49) st.bg = null;
        else if (v === 38 || v === 48) {
          let col = null;
          if (p[k + 1] === 5) {
            col = palette(theme, p[k + 2] | 0);
            k += 2;
          } else if (p[k + 1] === 2) {
            // 38;2;r;g;b，冒号写法里可能多一个色彩空间参数（38:2::r:g:b）
            const rest = p.slice(k + 2, k + 6);
            const rgb = params.includes(':') && rest.length === 4 ? rest.slice(1) : rest.slice(0, 3);
            col = '#' + rgb.map((x) => hex(Math.max(0, Math.min(255, x | 0)))).join('');
            k += params.includes(':') && rest.length === 4 ? 5 : 4;
          }
          if (v === 38) st.fg = col;
          else st.bg = col;
        }
      }
      fix();
    } else if (next === ']') {
      // OSC（超链接等）：跳过到 BEL 或 ST
      let j = i + 2;
      while (j < text.length && text[j] !== '\x07' && !(text[j] === '\x1b' && text[j + 1] === '\\')) j++;
      i = text[j] === '\x07' ? j + 1 : j + 2;
    } else {
      i += 2;
    }
  }
  flush();
  return trimRuns(runs);
}

// ---------- 文字层 ----------

export function setupTouch(term, parent, historyUrl) {
  if (!isTouchDevice()) return null;
  const screen = term.element && term.element.querySelector('.xterm-screen');
  if (!screen) return null;
  const width = makeWidth(term);

  const layer = document.createElement('div');
  layer.className = 'll-touch';
  const pre = document.createElement('pre');
  // 回滚区（若干块）和屏幕各是一个定高的块：屏幕那几行变了只重排屏幕这一块，不连带几千行回滚区一起重排；
  // 定高还让末尾的空行照样占一行（pre 里块末尾的换行不画出空行）。块与块之间复制时浏览器自己补换行。
  const hist = document.createElement('div');
  const live = document.createElement('div');
  live.className = 'live';
  pre.append(hist, live);
  layer.append(pre);
  const pill = document.createElement('button');
  pill.type = 'button';
  pill.className = 'll-touch-bottom';
  pill.setAttribute('aria-label', tr('bottom'));
  pill.innerHTML = '<svg viewBox="0 0 24 24" width="18" height="18" aria-hidden="true"><path d="M12 5v14M5 12l7 7 7-7" fill="none" stroke="currentColor" stroke-width="2.2" stroke-linecap="round" stroke-linejoin="round"/></svg>';
  const chip = document.createElement('button');
  chip.type = 'button';
  chip.className = 'll-touch-copy';
  chip.textContent = tr('copy');
  parent.append(layer, pill, chip);

  let cellW = 8, rowH = 16;
  let mode = 'tmux'; // tmux：本地翻看；app：备用屏幕 / 程序开了鼠标，滑动交给程序；local：没有 tmux，用 xterm 自己的回滚区
  // 回滚区按取回来的批次分块（<span>），新进回滚区的行接在最后一块后面，太多时丢掉最早的块：
  // 每次只画新增的几行，按下手指时重取不卡（整段 2000 行带颜色地重画在手机上要好几百毫秒）。
  let chunks = []; // [{ el, raw: [行], local }]
  let histCount = 0;
  let srv = null; // 服务端上次的 { size, width }，下次只要这之后的
  let histStale = true; // 终端有新输出，回滚区要重取
  let pending = []; // 离开最下面时取回来的，回到最下面再接上（翻看中不动用户看着的内容）
  let away = false; // 离开了最下面（翻看中）
  let selecting = false; // 文字层里有选区
  let liveRows = [];

  // ---- 尺寸、位置 ----
  function measure() {
    const d = term._core && term._core._renderService && term._core._renderService.dimensions;
    const cw = d && d.css && d.css.cell && d.css.cell.width;
    const ch = d && d.css && d.css.cell && d.css.cell.height;
    cellW = cw || screen.clientWidth / (term.cols || 80) || 8;
    rowH = ch || screen.clientHeight / (term.rows || 24) || 16;
    const pr = parent.getBoundingClientRect();
    const sr = screen.getBoundingClientRect();
    layer.style.left = sr.left - pr.left + 'px';
    layer.style.top = sr.top - pr.top + 'px';
    layer.style.width = sr.width + 'px';
    layer.style.height = rowH * term.rows + 'px';
    const o = term.options;
    layer.style.fontFamily = o.fontFamily;
    layer.style.fontSize = o.fontSize + 'px';
    layer.style.setProperty('--rh', rowH + 'px');
    layer.style.setProperty('--cw', cellW + 'px');
    // 字间距：让一个字正好一格宽
    const probe = document.createElement('span');
    probe.textContent = 'W'.repeat(50);
    probe.style.cssText = 'position:absolute;visibility:hidden;white-space:pre;letter-spacing:0;font-family:inherit;font-size:inherit';
    layer.append(probe);
    const one = probe.getBoundingClientRect().width / 50;
    probe.remove();
    layer.style.letterSpacing = (one ? cellW - one : 0) + 'px';
    applyTheme();
  }
  let lastTheme = null;
  function applyTheme() {
    const th = term.options.theme || {};
    if (th !== lastTheme) {
      // 换了配色：颜色是按配色算好写进去的，重画
      const first = lastTheme === null;
      lastTheme = th;
      rowHtml = rowHtml.map(() => null);
      if (!first && chunks.length) rerenderHist();
    }
    layer.style.setProperty('--bg', th.background || '#000');
    layer.style.setProperty('--fg', th.foreground || '#fff');
    layer.style.setProperty('--sel', th.selectionBackground || 'rgba(120,140,255,.45)');
  }

  // ---- 内容 ----
  function chunkHtml(c) {
    const theme = term.options.theme;
    return c.raw.map((r) => runsHtml(c.local ? r : sgrRuns(r, theme, width), theme)).join('\n');
  }
  function makeChunk(raw, local) {
    const c = { el: document.createElement('div'), raw, local };
    c.el.className = 'c';
    c.el.style.height = 'calc(var(--rh) * ' + raw.length + ')';
    c.el.innerHTML = chunkHtml(c);
    return c;
  }
  function addChunk(raw, local) {
    if (!raw.length) return;
    const c = makeChunk(raw, local);
    hist.append(c.el);
    chunks.push(c);
    histCount += raw.length;
    // 最多留 HISTORY_LINES 行再多一块
    while (chunks.length > 1 && histCount - chunks[0].raw.length >= HISTORY_LINES) {
      const old = chunks.shift();
      old.el.remove();
      histCount -= old.raw.length;
    }
  }
  // 整段（第一次打开、宽度变了）：先画最新的 200 行，更早的分几次空闲时插到上面，打开终端时不卡上几百毫秒
  let fillGen = 0;
  function fillHist(raw, local) {
    clearHist();
    const gen = fillGen;
    const SLICE = 200;
    let end = raw.length;
    addChunk(raw.slice(Math.max(0, end - SLICE)), local);
    end -= SLICE;
    const step = () => {
      if (gen !== fillGen || end <= 0) return;
      const part = raw.slice(Math.max(0, end - SLICE), end);
      end -= SLICE;
      const c = makeChunk(part, local);
      const before = layer.scrollHeight;
      hist.prepend(c.el);
      chunks.unshift(c);
      histCount += part.length;
      if (away) layer.scrollTop += layer.scrollHeight - before; // 翻看中：上面多出来的不挪动看着的内容
      else toBottom();
      later(step);
    };
    later(step);
  }
  function later(fn) {
    if (window.requestIdleCallback) requestIdleCallback(fn, { timeout: 300 });
    else setTimeout(fn, 16);
  }
  function clearHist() {
    fillGen++;
    hist.textContent = '';
    chunks = [];
    histCount = 0;
  }
  function rerenderHist() {
    for (const c of chunks) c.el.innerHTML = chunkHtml(c);
  }
  // 没有 tmux：回滚区是 xterm 自己的（普通屏幕里，视口上面的行），有新输出后按下手指或停下来时整段重画
  function syncLocalHist() {
    const b = term.buffer.active;
    const cell = b.getNullCell();
    const theme = term.options.theme;
    const rows = [];
    if (b.type === 'normal') for (let y = Math.max(0, b.viewportY - HISTORY_LINES); y < b.viewportY; y++) rows.push(bufferRuns(b.getLine(y), term.cols, cell, theme));
    fillHist(rows, true);
    histStale = false;
  }
  // 屏幕的每一行一个 <span>，之间是换行（复制时空行照样在）。只换内容变了的行：
  // 手指按下时要对一遍，换掉按着的那个节点的话这次触摸后面的事件就到不了文字层（见 touchstart 里的补救）。
  let rowEls = [], rowHtml = [];
  function syncLive() {
    applyTheme();
    const b = term.buffer.active;
    const theme = term.options.theme;
    const cell = b.getNullCell();
    if (rowEls.length !== term.rows) {
      live.textContent = '';
      live.style.height = 'calc(var(--rh) * ' + term.rows + ')';
      rowEls = [];
      rowHtml = [];
      for (let i = 0; i < term.rows; i++) {
        const r = document.createElement('span');
        rowEls.push(r);
        rowHtml.push(null);
        live.append(r);
        if (i < term.rows - 1) live.append('\n');
      }
    }
    const rows = [];
    for (let i = 0; i < term.rows; i++) {
      const runs = bufferRuns(b.getLine(b.viewportY + i), term.cols, cell, theme);
      rows.push(runs);
      const html = runsHtml(runs, theme);
      if (html !== rowHtml[i]) {
        rowHtml[i] = html;
        rowEls[i].innerHTML = html;
      }
    }
    liveRows = rows;
  }
  function toBottom() {
    layer.scrollTop = layer.scrollHeight;
  }

  // ---- 回滚区 ----
  let fetching = null;
  function fetchHistory() {
    if (fetching || !historyUrl) return fetching;
    histStale = false;
    const since = srv && mode === 'tmux' ? '&since=' + srv.size + '&w=' + srv.width : '';
    fetching = fetch(historyUrl + '?n=' + HISTORY_LINES + since, { cache: 'no-store' })
      .then((r) => (r.ok ? r.json() : null))
      .catch(() => null)
      .then((j) => {
        fetching = null;
        if (!j || !Array.isArray(j.lines)) {
          srv = null;
          setMode('local');
          if (!away) {
            syncLocalHist();
            toBottom();
          }
          return;
        }
        arrows = Boolean(j.alt && !j.mouse);
        const was = mode;
        setMode(j.alt || j.mouse ? 'app' : 'tmux');
        if (mode !== 'tmux') {
          srv = null;
          return;
        }
        // 刚从别的模式换回来、或者上次的没收到：这次的“增量”接不上，下次整段取
        const full = j.full || was !== 'tmux' || !srv;
        if (!j.full && full) {
          srv = null;
          histStale = true;
          return;
        }
        srv = { size: j.size, width: j.width };
        const batch = { full, raw: j.lines.map(String) };
        if (away && histCount) {
          if (full) pending = [];
          pending.push(batch);
          return;
        }
        const before = hist.offsetHeight;
        applyBatch(batch);
        if (away) layer.scrollTop += hist.offsetHeight - before; // 第一次取到（之前是空的）：保持看着的位置
        else toBottom();
      });
    return fetching;
  }
  function applyBatch(b) {
    if (b.full) fillHist(b.raw, false);
    else addChunk(b.raw, false);
  }
  function setMode(m) {
    if (m === mode) return;
    mode = m;
    layer.dataset.mode = m;
    pending = [];
    clearHist();
    toBottom();
  }

  // ---- 显示 / 透明 ----
  function setShown() {
    const on = away || selecting;
    layer.classList.toggle('shown', on);
    pill.classList.toggle('on', away);
    if (!on) {
      for (const b of pending) applyBatch(b);
      pending = [];
      syncLive();
      toBottom();
    }
  }
  layer.addEventListener('scroll', () => {
    const max = layer.scrollHeight - layer.clientHeight;
    const now = layer.scrollTop < max - 2;
    if (now === away) return;
    away = now;
    setShown();
  }, { passive: true });
  pill.addEventListener('click', () => {
    layer.scrollTo({ top: layer.scrollHeight, behavior: 'smooth' });
  });

  // ---- 选择 ----
  function selInLayer() {
    const s = document.getSelection();
    return s && s.rangeCount && !s.isCollapsed && layer.contains(s.anchorNode) && layer.contains(s.focusNode) ? s : null;
  }
  let press = null; // 当前按着的手指：{ x, y, t, moved, rowSelected }
  let held = null; // 最近一次按下（长按选中时浏览器可能先发 touchcancel，press 已经清掉）
  let scripted = false; // 选区是点两下 / 三下脚本选的（系统不一定给菜单，显示“复制”按钮）
  document.addEventListener('selectionchange', () => {
    const s = selInLayer();
    const was = selecting;
    selecting = Boolean(s);
    if (selecting !== was) setShown();
    // 长按：浏览器选中了按住的那个词 → 扩成整行（“长按选行”）。只扩一次，之后拖把手随用户。
    const age = held ? Date.now() - held.t : 1e9;
    if (s && held && !held.moved && !held.rowSelected && age > 300 && age < 2500) {
      held.rowSelected = true;
      selectRowAt(held.x, held.y);
      scripted = false;
    }
    if (!s) scripted = false;
    placeChip();
  });
  function textNodesOf(el) {
    const out = [];
    const w = document.createTreeWalker(el, NodeFilter.SHOW_TEXT);
    for (let n = w.nextNode(); n; n = w.nextNode()) out.push(n);
    return out;
  }
  /** 屏幕上一点所在的那一行的文字范围：先找到是哪一块（回滚区的某块或屏幕），块里按行高算第几行，再数换行 */
  function rowRange(y) {
    const blocks = [...chunks.map((c) => c.el), live];
    const el = blocks.find((b) => {
      const r = b.getBoundingClientRect();
      return y >= r.top && y < r.bottom;
    });
    if (!el) return null;
    const row = Math.floor((y - el.getBoundingClientRect().top) / rowH);
    let line = 0;
    let start = null, end = null;
    for (const n of textNodesOf(el)) {
      const t = n.nodeValue;
      for (let i = 0; i <= t.length; i++) {
        if (line === row && !start && i < t.length && t[i] !== '\n') start = [n, i];
        if (line === row && start && (i === t.length || t[i] === '\n')) end = [n, i];
        if (i < t.length && t[i] === '\n') {
          if (line === row) return start && end ? { start, end } : null;
          line++;
        }
      }
    }
    return start && end ? { start, end } : null;
  }
  function selectRowAt(x, y) {
    const r = rowRange(y);
    if (!r) return false;
    document.getSelection().setBaseAndExtent(r.start[0], r.start[1], r.end[0], r.end[1]);
    return true;
  }
  function caretAt(x, y) {
    if (document.caretPositionFromPoint) {
      const p = document.caretPositionFromPoint(x, y);
      return p && { node: p.offsetNode, offset: p.offset };
    }
    if (document.caretRangeFromPoint) {
      const r = document.caretRangeFromPoint(x, y);
      return r && { node: r.startContainer, offset: r.startOffset };
    }
    return null;
  }
  function selectWordAt(x, y) {
    const c = caretAt(x, y);
    if (!c || c.node.nodeType !== 3 || !layer.contains(c.node)) return false;
    const t = c.node.nodeValue;
    const isWord = (ch) => ch && /[^\s\n]/.test(ch) && !/[()[\]{}<>"'`,;|]/.test(ch);
    let a = c.offset, b = c.offset;
    if (!isWord(t[a]) && isWord(t[a - 1])) a = b = a - 1;
    if (!isWord(t[a])) return false;
    while (a > 0 && isWord(t[a - 1])) a--;
    while (b < t.length && isWord(t[b])) b++;
    document.getSelection().setBaseAndExtent(c.node, a, c.node, b);
    return true;
  }
  // 点两下选词后可能还要点第三下选行：按钮等这串点按结束再出来（离得近时浏览器会把第三下吸到按钮上）
  let chipTimer = 0;
  function placeChip() {
    const s = scripted && selInLayer();
    clearTimeout(chipTimer);
    if (!s) {
      chip.classList.remove('on');
      return;
    }
    if (Date.now() - lastTap.t < 460 && !chip.classList.contains('on')) {
      chipTimer = setTimeout(placeChip, 470 - (Date.now() - lastTap.t));
      return;
    }
    const r = s.getRangeAt(0).getBoundingClientRect();
    const pr = parent.getBoundingClientRect();
    chip.classList.add('on');
    const w = chip.offsetWidth, h = chip.offsetHeight;
    const left = Math.max(6, Math.min(r.left - pr.left + r.width / 2 - w / 2, pr.width - w - 6));
    const top = r.top - pr.top - h - 14 > 4 ? r.top - pr.top - h - 14 : r.bottom - pr.top + 14;
    chip.style.left = left + 'px';
    chip.style.top = Math.min(top, pr.height - h - 6) + 'px';
  }
  layer.addEventListener('scroll', () => scripted && placeChip(), { passive: true });
  chip.addEventListener('pointerdown', (e) => e.preventDefault());
  chip.addEventListener('click', () => {
    const s = selInLayer();
    if (!s) return;
    copyText(s.toString());
    s.removeAllRanges();
  });
  function copyText(text) {
    const done = (ok) => toast(tr(ok ? 'copied' : 'copy_failed'));
    const fallback = () => {
      const ta = document.createElement('textarea');
      ta.value = text;
      ta.style.cssText = 'position:fixed;top:-1000px;opacity:0';
      document.body.append(ta);
      ta.select();
      let ok = false;
      try {
        ok = document.execCommand('copy');
      } catch {
        ok = false;
      }
      ta.remove();
      return ok;
    };
    if (navigator.clipboard && navigator.clipboard.writeText) navigator.clipboard.writeText(text).then(() => done(true), () => done(fallback()));
    else done(fallback());
  }
  function toast(text) {
    const el = document.createElement('div');
    el.className = 'll-touch-toast';
    el.textContent = text;
    document.body.append(el);
    setTimeout(() => (el.style.opacity = '0'), 900);
    setTimeout(() => el.remove(), 1300);
  }

  // ---- 点按：交给终端（不弹输入法），点两下选词、三下选行 ----
  let taps = 0, lastTap = { t: 0, x: 0, y: 0 };
  function forwardTap(x, y) {
    // 直接发给画出来的屏幕（xterm 的鼠标上报、链接识别都挂在它和它的外层上）。不用 elementFromPoint 找：
    // 那要先把文字层设成不接收点击，几千个节点重算样式、重排，一下就上百毫秒。
    const target = screen;
    const ta = term.textarea;
    const focus = ta && ta.focus;
    // xterm 按下鼠标时会聚焦输入框 → 手机键盘弹出来。点终端不该弹键盘：这一下里先让聚焦什么也不做。
    if (ta) ta.focus = () => {};
    try {
      const base = { clientX: x, clientY: y, bubbles: true, cancelable: true, view: window, button: 0 };
      target.dispatchEvent(new MouseEvent('mousemove', { ...base, buttons: 0 }));
      target.dispatchEvent(new MouseEvent('mousedown', { ...base, buttons: 1, detail: 1 }));
      target.dispatchEvent(new MouseEvent('mouseup', { ...base, buttons: 0, detail: 1 }));
      target.dispatchEvent(new MouseEvent('click', { ...base, buttons: 0, detail: 1 }));
    } finally {
      if (ta) {
        if (focus === HTMLElement.prototype.focus) delete ta.focus;
        else ta.focus = focus;
      }
    }
  }

  // ---- 手指 ----
  layer.addEventListener('touchstart', (e) => {
    if (e.touches.length !== 1) {
      press = null;
      return;
    }
    const t = e.touches[0];
    held = press = { x: t.clientX, y: t.clientY, t: Date.now(), moved: false, rowSelected: false };
    // 停在最下面、没有选区：先把屏幕那几行对上现在画出来的（长按选字时按到的就是看到的），有新输出就重取回滚区。
    if (!away && !selecting) syncLive();
    if (histStale && mode !== 'local') fetchHistory();
    else if (histStale && mode === 'local' && !away) syncLocalHist();
    if (mode === 'app') appStart(e);
    // 按着的节点刚被换掉了：这次触摸后面的事件只发给它（已经不在页面里），不会冒泡到文字层，直接挂在它身上
    const target = e.target;
    if (target && target !== layer && !layer.contains(target)) {
      const opts = { passive: false };
      const off = () => types.forEach(([t, fn]) => target.removeEventListener(t, fn, opts));
      const types = [['touchmove', onMove], ['touchend', (ev) => { off(); onEnd(ev); }], ['touchcancel', (ev) => { off(); onCancel(ev); }]];
      types.forEach(([t, fn]) => target.addEventListener(t, fn, opts));
    }
  }, { passive: true });
  layer.addEventListener('touchmove', onMove, { passive: true }); // 被动：浏览器原生滚动不用等这里（备用屏幕时靠 touch-action 不让浏览器滚，见 style.css）
  function onMove(e) {
    if (!press || e.touches.length !== 1) return;
    const t = e.touches[0];
    if (Math.abs(t.clientX - press.x) + Math.abs(t.clientY - press.y) > 10) press.moved = true;
    if (mode === 'app') appMove(e);
  }
  layer.addEventListener('touchend', onEnd, { passive: false });
  function onEnd(e) {
    const p = press;
    press = null;
    if (mode === 'app') appEnd(e);
    if (!p || p.moved || Date.now() - p.t > 450 || e.changedTouches.length !== 1) return;
    const { clientX: x, clientY: y } = e.changedTouches[0];
    // 是一次点按
    const now = Date.now();
    taps = now - lastTap.t < 450 && Math.abs(x - lastTap.x) + Math.abs(y - lastTap.y) < 30 ? taps + 1 : 1;
    lastTap = { t: now, x, y };
    if (taps === 2 || taps === 3) {
      if (e.cancelable) e.preventDefault(); // 不让浏览器再补发鼠标事件、也不让它动选区
      scripted = taps === 2 ? selectWordAt(x, y) : selectRowAt(x, y);
      placeChip();
      return;
    }
    if (taps > 3) return;
    if (selInLayer() || away) return; // 有选区时点一下是取消选择；翻看时点一下不发给终端
    if (e.cancelable) e.preventDefault(); // 浏览器补发的鼠标事件落在文字层上没用，由下面自己转给终端
    forwardTap(x, y);
  }
  layer.addEventListener('touchcancel', onCancel, { passive: true });
  function onCancel() {
    press = null;
    if (mode === 'app') appEnd(null);
  }

  // ---- 程序自己处理滚动（备用屏幕、开了鼠标）：滑动变成滚轮事件 ----
  // 和以前一样：开了鼠标上报时每个滚轮事件 tmux 只上报一次，同一时间只让一批在路上（等终端收到输出再发下一批），
  // 不会越积越多；松手后带惯性。
  // arrows：备用屏幕里的程序没开鼠标（less、man、不开鼠标的 vim）：滚轮发给它也没用，按滑过的行数发 ↑ / ↓（桌面终端的做法）。
  let arrows = false;
  let acc = 0, lastY = 0, lastT = 0, vel = 0, fling = 0, scrolling = false, sx = 0, sy = 0, inflight = false, ackTimer = 0;
  function wheel(delta) {
    screen.dispatchEvent(new WheelEvent('wheel', { deltaY: delta, deltaMode: 0, clientX: sx, clientY: sy, bubbles: true, cancelable: true }));
  }
  function pump() {
    if (arrows) {
      const k = Math.trunc(acc / rowH);
      if (!k) return;
      acc -= k * rowH;
      const app = term.modes && term.modes.applicationCursorKeysMode;
      const key = k > 0 ? (app ? '\x1bOB' : '\x1b[B') : app ? '\x1bOA' : '\x1b[A';
      term.input(key.repeat(Math.min(Math.abs(k), 200)), true);
      return;
    }
    if (inflight) return;
    const step = rowH * 2.5;
    const n = Math.trunc(acc / step);
    if (!n) return;
    acc -= n * step;
    inflight = true;
    for (let i = 0; i < Math.abs(n); i++) wheel(n > 0 ? step : -step);
    ackTimer = setTimeout(ack, 400);
  }
  function ack() {
    if (!inflight) return;
    inflight = false;
    clearTimeout(ackTimer);
    pump();
  }
  function appStart(e) {
    cancelAnimationFrame(fling);
    fling = 0;
    acc = 0;
    scrolling = false;
    const t = e.touches[0];
    lastY = t.clientY;
    lastT = e.timeStamp;
    vel = 0;
  }
  function appMove(e) {
    const t = e.touches[0];
    if (!press) return;
    if (!scrolling) {
      if (Math.abs(t.clientY - press.y) < 10 || Math.abs(t.clientY - press.y) < Math.abs(t.clientX - press.x)) return;
      scrolling = true;
      lastY = t.clientY;
      lastT = e.timeStamp;
    }
    sx = t.clientX;
    sy = t.clientY;
    const d = lastY - t.clientY;
    const dt = Math.max(1, e.timeStamp - lastT);
    vel = 0.8 * (d / dt) + 0.2 * vel;
    lastY = t.clientY;
    lastT = e.timeStamp;
    acc += d;
    pump();
  }
  function appEnd(e) {
    if (!scrolling) return;
    scrolling = false;
    let v = e && e.timeStamp - lastT > 80 ? 0 : vel;
    if (Math.abs(v) < 0.3) return;
    let prev = performance.now();
    const step = (now) => {
      const dt = Math.min(48, now - prev);
      prev = now;
      acc += v * dt;
      pump();
      v *= Math.pow(0.95, dt / 16);
      fling = Math.abs(v) > 0.05 ? requestAnimationFrame(step) : 0;
    };
    fling = requestAnimationFrame(step);
  }

  // ---- 跟着终端变化 ----
  let idle = 0;
  term.onWriteParsed(() => {
    histStale = true;
    ack();
    // 停在最下面时文字是透明的，不用每帧跟着画；输出停下来一会儿再对一次（长按时按到的才准），并预先取好回滚区
    clearTimeout(idle);
    idle = setTimeout(() => {
      if (!away && !selecting) syncLive();
      if (away || selecting || document.visibilityState !== 'visible') return;
      if (mode === 'local') syncLocalHist();
      else fetchHistory();
    }, 1500);
  });
  const relayout = () => {
    measure();
    if (!away && !selecting) {
      syncLive();
      toBottom();
    }
  };
  term.onResize(() => {
    histStale = true;
    requestAnimationFrame(relayout);
  });
  term.onRender(() => {
    // 字体、行高变了（格子大小变）时位置要跟着改
    const d = term._core && term._core._renderService && term._core._renderService.dimensions;
    const ch = d && d.css && d.css.cell && d.css.cell.height;
    if (ch && Math.abs(ch - rowH) > 0.01) relayout();
  });
  window.addEventListener('resize', () => requestAnimationFrame(relayout));
  measure();
  syncLive();
  toBottom();
  layer.dataset.mode = mode;
  fetchHistory();

  return {
    layer,
    /** 测试、调试用 */
    selectRow: (y) => selectRowAt(0, y),
    state: () => ({ mode, away, selecting, hist: histCount, rowH, cellW, live: liveRows.length }),
  };
}
