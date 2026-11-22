// window.term 的替身：term-inject.js（按键条、输入法、复制、OSC 52、管理台消息……）原来直接用 xterm.js 的这批接口，
// DOM 渲染下由这里接到 DomTerminal 上，电脑 GPU 渲染下接到 GpuTerminal 上（设计稿 §5.8）。只实现 term-inject.js 用到的那些。

export function makeTermCompat(view) {
  const options = {};
  let fontTimer = 0;
  const refont = () => {
    clearTimeout(fontTimer);
    // term-inject.js 的 refit 会把 fontFamily 改一下再改回来，逼终端重新量字：合并成一次
    fontTimer = setTimeout(() => {
      view.applyFont();
      view.fit();
    }, 0);
  };
  Object.defineProperties(options, {
    fontFamily: {
      get: () => view.fontFamily,
      set: (v) => {
        view.fontFamily = String(v).trim() || view.fontFamily;
        refont();
      },
      enumerable: true,
    },
    fontSize: {
      get: () => view.fontSize,
      set: (v) => {
        view.fontSize = Number(v) || view.fontSize;
        refont();
      },
      enumerable: true,
    },
    lineHeight: {
      get: () => view.lineHeight,
      set: (v) => {
        view.lineHeight = Number(v) || view.lineHeight;
        refont();
      },
      enumerable: true,
    },
    theme: {
      get: () => view.theme,
      set: (v) => view.setTheme(v),
      enumerable: true,
    },
    disableStdin: {
      get: () => view.disableStdin,
      set: (v) => {
        view.disableStdin = Boolean(v);
      },
      enumerable: true,
    },
  });

  const line = (i) => {
    const l = view.bufferLine(i);
    if (!l) return undefined;
    return {
      isWrapped: l.wrappedFromPrev,
      translateToString: (trim) => (trim ? l.text.replace(/\s+$/, '') : l.text),
    };
  };
  const active = {
    get length() {
      return view.histCount + view.rowsN;
    },
    get viewportY() {
      return view.histCount;
    },
    get baseY() {
      return view.histCount;
    },
    get cursorX() {
      return view.cursor.c;
    },
    get cursorY() {
      return view.cursor.r;
    },
    getLine: line,
  };

  const hook = (fn) => {
    view.renderHooks.add(fn);
    return { dispose: () => view.renderHooks.delete(fn) };
  };

  return {
    options,
    get cols() {
      return view.cols;
    },
    get rows() {
      return view.rowsN;
    },
    get element() {
      return view.root;
    },
    get textarea() {
      return view.ta;
    },
    buffer: { active, get normal() { return active; } },
    modes: {
      get mouseTrackingMode() {
        return view.modes.mouse;
      },
      get applicationCursorKeysMode() {
        return view.modes.appCursor;
      },
      get bracketedPasteMode() {
        return view.modes.bracketed;
      },
    },
    parser: {
      registerOscHandler(code, fn) {
        view.oscHandlers.set(code, fn);
        return { dispose: () => view.oscHandlers.delete(code) };
      },
    },
    /** 按键条、管理台发来的输入：原样发出（不套用按键条的修饰键） */
    input(data) {
      view.sendRaw(data);
    },
    paste(text) {
      view.paste(text);
    },
    focus() {
      view.focus();
    },
    blur() {
      view.ta.blur();
    },
    fit() {
      view.fit();
    },
    refresh() {
      // DOM 里的文字不会“画坏”，不需要重画；GPU 渲染重画一帧
      view.redraw?.();
    },
    getSelection() {
      return view.selectionText();
    },
    hasSelection() {
      return Boolean(view.selectionText());
    },
    clearSelection() {
      if (view.clearSelection) view.clearSelection();
      else document.getSelection()?.removeAllRanges();
    },
    scrollToBottom() {
      view.toBottom();
    },
    onRender: hook,
    onWriteParsed: hook,
  };
}
