import { test } from 'node:test';
import assert from 'node:assert/strict';
import { fitView, naturalSize, zoomAt, MIN_SCALE, MAX_SCALE } from './mermaidView.js';

test('naturalSize 读 viewBox', () => {
  assert.deepEqual(naturalSize('<svg xmlns="x" viewBox="0 0 320.5 200" style="max-width: 320px"></svg>'), { w: 320.5, h: 200 });
  assert.deepEqual(naturalSize('<svg viewBox="-10 -5 100 50"></svg>'), { w: 100, h: 50 });
});

test('naturalSize 没有 viewBox 时读 width/height，都没有给默认值', () => {
  assert.deepEqual(naturalSize('<svg width="120px" height="60"></svg>'), { w: 120, h: 60 });
  assert.deepEqual(naturalSize('<svg><path/></svg>'), { w: 800, h: 600 });
});

test('fitView 整张图居中放进框里', () => {
  const v = fitView({ w: 200, h: 100 }, 424, 424, 12);
  // 宽：(424-24)/200 = 2，高：400/100 = 4，取小的 2
  assert.equal(v.s, 2);
  assert.equal(v.x, (424 - 400) / 2);
  assert.equal(v.y, (424 - 200) / 2);
});

test('fitView 限制在缩放范围内', () => {
  assert.equal(fitView({ w: 1, h: 1 }, 1000, 1000).s, MAX_SCALE);
  assert.equal(fitView({ w: 1e6, h: 1e6 }, 300, 300).s, MIN_SCALE);
});

test('zoomAt 让光标下的那一点不动', () => {
  const v = { s: 1, x: 10, y: 20 };
  const px = 100;
  const py = 50;
  const u = (px - v.x) / v.s; // 图上的点
  const z = zoomAt(v, px, py, 2);
  assert.equal(z.s, 2);
  assert.ok(Math.abs(z.x + u * z.s - px) < 1e-9);
  assert.ok(Math.abs(z.y + ((py - v.y) / v.s) * z.s - py) < 1e-9);
});

test('zoomAt 到边界就停', () => {
  assert.equal(zoomAt({ s: MAX_SCALE, x: 0, y: 0 }, 0, 0, 2).s, MAX_SCALE);
  assert.equal(zoomAt({ s: MIN_SCALE, x: 0, y: 0 }, 0, 0, 0.5).s, MIN_SCALE);
});
