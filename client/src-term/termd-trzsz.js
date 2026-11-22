// termd 会话里的 trzsz（设计稿 §5.8）：termd 认出 trzsz 握手后，从握手起的 PTY 输出不进内核，原样（`raw`）发给
// 发起传输的这个端；这里交给 trzsz 的 TrzszFilter（和 xterm 那条路同一个库）。
// - TrzszFilter 要写到屏幕上的字（进度条、不是传输的输出）发回 termd 喂给内核，所有端都看得到；
// - 隔一会儿告诉 termd 处理到哪了（它据此丢掉缓冲），传完了（或者根本不是 trzsz：1 秒内没开始传）发 done，
//   termd 把还没处理的字节喂给内核，回到正常模式。done 之后同一段迟到的字节不管（termd 自己喂）。
import { TrzszFilter } from 'trzsz';

const REPORT_BYTES = 256 * 1024;
const NOT_TRZSZ_MS = 1000;

function concat(parts) {
  if (parts.length === 1) return parts[0];
  let n = 0;
  for (const p of parts) n += p.length;
  const out = new Uint8Array(n);
  let o = 0;
  for (const p of parts) {
    out.set(p, o);
    o += p.length;
  }
  return out;
}

export class TermdTrzsz {
  /** @param {import('./dom/conn.js').TermConnection} conn */
  constructor(conn, { windows, dragInitTimeout, columns }) {
    this.conn = conn;
    this.enc = new TextEncoder();
    this.filter = new TrzszFilter({
      writeToTerminal: (d) => this.write(d),
      sendToServer: (d) => conn.sendInput(typeof d === 'string' ? d : new Uint8Array(d)),
      terminalColumns: columns,
      isWindowsShell: !!windows,
      dragInitTimeout,
    });
    this.id = 0; // 当前这一段
    this.ended = new Set(); // 发过 done 的段
    this.consumed = 0;
    this.reported = 0;
    this.writes = [];
    this.flushTimer = 0;
    this.watch = 0;
  }

  transferring() {
    return this.filter.isTransferringFiles();
  }

  terminalInput(text) {
    this.filter.processTerminalInput(text);
  }

  setColumns(cols) {
    this.filter.setTerminalColumns(cols);
  }

  upload(items) {
    this.filter
      .uploadFiles(items)
      .then(() => console.log('[looklook] trzsz upload success'))
      .catch((err) => console.log('[looklook] trzsz upload failed: ' + err));
  }

  onRaw(id, bytes) {
    if (this.ended.has(id)) return;
    if (id !== this.id) {
      if (this.id && !this.ended.has(this.id)) this.finish();
      this.id = id;
      this.consumed = 0;
      this.reported = 0;
      this.started = Date.now();
      this.saw = false;
      clearInterval(this.watch);
      this.watch = setInterval(() => this.check(), 100);
    }
    this.consumed += bytes.length;
    try {
      this.filter.processServerOutput(bytes);
    } catch (e) {
      console.error('[looklook] trzsz consume', e);
    }
    this.flushSoon();
  }

  write(d) {
    const bytes = typeof d === 'string' ? this.enc.encode(d) : d instanceof Uint8Array ? d : new Uint8Array(d);
    if (!bytes.length) return;
    this.writes.push(bytes);
    this.flushSoon();
  }

  flushSoon() {
    if (!this.flushTimer) this.flushTimer = setTimeout(() => this.flush(false), 0);
  }

  flush(done) {
    clearTimeout(this.flushTimer);
    this.flushTimer = 0;
    const tc = this.conn.tc;
    if (!tc || !this.id) return;
    const write = this.writes.length ? concat(this.writes) : new Uint8Array(0);
    this.writes = [];
    if (!done && !write.length && this.consumed - this.reported < REPORT_BYTES) return;
    this.reported = this.consumed;
    this.conn.send(tc.raw_progress(this.id, this.consumed, write, done));
  }

  check() {
    if (this.filter.isTransferringFiles()) {
      this.saw = true;
      return;
    }
    if (this.saw || Date.now() - this.started > NOT_TRZSZ_MS) this.finish();
  }

  finish() {
    clearInterval(this.watch);
    this.watch = 0;
    this.flush(true);
    this.ended.add(this.id);
  }
}
