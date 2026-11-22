// 键盘事件 → 发给终端的字节（xterm 的约定）。可打印字符不在这里处理：交给输入框的 input 事件（输入法、手机键盘都走那条路）。

const CSI = '\x1b[';
const SS3 = '\x1bO';

/** xterm 的修饰键参数：1 + Shift(1) + Alt(2) + Ctrl(4) + Meta(8)；没有修饰键时为 0 */
function modParam(e) {
  const m = (e.shiftKey ? 1 : 0) + (e.altKey ? 2 : 0) + (e.ctrlKey ? 4 : 0) + (e.metaKey ? 8 : 0);
  return m ? m + 1 : 0;
}

const CURSOR = { ArrowUp: 'A', ArrowDown: 'B', ArrowRight: 'C', ArrowLeft: 'D', Home: 'H', End: 'F' };
const TILDE = { Insert: 2, Delete: 3, PageUp: 5, PageDown: 6, F5: 15, F6: 17, F7: 18, F8: 19, F9: 20, F10: 21, F11: 23, F12: 24 };
const SS3_F = { F1: 'P', F2: 'Q', F3: 'R', F4: 'S' };

/** Ctrl + 一个字符 → 控制字符；不是能组合的字符返回 null */
export function ctrlChar(ch) {
  if (ch.length !== 1) return null;
  const c = ch.toUpperCase().charCodeAt(0);
  if (c >= 64 && c <= 95) return String.fromCharCode(c - 64); // @ A-Z [ \ ] ^ _
  if (ch === ' ' || ch === '2') return '\x00';
  if (ch >= '3' && ch <= '7') return String.fromCharCode(ch.charCodeAt(0) - 24); // 3→ESC … 7→^_
  if (ch === '8' || ch === '?') return '\x7f';
  if (ch === '/') return '\x1f';
  return null;
}

/**
 * 键盘事件对应的字节；返回 null 表示不归这里管（可打印字符、浏览器快捷键、输入法组字中）。
 * @param {KeyboardEvent} e
 * @param {{ appCursor: boolean, isMac?: boolean }} modes
 */
export function keyToBytes(e, modes) {
  if (e.isComposing || e.keyCode === 229) return null;
  const k = e.key;
  const mod = modParam(e);
  if (k in CURSOR) {
    if (mod) return CSI + '1;' + mod + CURSOR[k];
    return (modes.appCursor ? SS3 : CSI) + CURSOR[k];
  }
  if (k in TILDE) return CSI + TILDE[k] + (mod ? ';' + mod : '') + '~';
  if (k in SS3_F) return mod ? CSI + '1;' + mod + SS3_F[k] : SS3 + SS3_F[k];
  // 电脑上的 ⌘ 组合留给浏览器 / 管理台
  if (e.metaKey) return null;
  switch (k) {
    case 'Enter':
      return e.altKey ? '\x1b\r' : '\r';
    case 'Backspace':
      if (e.ctrlKey) return '\x08';
      return e.altKey ? '\x1b\x7f' : '\x7f';
    case 'Tab':
      return e.shiftKey ? CSI + 'Z' : '\t';
    case 'Escape':
      return '\x1b';
  }
  if (k.length !== 1) return null;
  if (e.ctrlKey && !e.altKey) {
    // Ctrl+Shift+C / V：复制、粘贴，留给浏览器
    if (e.shiftKey && (k === 'C' || k === 'V' || k === 'c' || k === 'v')) return null;
    return ctrlChar(k);
  }
  if (e.ctrlKey && e.altKey) {
    // Windows 上 AltGr = Ctrl + Alt，出的是普通字符
    const c = ctrlChar(k);
    return c && !modes.isWindows ? '\x1b' + c : null;
  }
  // Mac 上 Option 用来打特殊字符；其他系统 Alt + 字符 = ESC 前缀
  if (e.altKey && !modes.isMac) return '\x1b' + k;
  return null;
}

/** 粘贴：换行统一成回车；程序开了 bracketed paste 就包上 ESC[200~ … ESC[201~ */
export function pasteBytes(text, bracketed) {
  const body = text.replace(/\r?\n/g, '\r');
  return bracketed ? '\x1b[200~' + body.replace(/\x1b\[20[01]~/g, '') + '\x1b[201~' : body;
}
