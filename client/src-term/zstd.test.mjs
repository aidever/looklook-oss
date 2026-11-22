// node --test src-term/zstd.test.mjs（npm test）
import assert from 'node:assert/strict';
import { test } from 'node:test';
import zlib from 'node:zlib';

import { TermZstdDecoder, ZSTD_SUBPROTOCOL, wsProtocols } from './zstd.js';
import trzszPkg from 'trzsz';
const { TrzszFilter } = trzszPkg;

/** 用 Node 内置 libzstd 构造流式压缩器（与 Rust 侧一致：windowLog=17、每条消息 ZSTD_e_flush、首块补空 Raw 块到 >= 18 字节）。 */
function makeStreamEncoder() {
  const c = zlib.createZstdCompress({
    params: {
      [zlib.constants.ZSTD_c_compressionLevel]: 3,
      [zlib.constants.ZSTD_c_windowLog]: 17,
    },
  });
  let first = true;
  return async (input) => {
    const buf = typeof input === 'string' ? Buffer.from(input) : Buffer.from(input);
    if (buf.length === 0) return new Uint8Array(0);
    const parts = [];
    const onData = (d) => parts.push(d);
    c.on('data', onData);
    await new Promise((resolve, reject) => {
      const done = () => {
        c.off('error', reject);
        resolve();
      };
      c.once('error', reject);
      c.write(buf, (err) => {
        if (err) {
          c.off('error', reject);
          return reject(err);
        }
        c.flush(zlib.constants.ZSTD_e_flush, done);
      });
    });
    c.off('data', onData);
    let out = Buffer.concat(parts);
    if (first) {
      first = false;
      while (out.length < 18) {
        out = Buffer.concat([out, Buffer.from([0x00, 0x00, 0x00])]);
      }
    }
    return new Uint8Array(out.buffer, out.byteOffset, out.byteLength);
  };
}

test('wsProtocols negotiates tty-zstd with fallback to tty', () => {
  assert.deepEqual(wsProtocols(), [ZSTD_SUBPROTOCOL, 'tty']);
  assert.deepEqual(wsProtocols(true), [ZSTD_SUBPROTOCOL, 'tty']);
  assert.deepEqual(wsProtocols(false), ['tty']);
});

test('TermZstdDecoder decodes each flushed chunk immediately (including 1-byte first chunk and >128KB chunk)', async () => {
  const encode = makeStreamEncoder();
  const dec = new TermZstdDecoder();
  const td = new TextDecoder();

  const samples = [
    // 首条仅 1 字节
    new Uint8Array([0x61]),
    Buffer.from('hello world\r\n'),
    Buffer.from('\x1b[32muser@host\x1b[0m$ ls -la\r\n'.repeat(50)),
    // 第二次重复输出，验证跨消息共享上下文回引
    Buffer.from('\x1b[32muser@host\x1b[0m$ ls -la\r\n'.repeat(50)),
    // 超过单个 128 KiB 块（触发单次 push 多次 ondata 回调）
    Buffer.from('0123456789abcdef\n'.repeat(12000)),
    // 中文与 trzsz 魔术串
    Buffer.from('中文输出 ::TRZSZ:TRANSFER:R:1.1.6:0\r\n'),
  ];

  for (const s of samples) {
    const z = await encode(s);
    const out = dec.decode(z);
    assert.deepEqual(Buffer.from(out), Buffer.from(s));
  }

  // 空输入返回空 Uint8Array
  assert.equal(dec.decode(new Uint8Array(0)).length, 0);
  // 也接受 ArrayBuffer
  const zLast = await encode('final line\r\n');
  const ab = zLast.buffer.slice(zLast.byteOffset, zLast.byteOffset + zLast.byteLength);
  assert.equal(td.decode(dec.decode(ab)), 'final line\r\n');
});

test('TermZstdDecoder reset rebuilds context for reconnect and does not leak fzstd buffer list', async () => {
  const dec = new TermZstdDecoder();
  const enc1 = makeStreamEncoder();
  for (let i = 0; i < 200; i++) {
    const plain = `line ${i}: \x1b[33mwarning\x1b[0m something happened\r\n`;
    const z = await enc1(plain);
    assert.equal(new TextDecoder().decode(dec.decode(z)), plain);
  }
  // 确认清理了 fzstd 0.1.1 的内部空块队列，不泄漏历史 ArrayBuffer 引用
  assert.equal(dec._dec.c.length, 0);

  // 模拟重连：服务端新建压缩上下文，前端 reset 重建解压上下文
  dec.reset();
  const enc2 = makeStreamEncoder();
  const z2 = await enc2('after reconnect\r\n');
  assert.equal(new TextDecoder().decode(dec.decode(z2)), 'after reconnect\r\n');
});

test('decompressed output feeds TrzszFilter and preserves trzsz magic detection', async () => {
  const encode = makeStreamEncoder();
  const dec = new TermZstdDecoder();
  const written = [];
  const sent = [];
  const filter = new TrzszFilter({
    writeToTerminal: (d) => written.push(typeof d === 'string' ? d : new TextDecoder().decode(d)),
    sendToServer: (d) => sent.push(d),
    chooseSendFiles: async () => [],
    chooseSaveDirectory: async () => '',
    terminalColumns: 80,
  });

  const normalZ = await encode('prompt$ echo hi\r\nhi\r\n');
  filter.processServerOutput(dec.decode(normalZ));
  assert.equal(written.join(''), 'prompt$ echo hi\r\nhi\r\n');

  const trzszZ = await encode('::TRZSZ:TRANSFER:R:1.1.6:0\n');
  filter.processServerOutput(dec.decode(trzszZ));
  await new Promise((r) => setTimeout(r, 30));
  assert.ok(sent.some((s) => typeof s === 'string' && s.startsWith('#ACT:')), 'expected trzsz #ACT handshake');
});
