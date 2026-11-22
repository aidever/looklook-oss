// termd 会话在电脑上（地址上 ?renderer=gpu）用的终端：wgpu 渲染 + looklook-term.v1（设计稿 §7）。
// 由 main.js 按需加载；xterm.js 那一套不会被加载。WASM（crates/term-gpu）里带着 term-web 的 TermClient。

import { TermConnection } from '../dom/conn.js';
import { makeTermCompat } from '../dom/compat.js';
import { ProgressBar } from '../progress.js';
import { attachDrop } from '../termd-drop.js';
import { GpuTerminal } from './view.js';
import './gpu.css';

export async function startGpu(container, prefs) {
  const base = location.pathname.replace(/[/]+$/, '');
  const mod = await import('./wasm/term_gpu.js');
  await mod.default();
  let view = null;
  const conn = new TermConnection(base, {
    wasm: async () => mod,
    keepHistory: true,
    onUpdates: (list) => view?.push(list),
    onPredict: () => view?.redraw(),
    trzsz: prefs.enableTrzsz ? { windows: prefs.isWindows, dragInitTimeout: prefs.trzszDragInitTimeout } : null,
    onState: (open, ended) => {
      document.documentElement.classList.toggle('lld-ended', !open && Boolean(ended));
    },
  });
  view = new GpuTerminal(container, prefs, conn);

  // 等终端字体就绪再量格子
  try {
    await Promise.race([
      Promise.all([400, 700].map((w) => document.fonts.load(w + ' ' + view.fontSize + 'px "JetBrains Mono"'))),
      new Promise((r) => setTimeout(r, 1500)),
    ]);
  } catch {
    // 字体加载失败：用回退字体
  }
  const forceGl = new URLSearchParams(location.search).get('gpu') === 'gl';
  await view.init(mod, forceGl);
  document.fonts?.addEventListener?.('loadingdone', () => view.applyFont());

  const titleFixed = prefs.titleFixed;
  if (titleFixed) document.title = titleFixed;
  view.onTitle = (t) => {
    if (!titleFixed) document.title = t || 'Terminal';
  };
  const bar = new ProgressBar(container, () => view.theme);
  view.onProgress = (u) => {
    const state = { hidden: 0, normal: 1, error: 2, indeterminate: 3, paused: 4 }[u.s] ?? 0;
    bar.set({ state, value: u.v });
  };

  window.term = makeTermCompat(view);
  window.__llTerm = { view, conn }; // 排查用（控制台里看预测、trzsz 状态）
  attachDrop(container, conn);
  const { cols, rows } = view.proposedOrDefault();
  await conn.start(cols, rows);
  if (document.visibilityState !== 'visible') conn.setView(2);
  view.fit();
  view.focus();
  return view;
}
