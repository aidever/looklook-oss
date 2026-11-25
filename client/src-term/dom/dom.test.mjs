import test from 'node:test';
import assert from 'node:assert/strict';
import { ctrlChar, keyToBytes, pasteBytes } from './keys.js';
import { colorCss, escapeHtml, runsHtml, runsText, styleDecl, F } from './style.js';

const key = (k, mods = {}) => ({ key: k, keyCode: 0, isComposing: false, shiftKey: false, altKey: false, ctrlKey: false, metaKey: false, ...mods });
const normal = { appCursor: false };

test('cursor keys follow application cursor mode and modifiers', () => {
  assert.equal(keyToBytes(key('ArrowUp'), normal), '\x1b[A');
  assert.equal(keyToBytes(key('ArrowUp'), { appCursor: true }), '\x1bOA');
  assert.equal(keyToBytes(key('ArrowLeft', { ctrlKey: true }), normal), '\x1b[1;5D');
  assert.equal(keyToBytes(key('ArrowRight', { shiftKey: true }), { appCursor: true }), '\x1b[1;2C');
  assert.equal(keyToBytes(key('Home'), normal), '\x1b[H');
  assert.equal(keyToBytes(key('PageDown'), normal), '\x1b[6~');
  assert.equal(keyToBytes(key('Delete', { shiftKey: true }), normal), '\x1b[3;2~');
  assert.equal(keyToBytes(key('F1'), normal), '\x1bOP');
  assert.equal(keyToBytes(key('F5'), normal), '\x1b[15~');
  assert.equal(keyToBytes(key('F12', { ctrlKey: true }), normal), '\x1b[24;5~');
});

test('editing keys, control and alt combinations', () => {
  assert.equal(keyToBytes(key('Enter'), normal), '\r');
  assert.equal(keyToBytes(key('Backspace'), normal), '\x7f');
  assert.equal(keyToBytes(key('Backspace', { altKey: true }), normal), '\x1b\x7f');
  assert.equal(keyToBytes(key('Tab', { shiftKey: true }), normal), '\x1b[Z');
  assert.equal(keyToBytes(key('Escape'), normal), '\x1b');
  assert.equal(keyToBytes(key('c', { ctrlKey: true }), normal), '\x03');
  assert.equal(keyToBytes(key('[', { ctrlKey: true }), normal), '\x1b');
  assert.equal(keyToBytes(key(' ', { ctrlKey: true }), normal), '\x00');
  assert.equal(keyToBytes(key('b', { altKey: true }), normal), '\x1bb');
  assert.equal(keyToBytes(key('b', { altKey: true }), { appCursor: false, isMac: true }), null); // Option 打特殊字符
  assert.equal(keyToBytes(key('C', { ctrlKey: true, shiftKey: true }), normal), null); // 复制留给浏览器
  assert.equal(keyToBytes(key('a'), normal), null); // 可打印字符走 input 事件
  assert.equal(keyToBytes(key('a', { metaKey: true }), normal), null);
  assert.equal(keyToBytes({ ...key('Process'), keyCode: 229 }, normal), null); // 输入法组字中
  assert.equal(ctrlChar('?'), '\x7f');
  assert.equal(ctrlChar('é'), null);
});

test('paste normalizes newlines and wraps bracketed paste', () => {
  assert.equal(pasteBytes('a\nb\r\nc', false), 'a\rb\rc');
  assert.equal(pasteBytes('x\x1b[201~y', true), '\x1b[200~xy\x1b[201~');
});

test('color codes map to theme, 256-color cube and truecolor', () => {
  const theme = { red: '#ff0000' };
  assert.equal(colorCss(-1, theme), null);
  assert.equal(colorCss(1, theme), '#ff0000');
  assert.equal(colorCss(2, theme), '#4e9a06');
  assert.equal(colorCss(16, theme), '#000000');
  assert.equal(colorCss(196, theme), '#ff0000');
  assert.equal(colorCss(232, theme), '#080808');
  assert.equal(colorCss(0x1000000 + 0x0a0b0c, theme), '#0a0b0c');
  assert.equal(colorCss(1, theme, { 1: 0x1000000 + 0x123456 }), '#123456'); // OSC 4 改过的调色板
});

test('style declarations', () => {
  assert.equal(styleDecl(1, -1, F.BOLD, {}), 'color:#ef2929;font-weight:bold;'); // 粗体用亮色
  assert.equal(styleDecl(-1, -1, F.INVERSE, { foreground: '#eee', background: '#111' }), 'color:#111;background:#eee;');
  assert.equal(styleDecl(-1, -1, F.UNDERCURL | F.STRIKEOUT, {}), 'text-decoration-line:underline line-through;text-decoration-style:wavy;');
});

test('runs render with fixed-width boxes, classes and links', () => {
  const sheet = { cls: (f = -1, b = -1, a = 0) => (f === -1 && b === -1 && a === 0 ? '' : `s${f}_${b}_${a}`) };
  const runs = [{ t: 'a<b' }, { w: 2, c: ['中', '文'], f: 1 }, { w: 1, c: ['─'] }, { t: 'go', l: 'https://x/"y' }];
  assert.equal(
    runsHtml(runs, sheet),
    'a&lt;b<span class="s1_-1_0"><i class="w2">中</i><i class="w2">文</i></span><i class="w1">─</i>' +
      '<span class="lk" data-l="https://x/&quot;y">go</span>',
  );
  assert.equal(runsText(runs), 'a<b中文─go');
  assert.equal(escapeHtml('<&>"'), '&lt;&amp;&gt;&quot;');
});

// ---------- 输入框按差发字（手机输入法重交整个词不重复） ----------
import { attachTextInput } from './input.js';

function fakeInput() {
  const ta = new EventTarget();
  ta.value = '';
  ta.selectionStart = ta.selectionEnd = 0;
  const out = [];
  const ctl = attachTextInput(ta, {
    modes: () => ({ appCursor: false }),
    sendRaw: (d, keep) => {
      if (!keep) ctl.reset();
      out.push(d);
    },
    sendTyped: (t) => out.push(t),
    paste: () => {},
  });
  const ev = (type, props = {}) => {
    const e = new Event(type, { cancelable: true });
    Object.assign(e, { isComposing: false, data: null, inputType: '' }, props);
    ta.dispatchEvent(e);
    return e;
  };
  const set = (v) => {
    ta.value = v;
    ta.selectionStart = ta.selectionEnd = v.length;
  };
  const type = (v) => {
    set(v);
    ev('input', { inputType: 'insertText' });
  };
  const compose = (v) => {
    ev('compositionstart');
    set(v);
    ev('input', { inputType: 'insertCompositionText', isComposing: true });
    ev('compositionend', { data: v });
  };
  return { ta, out, ev, set, type, compose, ctl };
}

test('IME re-committing the whole word does not repeat what was already sent', () => {
  const f = fakeInput();
  f.type('/');
  f.compose('/n'); // 输入法把光标前的 “/” 连同新字一起再交一遍
  f.compose('/ne');
  f.compose('/new');
  f.ev('input', { inputType: 'insertText' }); // Safari 组字后补的同样内容
  assert.equal(f.out.join(''), '/new');
});

test('IME deleting or replacing text sends backspaces for the difference', () => {
  const f = fakeInput();
  f.compose('teh');
  f.compose('the'); // 自动纠错
  assert.equal(f.out.join(''), 'teh\x7f\x7fhe');
  f.type('th');
  assert.equal(f.out.join(''), 'teh\x7f\x7fhe\x7f');
  // 输入框空了再退格：直接发
  const g = fakeInput();
  const e = g.ev('beforeinput', { inputType: 'deleteContentBackward' });
  assert.ok(e.defaultPrevented);
  assert.deepEqual(g.out, ['\x7f']);
});

test('enter and other keys clear the box so later typing starts fresh', () => {
  const f = fakeInput();
  f.type('ls');
  f.ev('beforeinput', { inputType: 'insertLineBreak' });
  assert.equal(f.ta.value, '');
  f.type('p');
  assert.equal(f.out.join(''), 'ls\rp');
  // 组字中被按键条打断：组字结束、发完再清空
  const g = fakeInput();
  g.ev('compositionstart');
  g.set('ni');
  g.ctl.reset();
  assert.equal(g.ta.value, 'ni');
  g.set('你');
  g.ev('compositionend', { data: '你' });
  assert.equal(g.out.join(''), '你');
  assert.equal(g.ta.value, '');
});
