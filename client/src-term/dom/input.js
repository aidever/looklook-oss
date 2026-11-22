// 看不见的输入框接键盘、输入法、粘贴（手机 DOM 渲染和电脑 wgpu 渲染共用）：
// 特殊键在 keydown 里编成终端字节；可打印字符走 input 事件（输入法、手机键盘都走这条路）；组字结束一次发出。

import { keyToBytes } from './keys.js';

export const IS_MAC = /Mac|iPhone|iPad|iPod/.test(navigator.platform || '') || /Mac OS X/.test(navigator.userAgent);
export const IS_WIN = /Win/.test(navigator.platform || '');

/**
 * @param {HTMLTextAreaElement} ta
 * @param {{
 *   modes(): { appCursor: boolean },
 *   sendRaw(data: string): void,
 *   sendTyped(text: string): void,
 *   paste(text: string): void,
 *   onKey?(e: KeyboardEvent): boolean | 'pass',
 *   onCompose?(text: string | null): void,
 * }} h onKey：先给调用方看（快捷键），返回 true 表示已处理，'pass' 表示交给浏览器默认处理；onCompose：组字中的文字（结束时 null）
 */
export function attachTextInput(ta, h) {
  let composing = false, composed = null;
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
  ta.addEventListener('compositionend', (e) => {
    composing = false;
    h.onCompose?.(null);
    if (e.data) {
      composed = { text: e.data, at: Date.now() };
      h.sendTyped(e.data);
    }
    ta.value = '';
  });
  ta.addEventListener('beforeinput', (e) => {
    // 手机键盘的退格、回车多半只有 beforeinput（keydown 是 229）
    if (composing || e.isComposing) return;
    if (e.inputType === 'deleteContentBackward') {
      e.preventDefault();
      h.sendRaw('\x7f');
    } else if (e.inputType === 'insertLineBreak' || e.inputType === 'insertParagraph') {
      e.preventDefault();
      h.sendRaw('\r');
    }
  });
  ta.addEventListener('input', (e) => {
    if (composing || e.isComposing || e.inputType === 'insertCompositionText') return;
    const text = e.data ?? ta.value;
    // Safari 组字结束后还会补一个同样内容的 insertText
    if (composed && composed.text === text && Date.now() - composed.at < 80) {
      composed = null;
      ta.value = '';
      return;
    }
    composed = null;
    if (e.inputType === 'insertFromPaste') return; // paste 事件里处理
    if (text) h.sendTyped(text);
    ta.value = '';
  });
  ta.addEventListener('paste', (e) => {
    e.preventDefault();
    h.paste(e.clipboardData?.getData('text/plain') || '');
    ta.value = '';
  });
}
