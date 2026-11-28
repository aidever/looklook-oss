// 文件查看器的大小（docs/FILE_VIEWER_LAYOUT_2026-11-27.md）：停靠 / 放大 / 全屏，加上“新标签页”打开 /view。
// 只在电脑上（不是触屏）有；手机上的查看器本来就是整屏。
import { Expand, ExternalLink, Maximize2, Minimize2, Shrink } from 'lucide-react';
import { useLayoutEffect, useRef, useState } from 'react';
import { createPortal } from 'react-dom';
import { useTranslation } from 'react-i18next';

import { touchUi } from '../shared/layers.js';

export const sizeable = !touchUi;

/**
 * 单独的查看页地址：`/view?path=…`，`id`：文件属于哪个会话（走 /instances/{id}/…，远程时按会话所在的电脑转发）；
 * `device`：没有会话时文件在哪台电脑；`focus`：标出的行；`diff`：打开就看改动；`artifact`：Codex 产出的文件。
 */
export function viewUrl({ id, device, path, focus, diff, artifact }) {
  const q = new URLSearchParams({ path });
  if (id) q.set('id', id);
  if (device && !id) q.set('device', device);
  if (focus?.from) {
    q.set('line', String(focus.from));
    if (focus.to && focus.to !== focus.from) q.set('to', String(focus.to));
  }
  if (diff) q.set('diff', '1');
  if (artifact) q.set('art', '1');
  return `/view?${q}`;
}

export const openViewTab = (target) => window.open(viewUrl(target), '_blank', 'noopener');

/**
 * 全屏：查看器盖住整个页面（含左边的会话栏）。查看器所在的容器有 container-type（布局隔离），
 * position: fixed 出不去，所以把查看器放在一个自己的节点里（portal），全屏时把这个节点挪到 body 下，
 * 不全屏时放回原位——React 不重新挂载，编辑到一半的内容、滚动位置都在。
 * 用法：`const fh = useFullHost(full)`，在原位放 `{fh.slot}`，内容用 `fh.portal(内容)` 包起来。
 */
export function useFullHost(full) {
  const [host] = useState(() => {
    const el = document.createElement('div');
    el.className = 'fv-host';
    return el;
  });
  const slot = useRef(null);
  // 每次渲染都对一下（原位的 slot 可能卸了又挂上）
  useLayoutEffect(() => {
    const parent = full ? document.body : slot.current;
    if (parent && host.parentNode !== parent) parent.appendChild(host);
  });
  useLayoutEffect(() => () => host.remove(), [host]);
  return { slot: <span ref={slot} className="fv-host" />, portal: (children) => createPortal(children, host) };
}

/** 标题栏上的三个按钮：放大 / 还原、全屏 / 退出全屏、新标签页打开。哪个没传就不显示。 */
export function SizeButtons({ wide, onWide, full, onFull, onNewTab }) {
  const { t } = useTranslation();
  if (!sizeable) return null;
  return (
    <>
      {onWide && !full && (
        <button type="button" className={`btn btn-plain btn-icon ${wide ? 'on' : ''}`} onClick={onWide} aria-pressed={wide} aria-label={t(wide ? 'fv.restore' : 'fv.widen')} title={`${t(wide ? 'fv.restore' : 'fv.widen')} (W)`}>
          {wide ? <Minimize2 size={16} /> : <Maximize2 size={16} />}
        </button>
      )}
      {onFull && (
        <button type="button" className={`btn btn-plain btn-icon ${full ? 'on' : ''}`} onClick={onFull} aria-pressed={full} aria-label={t(full ? 'fv.full_exit' : 'fv.full')} title={`${t(full ? 'fv.full_exit' : 'fv.full')} (F)`}>
          {full ? <Shrink size={16} /> : <Expand size={16} />}
        </button>
      )}
      {onNewTab && (
        <button type="button" className="btn btn-plain btn-icon" onClick={onNewTab} aria-label={t('fv.new_tab')} title={t('fv.new_tab')}>
          <ExternalLink size={16} />
        </button>
      )}
    </>
  );
}

/** W / F 快捷键要不要管：焦点在输入框、编辑器里，或者按着修饰键时不管 */
export const sizeKey = (e) =>
  !e.altKey && !e.ctrlKey && !e.metaKey && !e.shiftKey && !e.target.closest?.('input, textarea, select, [contenteditable], .cm-editor') ? { w: 'wide', f: 'full' }[e.key.toLowerCase()] ?? null : null;
