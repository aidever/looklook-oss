// 看 Mermaid 大图：整屏打开，滚轮 / 双指 / 按钮缩放，拖动平移，“适应屏幕”，复制源码，下载 SVG。
// 图是 Markdown.jsx 里 mermaid 画好的 svg（securityLevel strict，已经是页面里显示的内容）。返回键、Esc、点空白处关闭。
import { useCallback, useEffect, useLayoutEffect, useMemo, useRef, useState } from 'react';
import { createPortal } from 'react-dom';
import { useTranslation } from 'react-i18next';

import { copyText } from './chatCopy.js';
import { useLayer } from './layers.js';
import { fitView, naturalSize, zoomAt } from './mermaidView.js';

const SVG_NS = 'http://www.w3.org/2000/svg';
const STEP = 1.25;
const dist = (a, b) => Math.hypot(a.x - b.x, a.y - b.y);
const mid = (a, b) => ({ x: (a.x + b.x) / 2, y: (a.y + b.y) / 2 });

/** 根 svg 固定成自然尺寸（去掉 mermaid 自带的 max-width / 百分比宽），缩放交给外面的 transform */
function sized(svg) {
  const { w, h } = naturalSize(svg);
  let markup = svg.replace(/<svg\b([^>]*)>/, (_, attrs) => `<svg${attrs.replace(/\s(?:width|height|style)\s*=\s*"[^"]*"/g, '')} width="${w}" height="${h}">`);
  if (!/\sxmlns=/.test(markup.slice(0, markup.indexOf('>')))) markup = markup.replace('<svg', `<svg xmlns="${SVG_NS}"`);
  return { w, h, markup };
}

const Icon = ({ children }) => (
  <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round" aria-hidden="true">
    {children}
  </svg>
);

/**
 * svg：画好的图的 svg 源码；src：Mermaid 源码（复制用）；onClose 关掉。
 */
export default function MermaidViewer({ svg, src, onClose }) {
  const { t } = useTranslation();
  useLayer(true, onClose);
  const { w, h, markup } = useMemo(() => sized(svg), [svg]);
  const box = useRef(null);
  const [view, setView] = useState(null); // 量到容器尺寸之前是 null，不显示
  const [copied, setCopied] = useState(false);
  const pointers = useRef(new Map()); // pointerId → 最近的屏幕坐标
  const gesture = useRef(null); // 单指：起点与是否拖动过；双指：上一次的中点和距离
  const moved = useRef(false);
  const copyTimer = useRef(null);
  useEffect(() => () => clearTimeout(copyTimer.current), []);

  const fit = useCallback(() => {
    const r = box.current?.getBoundingClientRect();
    if (r) setView(fitView({ w, h }, r.width, r.height));
  }, [w, h]);

  useLayoutEffect(() => {
    fit();
  }, [fit]);

  // 窗口尺寸变了（转屏、拉窗口）：重新适应屏幕
  useEffect(() => {
    window.addEventListener('resize', fit);
    return () => window.removeEventListener('resize', fit);
  }, [fit]);

  // 滚轮缩放要用非被动监听，才能阻止页面跟着滚
  useEffect(() => {
    const el = box.current;
    if (!el) return undefined;
    const onWheel = (e) => {
      e.preventDefault();
      const r = el.getBoundingClientRect();
      const dy = Math.max(-100, Math.min(100, e.deltaMode === 1 ? e.deltaY * 16 : e.deltaY));
      const f = Math.exp(-dy * 0.0015);
      setView((v) => v && zoomAt(v, e.clientX - r.left, e.clientY - r.top, f));
    };
    el.addEventListener('wheel', onWheel, { passive: false });
    return () => el.removeEventListener('wheel', onWheel);
  }, []);

  // Esc 只关看图，不让页面拿去做别的
  useEffect(() => {
    const key = (e) => {
      const cw = box.current?.getBoundingClientRect();
      const zoom = (f) => cw && setView((v) => v && zoomAt(v, cw.width / 2, cw.height / 2, f));
      if (e.key === 'Escape') {
        e.preventDefault();
        e.stopPropagation();
        onClose();
      } else if (e.key === '+' || e.key === '=') zoom(STEP);
      else if (e.key === '-') zoom(1 / STEP);
      else if (e.key === '0') fit();
    };
    window.addEventListener('keydown', key, true);
    return () => window.removeEventListener('keydown', key, true);
  }, [onClose, fit]);

  const onPointerDown = (e) => {
    if (e.pointerType === 'mouse' && e.button !== 0) return;
    box.current.setPointerCapture?.(e.pointerId);
    const p = pointers.current;
    p.set(e.pointerId, { x: e.clientX, y: e.clientY });
    if (p.size === 1) {
      gesture.current = { x: e.clientX, y: e.clientY };
      moved.current = false;
    } else if (p.size === 2) {
      const [a, b] = [...p.values()];
      gesture.current = { ...mid(a, b), d: dist(a, b) };
      moved.current = true; // 双指不算点击
    }
  };

  const onPointerMove = (e) => {
    const p = pointers.current;
    if (!p.has(e.pointerId)) return;
    const prev = p.get(e.pointerId);
    const cur = { x: e.clientX, y: e.clientY };
    p.set(e.pointerId, cur);
    const r = box.current.getBoundingClientRect();
    if (p.size === 1) {
      const dx = cur.x - prev.x;
      const dy = cur.y - prev.y;
      if (Math.abs(cur.x - gesture.current.x) + Math.abs(cur.y - gesture.current.y) > 4) moved.current = true;
      setView((v) => v && { ...v, x: v.x + dx, y: v.y + dy });
    } else if (p.size === 2) {
      const [a, b] = [...p.values()];
      const m = mid(a, b);
      const g = gesture.current;
      const f = g.d > 0 ? dist(a, b) / g.d : 1;
      setView((v) => {
        if (!v) return v;
        const z = zoomAt(v, g.x - r.left, g.y - r.top, f);
        return { ...z, x: z.x + (m.x - g.x), y: z.y + (m.y - g.y) };
      });
      gesture.current = { ...m, d: dist(a, b) };
    }
  };

  const onPointerEnd = (e) => {
    pointers.current.delete(e.pointerId);
    if (pointers.current.size === 1) {
      const [only] = pointers.current.values();
      gesture.current = { ...only };
    }
  };

  // 点在图以外的空白处（没有拖动过）就关掉
  const onClick = (e) => {
    if (e.target === box.current && !moved.current) onClose();
  };

  const zoomBy = (f) => {
    const r = box.current.getBoundingClientRect();
    setView((v) => v && zoomAt(v, r.width / 2, r.height / 2, f));
  };

  const onDoubleClick = (e) => {
    const r = box.current.getBoundingClientRect();
    const at = { x: e.clientX - r.left, y: e.clientY - r.top };
    setView((v) => {
      if (!v) return v;
      const f = fitView({ w, h }, r.width, r.height);
      // 已经是适应屏幕的大小 → 放大到 2.5 倍（以双击的点为中心）；否则回到适应屏幕
      return Math.abs(v.s - f.s) < 1e-3 ? zoomAt(v, at.x, at.y, 2.5) : f;
    });
  };

  const copySource = async () => {
    if (await copyText(src)) {
      setCopied(true);
      clearTimeout(copyTimer.current);
      copyTimer.current = setTimeout(() => setCopied(false), 1500);
    } else {
      window.prompt('', src);
    }
  };

  const download = () => {
    const url = URL.createObjectURL(new Blob([markup], { type: 'image/svg+xml' }));
    const a = document.createElement('a');
    a.href = url;
    a.download = 'mermaid.svg';
    document.body.appendChild(a);
    a.click();
    a.remove();
    setTimeout(() => URL.revokeObjectURL(url), 1000);
  };

  const pct = view ? `${Math.round(view.s * 100)}%` : '';
  return createPortal(
    <div className="mmd-view" role="dialog" aria-modal="true" aria-label={t('mermaid.zoom')}>
      <div className="mmd-bar">
        <button type="button" className="mmd-btn" onClick={() => zoomBy(1 / STEP)} title={t('mermaid.zoom_out')} aria-label={t('mermaid.zoom_out')}>
          <Icon><circle cx="11" cy="11" r="7" /><path d="m21 21-4.3-4.3M8 11h6" /></Icon>
        </button>
        <button type="button" className="mmd-btn" onClick={() => zoomBy(STEP)} title={t('mermaid.zoom_in')} aria-label={t('mermaid.zoom_in')}>
          <Icon><circle cx="11" cy="11" r="7" /><path d="m21 21-4.3-4.3M11 8v6M8 11h6" /></Icon>
        </button>
        <span className="mmd-pct" aria-live="polite">{pct}</span>
        <button type="button" className="mmd-btn" onClick={fit} title={t('mermaid.fit')} aria-label={t('mermaid.fit')}>
          <Icon><path d="M8 3H5a2 2 0 0 0-2 2v3M21 8V5a2 2 0 0 0-2-2h-3M3 16v3a2 2 0 0 0 2 2h3M16 21h3a2 2 0 0 0 2-2v-3" /></Icon>
        </button>
        <button type="button" className="mmd-btn" onClick={copySource} title={t('mermaid.copy_source')} aria-label={t('mermaid.copy_source')}>
          {copied ? (
            <Icon><path d="M20 6 9 17l-5-5" /></Icon>
          ) : (
            <Icon><rect x="8" y="8" width="14" height="14" rx="2" /><path d="M4 16c-1.1 0-2-.9-2-2V4c0-1.1.9-2 2-2h10c1.1 0 2 .9 2 2" /></Icon>
          )}
        </button>
        <button type="button" className="mmd-btn" onClick={download} title={t('mermaid.download_svg')} aria-label={t('mermaid.download_svg')}>
          <Icon><path d="M21 15v4a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2v-4M7 10l5 5 5-5M12 15V3" /></Icon>
        </button>
        <button type="button" className="mmd-btn mmd-close" onClick={onClose} title={t('action.close')} aria-label={t('action.close')}>
          <Icon><path d="M18 6 6 18M6 6l12 12" /></Icon>
        </button>
      </div>
      <div
        ref={box}
        className="mmd-box"
        onPointerDown={onPointerDown}
        onPointerMove={onPointerMove}
        onPointerUp={onPointerEnd}
        onPointerCancel={onPointerEnd}
        onDoubleClick={onDoubleClick}
        onClick={onClick}
      >
        <div
          className="mmd-stage"
          style={view ? { transform: `translate(${view.x}px, ${view.y}px) scale(${view.s})` } : { visibility: 'hidden' }}
          // eslint-disable-next-line react/no-danger -- 图是 mermaid（strict）画的，页面里本来就显示着它
          dangerouslySetInnerHTML={{ __html: markup }}
        />
        <p className="mmd-hint">{t('mermaid.hint')}</p>
      </div>
    </div>,
    document.body,
  );
}
