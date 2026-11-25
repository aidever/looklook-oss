// 看不见的输入框接键盘、输入法、粘贴（手机 DOM 渲染和电脑 wgpu 渲染共用）：
// 特殊键在 keydown 里编成终端字节；可打印字符走 input 事件（输入法、手机键盘都走这条路）；组字结束一次发出。
// 可打印字符按“输入框里的字 − 已发出的字”的差发，不靠每次清空输入框（见 attachTextInput）。

import { keyToBytes } from './keys.js';

export const IS_MAC = /Mac|iPhone|iPad|iPod/.test(navigator.platform || '') || /Mac OS X/.test(navigator.userAgent);
export const IS_WIN = /Win/.test(navigator.platform || '');

/**
 * @param {HTMLTextAreaElement} ta
 * 返回 { reset }：别的途径（按键条、粘贴）发了输入后调用，清空输入框重新对齐。
 * @param {{
 *   modes(): { appCursor: boolean },
 *   sendRaw(data: string, keepInput?: boolean): void,
 *   sendTyped(text: string): void,
 *   paste(text: string): void,
 *   onKey?(e: KeyboardEvent): boolean | 'pass',
 *   onCompose?(text: string | null): void,
 * }} h onKey：先给调用方看（快捷键），返回 true 表示已处理，'pass' 表示交给浏览器默认处理；onCompose：组字中的文字（结束时 null）
 */
export function attachTextInput(ta, h) {
  // 输入框里的字不再每发一次就清空：手机输入法（搜狗、讯飞、Gboard 英文联想）自己记着光标前的“当前词”，
  // 网页偷偷清空它们常常不知道，下一次组字会把整个词（连同已经发过的）再交一遍，终端里就成了 “//n/n/ne/n”。
  // 现在让输入框照常留着字，每次只把“已发出的”和“现在的”之差发给终端（多出来的发字符，少了的发退格）；
  // 重发同一个词、Safari 组字后补的 insertText 都没有差，不会重复。按键条、回车、方向键等别的输入发出后才清空（reset）。
  let composing = false, sent = '', pendingReset = false;
  const MAX_KEEP = 64;

  const reset = () => {
    if (composing) {
      pendingReset = true;
      return;
    }
    pendingReset = false;
    sent = '';
    if (ta.value) ta.value = '';
  };

  const sync = () => {
    const now = ta.value;
    if (now !== sent) {
      let p = 0;
      const n = Math.min(now.length, sent.length);
      while (p < n && now.charCodeAt(p) === sent.charCodeAt(p)) p++;
      // 退格按字（码点）数；补发的部分整段发
      const del = Array.from(sent.slice(p)).length;
      sent = now;
      if (del) h.sendRaw('\x7f'.repeat(del), true);
      const add = now.slice(p);
      if (add) h.sendTyped(add);
    }
    if (pendingReset || ta.value.length > MAX_KEEP || /[\r\n]/.test(ta.value)) reset();
  };

  ta.addEventListener('keydown', (e) => {
    const r = h.onKey?.(e);
    if (r === 'pass') return; // 交给浏览器（复制、粘贴）
    if (r) {
      e.preventDefault();
      return;
    }
    const bytes = keyToBytes(e, { appCursor: h.modes().appCursor, isMac: IS_MAC, isWindows: IS_WIN });
    if (bytes === null) return;
    e.preventDefault();
    h.sendRaw(bytes);
  });
  ta.addEventListener('compositionstart', () => {
    composing = true;
  });
  ta.addEventListener('compositionupdate', (e) => {
    h.onCompose?.(e.data || '');
  });
  ta.addEventListener('compositionend', () => {
    composing = false;
    h.onCompose?.(null);
    sync();
  });
  ta.addEventListener('beforeinput', (e) => {
    // 手机键盘的退格、回车多半只有 beforeinput（keydown 是 229）
    if (composing || e.isComposing) return;
    if (e.inputType === 'deleteContentBackward') {
      // 输入框里有字就让它自己删，input 里按差发退格；空的时候删不动，直接发
      if (ta.selectionStart === 0 && ta.selectionEnd === 0) {
        e.preventDefault();
        h.sendRaw('\x7f');
      }
    } else if (e.inputType === 'insertLineBreak' || e.inputType === 'insertParagraph') {
      e.preventDefault();
      sync();
      h.sendRaw('\r');
    }
  });
  ta.addEventListener('input', (e) => {
    if (composing || e.isComposing || e.inputType === 'insertCompositionText') return;
    if (e.inputType === 'insertFromPaste') return; // paste 事件里处理
    sync();
  });
  ta.addEventListener('paste', (e) => {
    e.preventDefault();
    h.paste(e.clipboardData?.getData('text/plain') || '');
  });
  ta.addEventListener('blur', reset);
  return { reset };
}
