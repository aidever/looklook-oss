// 图的放大看：缩放、平移、适应屏幕的算法。不碰 DOM，单元测试见 mermaidView.test.mjs。
// 视图 { s, x, y }：图上的点 (u, v) 画在屏幕上的 (x + u·s, y + v·s)。

export const MIN_SCALE = 0.1;
export const MAX_SCALE = 8;

const clamp = (n, lo, hi) => Math.min(hi, Math.max(lo, n));

/** 图的自然尺寸：优先 viewBox（mermaid 都带），没有就看 width / height，再不行给 800×600 */
export function naturalSize(svg) {
  const vb = /viewBox\s*=\s*["']\s*[-\d.eE+]+[\s,]+[-\d.eE+]+[\s,]+([\d.eE+]+)[\s,]+([\d.eE+]+)/.exec(svg);
  if (vb && +vb[1] > 0 && +vb[2] > 0) return { w: +vb[1], h: +vb[2] };
  const w = /<svg\b[^>]*\swidth\s*=\s*["']([\d.]+)(?:px)?["']/.exec(svg);
  const h = /<svg\b[^>]*\sheight\s*=\s*["']([\d.]+)(?:px)?["']/.exec(svg);
  if (w && h && +w[1] > 0 && +h[1] > 0) return { w: +w[1], h: +h[1] };
  return { w: 800, h: 600 };
}

/** 适应屏幕：整张图放进 cw×ch 的框里（留 pad 边距），居中。小图也会放大到框里 */
export function fitView({ w, h }, cw, ch, pad = 24) {
  const s = clamp(Math.min((cw - 2 * pad) / w, (ch - 2 * pad) / h), MIN_SCALE, MAX_SCALE);
  return { s, x: (cw - w * s) / 2, y: (ch - h * s) / 2 };
}

/** 以屏幕上的 (px, py) 为中心缩放 f 倍：这一点在屏幕上的位置不动。超出范围就停在边界 */
export function zoomAt(v, px, py, f) {
  const s = clamp(v.s * f, MIN_SCALE, MAX_SCALE);
  const k = s / v.s;
  return { s, x: px - (px - v.x) * k, y: py - (py - v.y) * k };
}
