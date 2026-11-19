import assert from 'node:assert/strict';
import { test } from 'node:test';
import { inProject } from './project.js';

test('inProject', () => {
  assert.ok(inProject('/home/a/proj', '/home/a/proj'));
  assert.ok(inProject('/home/a/proj/web', '/home/a/proj/'));
  assert.ok(!inProject('/home/a/proj2', '/home/a/proj'), '前缀相同的兄弟目录不算');
  assert.ok(!inProject('/home/a', '/home/a/proj'));
  assert.ok(inProject('C:\\Users\\A\\Proj\\app', 'c:/users/a/proj'), 'Windows 不分大小写、斜杠');
  assert.ok(!inProject(null, '/x') && !inProject('/x', ''));
});
