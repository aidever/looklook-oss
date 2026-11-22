// 终端 WebSocket 输出流式 zstd 解压（与主进程 gateway/term_zstd.rs 配套）。
// 整条连接共用一个 Decompress 上下文；服务端每条 OUTPUT 消息 flush 一次，这里收到即解出。
import { Decompress } from 'fzstd';

export const ZSTD_SUBPROTOCOL = 'tty-zstd';

/** WebSocket 握手子协议列表：优先协商 tty-zstd，旧后端或关闭压缩时回退到 tty。 */
export function wsProtocols(enableZstd = true) {
  return enableZstd ? [ZSTD_SUBPROTOCOL, 'tty'] : ['tty'];
}

export class TermZstdDecoder {
  constructor() {
    this.reset();
  }

  /** 重连时重建全新的流式解压上下文。 */
  reset() {
    this._chunks = [];
    this._dec = new Decompress((chunk) => {
      if (chunk.length > 0) this._chunks.push(chunk);
    });
  }

  /**
   * 解压单条 OUTPUT 消息的压缩负载（不含首字节 '0'），返回完整原始字节。
   * @param {Uint8Array | ArrayBuffer} input
   * @returns {Uint8Array}
   */
  decode(input) {
    const bytes = input instanceof Uint8Array ? input : new Uint8Array(input);
    if (bytes.length === 0) return new Uint8Array(0);
    this._chunks.length = 0;
    this._dec.push(bytes, false);
    // fzstd 0.1.1 在块边界返回时会往内部队列 c 里塞一个 0 长度 subarray，
    // l 为 0 时不会清理它，长期连接会累积并持有所有历史 ArrayBuffer 引用。
    if (this._dec.l === 0 && Array.isArray(this._dec.c) && this._dec.c.length > 0) {
      this._dec.c.length = 0;
    }
    const chunks = this._chunks;
    if (chunks.length === 0) return new Uint8Array(0);
    if (chunks.length === 1) {
      const out = chunks[0];
      chunks.length = 0;
      return out;
    }
    let total = 0;
    for (const c of chunks) total += c.length;
    const out = new Uint8Array(total);
    let offset = 0;
    for (const c of chunks) {
      out.set(c, offset);
      offset += c.length;
    }
    chunks.length = 0;
    return out;
  }
}
