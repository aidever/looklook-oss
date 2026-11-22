// 到 termd 的连接（looklook-term.v1，设计稿 §5）：
// - 实时通道 /i/{id}/term：下行是整条连接共用一个上下文的 zstd 流（每条消息 flush），这里用 fzstd 解压后交给 WASM（term-web）
//   拆包、解码、还原屏幕；上行是 WASM 编好的 bincode 消息，不压缩；
// - 回滚通道 /i/{id}/term-history：按需取回滚行，和实时画面分开，不会堵住画面；
// - 断线后带着帧号和已确认的输入序号重连（Hello），termd 接着发差量或一个完整画面；没被确认的输入重发，termd 按序号去重。
// - 本地预测回显（§5.7）：量“输入 → termd 确认”的往返时间，超过 30 ms 时让 WASM 预测（地址上 ?predict=on / off 可强制）；
// - trzsz（§5.8）：termd 认出 trzsz 握手后把原始字节发过来（`raw`），交给 ../termd-trzsz.js。
// window.WebSocket 在连接时现取：term-inject.js 把它换成了显示连接状态（远程时走 WebRTC）的版本。

import { TermZstdDecoder } from '../zstd.js';

export const PROTOCOL = 'looklook-term.v1';

function clientId(base) {
  // 同一个标签页里重连用同一个编号：termd 据此保留上次的差量编码器（接着发差量，不用重发整屏）
  const key = 'lld-client:' + base;
  try {
    let v = sessionStorage.getItem(key);
    if (!v) {
      v = Math.random().toString(36).slice(2) + Date.now().toString(36);
      sessionStorage.setItem(key, v);
    }
    return v;
  } catch {
    return Math.random().toString(36).slice(2);
  }
}

export class TermConnection {
  /**
   * @param {string} base 终端地址（/i/{id}，不带结尾斜杠）
   * @param {{ wasm(): Promise<{ default: Function, TermClient: Function }>, keepHistory?: boolean,
   *   onUpdates(list: object[]): void, onState?(open: boolean, ended?: boolean): void }} handlers
   *   wasm：加载哪个 WASM（手机 term_web，电脑 term_gpu，后者也导出 TermClient）；keepHistory：回滚行留在 WASM 里；
   *   trzsz：{ windows, dragInitTimeout } 时支持 trz / tsz；onPredict：打字后预测变了（要重画）
   */
  constructor(base, handlers) {
    const proto = location.protocol === 'https:' ? 'wss:' : 'ws:';
    this.wsUrl = proto + '//' + location.host + base + '/term';
    this.historyUrl = proto + '//' + location.host + base + '/term-history';
    this.tokenUrl = location.protocol + '//' + location.host + base + '/token';
    this.handlers = handlers;
    this.clientId = clientId(base);
    this.enc = new TextEncoder();
    this.zstd = new TermZstdDecoder();
    this.tc = null;
    this.socket = null;
    this.size = { cols: 80, rows: 24 };
    this.view = 0;
    this.everOpen = false;
    this.closed = false;
    this.hist = null; // 回滚通道
    this.histReqs = new Map(); // request_id → resolve
    this.histSeq = 1;
    this.sentAt = []; // [输入序号, 发出时间]，量往返
    this.srtt = 0;
    const p = new URLSearchParams(location.search).get('predict');
    this.predictForce = p === 'on' ? true : p === 'off' ? false : null;
    this.trz = null; // TermdTrzsz
    this.trzLoading = null;
  }

  async start(cols, rows) {
    const mod = await this.handlers.wasm();
    await mod.default();
    this.mod = mod;
    this.tc = new mod.TermClient();
    if (this.handlers.keepHistory) this.tc.keep_history();
    this.size = { cols, rows };
    this.connect(false);
  }

  connect(resume) {
    if (this.closed) return;
    const WS = window.WebSocket;
    const ws = (this.socket = new WS(this.wsUrl, [PROTOCOL]));
    ws.binaryType = 'arraybuffer';
    ws.addEventListener('open', () => {
      if (ws !== this.socket) return;
      this.zstd.reset();
      this.tc.reset_stream(resume);
      ws.send(this.tc.hello(this.size.cols, this.size.rows, this.clientId, resume));
      // 断线前没被 termd 确认的输入重发（termd 按序号去重，不会打两遍）
      for (let i = 0; i < this.tc.pending_inputs(); i++) ws.send(this.tc.pending_input(i));
      if (this.view) ws.send(this.tc.view(this.view));
      this.everOpen = true;
      this.handlers.onState?.(true);
    });
    ws.addEventListener('message', (e) => {
      if (ws !== this.socket || !(e.data instanceof ArrayBuffer)) return;
      let plain;
      try {
        plain = this.zstd.decode(new Uint8Array(e.data));
      } catch (err) {
        console.error('[looklook] term stream decode error, reconnecting', err);
        this.resync();
        return;
      }
      if (!plain.length) return;
      const list = JSON.parse(this.tc.feed(plain));
      if (list.some((u) => u.t === 'resync')) {
        this.resync();
        return;
      }
      let screen = false;
      for (const u of list) {
        if (u.t === 'raw') this.onRaw(u.id, this.tc.take_raw());
        else if (u.t === 'welcome') this.onWelcome(u);
        else if (u.t === 'screen') screen = true;
      }
      if (screen) this.measure();
      if (list.length) this.handlers.onUpdates(list);
    });
    ws.addEventListener('close', (e) => {
      if (ws !== this.socket) return;
      this.socket = null;
      // 1000：会话结束（程序退出、被停止）。其他情况退避后重连：取 /token 这一步由 term-inject.js 控制退避节奏
      const ended = e.code === 1000;
      this.handlers.onState?.(false, ended);
      if (ended || this.closed) return;
      fetch(this.tokenUrl).catch(() => {}).finally(() => this.connect(true));
    });
  }

  /** 帧对不上、解压出错：丢掉本地画面，重连要一个完整画面 */
  resync() {
    const ws = this.socket;
    this.socket = null;
    try {
      ws?.close(4000, 'resync');
    } catch {
      // 忽略
    }
    this.tc.reset_stream(false);
    this.connect(false);
  }

  /** 会话结束后用户要求重新连上 */
  reopen() {
    if (this.socket) return;
    this.connect(false);
  }

  send(bytes) {
    const ws = this.socket;
    if (ws && ws.readyState === 1) ws.send(bytes);
  }

  /**
   * 按键、粘贴（字符串或字节）。没连上时也记下来，连上后重发。
   * typed：键盘打的字（不是粘贴、不是编好的转义），可以本地预测回显
   */
  input(data, typed = false) {
    if (!this.tc) return;
    // trzsz 传文件时键盘输入归它（Ctrl+C 取消传输）
    if (this.trz?.transferring()) {
      this.trz.terminalInput(typeof data === 'string' ? data : new TextDecoder().decode(data));
      return;
    }
    this.sendInput(data, typed);
  }

  sendInput(data, typed = false) {
    const bytes = typeof data === 'string' ? this.enc.encode(data) : data;
    if (!bytes.length) return;
    const v = this.tc.predict_version();
    this.send(typed ? this.tc.input_typed(bytes) : this.tc.input(bytes));
    const seq = this.tc.last_input_seq();
    this.sentAt.push([seq, performance.now()]);
    if (this.sentAt.length > 64) this.sentAt.shift();
    if (this.tc.predict_version() !== v) this.handlers.onPredict?.();
  }

  /** 一帧到了：“输入 → 确认”的往返时间（平滑），决定开不开预测 */
  measure() {
    const acked = this.tc.acked_input_seq();
    let sample = null;
    while (this.sentAt.length && this.sentAt[0][0] <= acked) sample = this.sentAt.shift()[1];
    if (sample !== null) {
      const rtt = performance.now() - sample;
      this.srtt = this.srtt ? this.srtt * 0.8 + rtt * 0.2 : rtt;
    }
    const on = this.predictForce ?? this.srtt > 30;
    if (on !== this.tc.predicting()) this.tc.set_predict(on);
  }

  // ---------- trzsz ----------

  onWelcome(u) {
    const opts = this.handlers.trzsz;
    if (!opts || !(u.caps || '').split(' ').includes('trzsz')) return;
    this.send(this.tc.features(1));
    if (!this.trzLoading) {
      this.trzLoading = import('../termd-trzsz.js').then(({ TermdTrzsz }) => {
        this.trz = new TermdTrzsz(this, { ...opts, columns: this.size.cols });
        for (const [id, bytes] of this.rawEarly || []) this.trz.onRaw(id, bytes);
        this.rawEarly = null;
        return this.trz;
      });
    }
  }

  onRaw(id, bytes) {
    if (this.trz) this.trz.onRaw(id, bytes);
    else if (this.trzLoading) (this.rawEarly ||= []).push([id, bytes]);
    else this.send(this.tc.raw_progress(id, bytes.length, bytes, true)); // 不该发生：原样上屏
  }

  /** 拖到终端上的文件：自动敲 trz 上传（没开 trzsz 返回 false） */
  uploadFiles(items) {
    if (!this.trz) return false;
    this.trz.upload(items);
    return true;
  }

  resize(cols, rows) {
    if (this.size.cols === cols && this.size.rows === rows) return;
    this.size = { cols, rows };
    this.trz?.setColumns(cols);
    if (this.tc) this.send(this.tc.resize(cols, rows));
  }

  /** 这一帧画完了：termd 收到才发下一帧（发帧节奏跟着端上的处理速度走，§5.3） */
  ack() {
    if (this.tc) this.send(this.tc.ack());
  }

  /** 0 前台 / 1 后台 / 2 看不见 */
  setView(v) {
    if (this.view === v) return;
    this.view = v;
    if (this.tc) this.send(this.tc.view(v));
  }

  /** kind：0 按下 1 抬起 2 拖动 3 移动 4 上滚 5 下滚 */
  mouse(kind, button, col, row, mods = 0) {
    if (this.tc) this.send(this.tc.mouse(kind, button, col, row, mods));
  }

  // ---------- 回滚区 ----------

  historySocket() {
    if (this.hist && this.hist.readyState <= 1) return this.hist;
    const WS = window.WebSocket;
    const ws = (this.hist = new WS(this.historyUrl, [PROTOCOL]));
    ws.binaryType = 'arraybuffer';
    const zstd = new TermZstdDecoder();
    ws.addEventListener('message', (e) => {
      if (!(e.data instanceof ArrayBuffer)) return;
      let list;
      try {
        list = JSON.parse(this.tc.feed_history(zstd.decode(new Uint8Array(e.data))));
      } catch (err) {
        console.error('[looklook] history decode error', err);
        ws.close();
        return;
      }
      for (const u of list) {
        const done = u.t === 'history' && this.histReqs.get(u.id);
        if (done) {
          this.histReqs.delete(u.id);
          done(u);
        }
      }
    });
    ws.addEventListener('close', () => {
      if (this.hist === ws) this.hist = null;
      // 新的回滚通道要从头拆包
      this.tc?.reset_history?.();
      for (const done of this.histReqs.values()) done(null);
      this.histReqs.clear();
    });
    return ws;
  }

  /**
   * 取一段回滚行：[start, start + count)。返回 { epoch, start, oldest, next, mismatch, rows: [{ seq, w, r }] }，失败为 null。
   */
  history(epoch, start, count) {
    if (!this.tc) return Promise.resolve(null);
    const ws = this.historySocket();
    const id = this.histSeq++;
    return new Promise((resolve) => {
      this.histReqs.set(id, resolve);
      const send = () => ws.send(this.tc.history_req(id, epoch, start, count));
      if (ws.readyState === 1) send();
      else ws.addEventListener('open', send, { once: true });
      setTimeout(() => {
        if (this.histReqs.delete(id)) resolve(null);
      }, 15000);
    });
  }

  close() {
    this.closed = true;
    try {
      this.socket?.close(1000);
      this.hist?.close(1000);
    } catch {
      // 忽略
    }
  }
}
