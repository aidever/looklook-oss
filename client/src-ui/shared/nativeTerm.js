// 用这台电脑上装的终端（Ghostty、iTerm2、Windows Terminal……）打开会话（后端 src/native_term.rs，设计稿 §8）。
// 只问本机：管理台不是在这台电脑上打开时（远程、局域网）没有这个选项。会话在同一账户的另一台电脑上也行：
// 窗口还是弹在这台电脑上，窗口里的 `looklook attach --device` 经本机看看连过去。
import { useEffect, useState } from 'react';
import { isLocalDevice, localApi, terminalDevice } from './api.js';

let cache = null;
const listeners = new Set();

function load(force = false) {
  if (!cache || force) {
    cache = localApi.get('/native-terminals').catch(() => ({ items: [], default: null }));
    cache.then((v) => listeners.forEach((f) => f(v)));
  }
  return cache;
}

/** 识别到的终端 `{ items: [{ id, name }], default }`；还没取到时 items 为空 */
export function useNativeTerms() {
  const [v, setV] = useState({ items: [], default: null });
  useEffect(() => {
    let alive = true;
    const on = (x) => alive && setV(x);
    listeners.add(on);
    load().then(on);
    return () => {
      alive = false;
      listeners.delete(on);
    };
  }, []);
  return v;
}

/** 设置里改了默认终端：重新取 */
export function refreshNativeTerms() {
  load(true);
}

export function openNative(id, terminal) {
  const dev = terminalDevice(id);
  return localApi.post('/native-open', { id, device: isLocalDevice(dev) ? null : dev, terminal: terminal ?? null });
}

/** 这一页里已经自动用默认终端打开过的会话（切回来时不再弹窗口） */
export const autoOpened = new Set();
