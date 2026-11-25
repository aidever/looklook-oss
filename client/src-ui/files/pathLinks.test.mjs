import assert from 'node:assert/strict';
import { test } from 'node:test';
import { pathLike, scanPaths } from './pathLinks.js';

const paths = (s) => scanPaths(s).map((x) => [x.path, x.from, x.to]);

test('scanPaths finds paths with line forms', () => {
  assert.deepEqual(paths('改了 src/main.rs:12 和 `a/b.ts:3:5`'), [['src/main.rs', 12, 12], ['a/b.ts', 3, 3]]);
  assert.deepEqual(paths('see lib/x.py#L10-L20.'), [['lib/x.py', 10, 20]]);
  assert.deepEqual(paths('打开 README.md。'), [['README.md', 0, 0]]);
  assert.deepEqual(paths('in ~/a/b.txt, C:\\x\\y.log'), [['~/a/b.txt', 0, 0], ['C:\\x\\y.log', 0, 0]]);
  assert.deepEqual(paths('看 src/a.rs.'), [['src/a.rs', 0, 0]]);
  assert.deepEqual(paths('文档/设计稿.md'), [['文档/设计稿.md', 0, 0]]);
});

test('scanPaths skips urls, versions, numbers', () => {
  assert.deepEqual(paths('https://example.com/a/b.html'), []);
  assert.deepEqual(paths('version 1.2.3 and 3.14'), []);
  assert.deepEqual(paths('e.g. this'), []);
  assert.equal(pathLike('/'), false);
  assert.equal(pathLike('.env'), false);
});
