import assert from 'node:assert/strict';
import test from 'node:test';

globalThis.window = { dispatchEvent() {} };
const store = new Map();
globalThis.localStorage = { getItem: (k) => store.get(k) ?? null, setItem: (k, v) => store.set(k, String(v)), removeItem: (k) => store.delete(k) };
globalThis.Event = class { constructor(t) { this.type = t; } };
const { cleanOperator, operatorName, setCloudOperator, setOwnOperator, withOperator } = await import('./operator.js');

test('clean trims, strips control chars, caps 32', () => {
  assert.equal(cleanOperator('  张三\n '), '张三');
  assert.equal(cleanOperator('a\u0007b'), 'a b');
  assert.equal(cleanOperator('字'.repeat(40)).length, 32);
  assert.equal(cleanOperator(null), '');
});

test('own name wins over cloud nickname', () => {
  setCloudOperator('cloud');
  assert.equal(operatorName(), 'cloud');
  setOwnOperator(' 小王 ');
  assert.equal(operatorName(), '小王');
  setOwnOperator('');
  assert.equal(operatorName(), 'cloud');
});

test('only POST /instances gets signed', () => {
  setCloudOperator('me');
  assert.deepEqual(withOperator('POST', '/instances', { name: 'x' }), { name: 'x', created_by: 'me' });
  assert.deepEqual(withOperator('POST', '/instances', { created_by: 'other' }), { created_by: 'other' });
  assert.deepEqual(withOperator('PUT', '/instances', { a: 1 }), { a: 1 });
  assert.deepEqual(withOperator('POST', '/instances/x/start', {}), {});
  setCloudOperator('');
  assert.deepEqual(withOperator('POST', '/instances', { a: 1 }), { a: 1 });
});
