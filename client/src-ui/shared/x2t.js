// 旧版 Office（doc / ppt）→ docx / pptx：在浏览器的 Web Worker 里跑 x2t（CryptPad 编译的 OnlyOffice 转换器 WASM，AGPL-3.0）。
// 转换器由电脑第一次用到时下载（src/office.rs），这里经现有连接（fetchRaw，远程也一样）取来，能用 Cache Storage 时按版本存下，
// 下次不再取。docs/FILES_REDO_DESIGN_2026-11-24.md §4.4.1。

import { fetchRaw } from './api.js';

const TARGET = { doc: 'docx', ppt: 'pptx' };
/** 输入 / 输出上限（和 browser-office-converter 一样） */
export const X2T_MAX = 50 * 1024 * 1024;
const CONVERT_TIMEOUT = 120_000;

// Worker 用 Blob 建（不另外发一个资源文件），远程时不用经过 Service Worker / 资源镜像。
// 收到 { js, wasmGz, buffer, format, output }，回 { buffer } 或 { error }。每次转换一个新 Worker，用完就关。
const WORKER = `
self.onmessage = async ({ data }) => {
  try {
    const wasm = await new Response(new Blob([data.wasmGz]).stream().pipeThrough(new DecompressionStream('gzip'))).arrayBuffer();
    await new Promise((resolve, reject) => {
      self.Module = { wasmBinary: wasm, onRuntimeInitialized: resolve, onAbort: reject, print() {}, printErr() {} };
      importScripts(URL.createObjectURL(new Blob([data.js], { type: 'text/javascript' })));
    });
    const m = self.Module;
    for (const p of ['/working', '/working/fonts', '/working/themes']) m.FS.mkdir(p);
    const input = '/working/input.' + data.format;
    const output = '/working/output.' + data.output;
    m.FS.writeFile(input, new Uint8Array(data.buffer));
    m.FS.writeFile('/working/params.xml', '<?xml version="1.0"?><TaskQueueDataConvert><m_sFileFrom>' + input + '</m_sFileFrom><m_sFileTo>' + output +
      '</m_sFileTo><m_sFontDir>/working/fonts/</m_sFontDir><m_sThemeDir>/working/themes</m_sThemeDir></TaskQueueDataConvert>');
    const code = m.ccall('main1', 'number', ['string'], ['/working/params.xml']);
    if (code !== 0) throw new Error('x2t ' + code);
    if (m.FS.stat(output).size > ${X2T_MAX}) throw new Error('tooLarge');
    const buffer = m.FS.readFile(output).slice().buffer;
    self.postMessage({ buffer }, [buffer]);
  } catch (e) {
    self.postMessage({ error: String(e && e.message || e) });
  }
};`;

/** 转换器在 Cache Storage 里的键（只是键，不会真的去请求这个地址） */
const cacheKey = (version, name) => `/__ll_x2t/${version}/${name}`;

async function cached(version, name) {
  try {
    const c = await caches.open('ll-x2t');
    const r = await c.match(cacheKey(version, name));
    return r ? await r.arrayBuffer() : null;
  } catch {
    return null; // 不是安全上下文（局域网 http）、隐私模式：没有 caches
  }
}

async function store(version, files) {
  try {
    const c = await caches.open('ll-x2t');
    // 换版本时清掉旧的
    for (const req of await c.keys()) if (!new URL(req.url).pathname.startsWith(`/__ll_x2t/${version}/`)) await c.delete(req);
    for (const [name, buf] of Object.entries(files)) await c.put(cacheKey(version, name), new Response(buf));
  } catch {
    /* 存不下就下次再取 */
  }
}

/** 边收边报进度（远程时 14 MB 要一会儿） */
async function readAll(res, onBytes) {
  const total = Number(res.headers.get('content-length')) || null;
  if (!res.body || !onBytes) return res.arrayBuffer();
  const reader = res.body.getReader();
  const parts = [];
  let done = 0;
  for (;;) {
    const { done: end, value } = await reader.read();
    if (end) break;
    parts.push(value);
    done += value.length;
    onBytes(done, total);
  }
  return new Blob(parts).arrayBuffer();
}

let inMemory = null; // { version, js, wasmGz }：这一页里转第二个文件时不再取

/**
 * 取到转换器：浏览器里有就用；没有时电脑上没下载过的先让电脑下载（报进度），再取过来。
 * `ops`：发到文件所在电脑的接口（deviceApi）；`device`：同一台电脑，给 fetchRaw。
 * `onStage({ stage: 'download' | 'unpack' | 'fetch' | 'convert', done?, total? })`。
 */
async function engine(ops, device, onStage, signal) {
  let st;
  try {
    st = await ops.get('/office/engine');
  } catch (e) {
    // 那台电脑上的看看还没有这个接口
    if (e.status === 404) throw Object.assign(new Error('old client'), { code: 'X2T_CLIENT_OLD' });
    throw e;
  }
  const { version } = st;
  if (inMemory?.version === version) return inMemory;
  const [js0, gz0] = await Promise.all([cached(version, 'x2t.js'), cached(version, 'x2t.wasm.gz')]);
  if (js0 && gz0) return (inMemory = { version, js: new TextDecoder().decode(js0), wasmGz: gz0 });
  if (!st.ready) {
    if (!st.running) st = await ops.post('/office/engine');
    while (!st.ready) {
      if (st.error && !st.running) throw Object.assign(new Error(st.error), { code: 'X2T_DOWNLOAD_FAILED' });
      // 下完了还在跑 = 电脑在校验、解压
      onStage?.(st.total && st.done >= st.total ? { stage: 'unpack' } : { stage: 'download', done: st.done, total: st.total });
      await new Promise((r) => setTimeout(r, 1000));
      if (signal?.aborted) throw new DOMException('aborted', 'AbortError');
      st = await ops.get('/office/engine');
    }
  }
  const opt = { device: device ?? undefined, signal, cache: 'default' };
  const js = await (await fetchRaw('/office/engine/x2t.js', opt)).arrayBuffer();
  const wasmGz = await readAll(await fetchRaw('/office/engine/x2t.wasm.gz', opt), (done, total) => onStage?.({ stage: 'fetch', done, total }));
  await store(version, { 'x2t.js': js, 'x2t.wasm.gz': wasmGz });
  return (inMemory = { version, js: new TextDecoder().decode(js), wasmGz });
}

/** doc / ppt 的 ArrayBuffer → { kind: 'docx' | 'pptx', buf } */
export async function convertLegacy(buf, kind, { ops, device, onStage, signal } = {}) {
  const output = TARGET[kind];
  if (!output) throw new Error(`unsupported ${kind}`);
  if (buf.byteLength > X2T_MAX) throw Object.assign(new Error('too large'), { code: 'X2T_TOO_LARGE' });
  const eng = await engine(ops, device, onStage, signal);
  onStage?.({ stage: 'convert' });
  const url = URL.createObjectURL(new Blob([WORKER], { type: 'text/javascript' }));
  const worker = new Worker(url);
  try {
    const out = await new Promise((resolve, reject) => {
      const timer = setTimeout(() => reject(Object.assign(new Error('timeout'), { code: 'X2T_FAILED' })), CONVERT_TIMEOUT);
      signal?.addEventListener('abort', () => reject(new DOMException('aborted', 'AbortError')), { once: true });
      worker.onmessage = ({ data }) => {
        clearTimeout(timer);
        if (data.error) reject(Object.assign(new Error(data.error), { code: 'X2T_FAILED' }));
        else resolve(data.buffer);
      };
      worker.onerror = (e) => {
        clearTimeout(timer);
        reject(Object.assign(new Error(e.message || 'worker'), { code: 'X2T_FAILED' }));
      };
      // wasm 留一份在内存里（转下一个文件用），给 Worker 的是副本
      worker.postMessage({ js: eng.js, wasmGz: eng.wasmGz.slice(0), buffer: buf, format: kind, output }, [buf]);
    });
    return { kind: output, buf: out };
  } finally {
    worker.terminate();
    URL.revokeObjectURL(url);
  }
}
