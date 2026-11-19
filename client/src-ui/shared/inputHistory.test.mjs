// node --test src-ui/shared/inputHistory.test.mjs（npm test）
import assert from 'node:assert/strict';
import { test } from 'node:test';

import { inputHistory, searchInputs } from './inputHistory.js';

test('inputHistory: 当前对话倒着在前，再接本机存着的，去重', () => {
  const items = [{ k: 'user', text: 'a' }, { k: 'text', text: 'reply' }, { k: 'user', text: ' b ' }, { k: 'user', text: '' }];
  const saved = [{ text: 'b', at: 3 }, { text: 'c', at: 2 }, { text: 'a', at: 1 }];
  assert.deepEqual(inputHistory(items, saved), [
    { text: 'b', at: null, here: true },
    { text: 'a', at: null, here: true },
    { text: 'c', at: 2, here: false },
  ]);
});

test('searchInputs: 每个词都要有，不分大小写', () => {
  const list = [{ text: 'Fix the login bug' }, { text: 'fix tests' }, { text: 'deploy' }];
  assert.deepEqual(searchInputs(list, 'FIX bug').map((x) => x.text), ['Fix the login bug']);
  assert.equal(searchInputs(list, '  ').length, 3);
});

test('inputHistory: 同一个会话号的也算这段对话里的', () => {
  const saved = [{ text: 'x', at: 2, session: 's1' }, { text: 'y', at: 1, session: 's0' }];
  assert.deepEqual(inputHistory([], saved, 's1').map((x) => x.here), [true, false]);
});

test('inputHistory: 看看送进助理的通知不算输入', () => {
  const ev = '<looklook-event type="turn_end" session="s1" title="修登录">\n做完了\n</looklook-event>';
  const items = [{ k: 'user', text: ev }, { k: 'user', text: '修一下登录' }];
  assert.deepEqual(inputHistory(items, [{ text: ev, at: 1 }]).map((x) => x.text), ['修一下登录']);
});
