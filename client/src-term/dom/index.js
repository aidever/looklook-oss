// termd 会话、手机上（或地址上带 ?renderer=dom）用的终端：DOM 渲染 + looklook-term.v1（设计稿 §6）。
// 由 main.js 按需加载；xterm.js 那一套不会被加载。

import { TermConnection } from './conn.js';
import { DomTerminal } from './view.js';
import { makeTermCompat } from './compat.js';
import { ProgressBar } from '../progress.js';
import { attachDrop } from '../termd-drop.js';
import './dom.css';

export async function startDom(container, prefs) {
  const base = location.pathname.replace(/[/]+$/, '');
  let view = null;
  const conn = new TermConnection(base, {
    wasm: () => import('./wasm/term_web.js'),
    onUpdates: (list) => view?.push(list),
    onPredict: () => view?.renderPredictions(),
    trzsz: prefs.enableTrzsz ? { windows: prefs.isWindows, dragInitTimeout: prefs.trzszDragInitTimeout } : null,
    onState: (open, ended) => {
      document.documentElement.classList.toggle('lld-ended', !open && Boolean(ended));
    },
  });
  view = new DomTerminal(container, prefs, conn);
  const titleFixed = prefs.titleFixed;
  if (titleFixed) document.title = titleFixed;
  view.onTitle = (t) => {
    if (!titleFixed) document.title = t || 'Terminal';
  };
  const bar = new ProgressBar(container, () => view.theme);
  view.onProgress = (u) => {
    // 和 xterm 的 ProgressAddon 一样的状态：0 隐藏 1 普通 2 出错 3 不确定 4 暂停
    const state = { hidden: 0, normal: 1, error: 2, indeterminate: 3, paused: 4 }[u.s] ?? 0;
    bar.set({ state, value: u.v });
  };

  // 等终端字体就绪再量格子，免得先按回退字体排一次、再按网页字体排一次
  try {
    await Promise.race([
      Promise.all([400, 700].map((w) => document.fonts.load(w + ' ' + view.fontSize + 'px "JetBrains Mono"'))),
      new Promise((r) => setTimeout(r, 1500)),
    ]);
  } catch {
    // 字体加载失败：用回退字体
  }
  view.measure();
  document.fonts?.addEventListener?.('loadingdone', () => view.measure());

  window.__llTouch = true; // term-inject.js：触摸滑动、选字由这里处理
  window.term = makeTermCompat(view);
  window.__llTerm = { view, conn }; // 排查用（控制台里看预测、trzsz 状态）
  attachDrop(container, conn);
  const { cols, rows } = view.proposedOrDefault();
  await conn.start(cols, rows);
  if (document.visibilityState !== 'visible') conn.setView(2);
  view.fit();
  return view;
}
