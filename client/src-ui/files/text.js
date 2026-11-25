// 文字文件的读写（docs/FILES_REDO_DESIGN_2026-11-24.md §4.1）：按原编码 / 换行 / BOM 读，分段读大文件，
// 编辑后按原样存回去。新接口（/fs/read）在旧版看看上没有：退回 /fs/preview（只认 UTF-8，前 1 MiB）。

/** 旧版看看没有这个接口（找不到路由也回 NOT_FOUND，但不带 path；文件不存在时带 path） */
export const noRoute = (e) => e?.status === 404 && !e?.params?.path;

/** 编辑上限（与 textfile::EDIT_MAX 一致） */
export const EDIT_MAX = 4 * 1024 * 1024;
/** 一次读多少（与 textfile::READ_DEFAULT 一致） */
export const READ_LEN = 1024 * 1024;

/** “换编码”菜单里的编码（WHATWG 名字，后端 encoding_rs 认） */
export const ENCODINGS = ['UTF-8', 'GBK', 'gb18030', 'Big5', 'Shift_JIS', 'EUC-JP', 'EUC-KR', 'UTF-16LE', 'UTF-16BE', 'windows-1252', 'ISO-8859-1', 'KOI8-R', 'windows-1251'];

/** 旧接口的文字：按 UTF-8 解码，含 NUL 或解不出来就当二进制 */
export function decodeUtf8(buf) {
  const bytes = new Uint8Array(buf);
  if (bytes.subarray(0, 8192).includes(0)) return null;
  try {
    return new TextDecoder('utf-8', { fatal: true }).decode(bytes);
  } catch {
    // 截断可能切在多字节字符中间：去掉末尾几个字节再试
    try {
      return new TextDecoder('utf-8', { fatal: true }).decode(bytes.subarray(0, Math.max(0, bytes.length - 3)));
    } catch {
      return null;
    }
  }
}

/**
 * 读一段文字：`{ text, encoding, bom, eol, eol_major, size, mtime_ms, offset, end, line, legacy? }`。
 * `line`：从这一行附近开始（前面留 50 行）；`offset`：从这个字节开始；`encoding`：按这个编码重读。
 * 旧版看看没有 /fs/read 时退回 `ep.preview`（legacy: true，只有开头 1 MiB）。
 */
export async function readText(ops, ep, path, { offset, line, encoding, len, fetchRaw } = {}) {
  const q = new URLSearchParams({ path });
  if (offset) q.set('offset', String(offset));
  if (line) q.set('line', String(line));
  if (encoding) q.set('encoding', encoding);
  if (len) q.set('len', String(len));
  try {
    return await ops.get(`/fs/read?${q}`);
  } catch (e) {
    if (!noRoute(e) || !fetchRaw) throw e;
  }
  const res = await fetchRaw(ep.preview(path));
  const size = Number(res.headers.get('x-looklook-size') ?? 0);
  const mtime = Number(res.headers.get('x-looklook-mtime') ?? 0) || 0;
  const text = decodeUtf8(await res.arrayBuffer());
  return { text, encoding: text == null ? null : 'UTF-8', bom: false, eol: text?.includes('\r\n') ? 'crlf' : 'lf', size, mtime_ms: mtime, offset: 0, end: Math.min(size, READ_LEN), line: 1, legacy: true };
}

/** 换行的显示名 */
export const eolLabel = (eol) => ({ lf: 'LF', crlf: 'CRLF', cr: 'CR', mixed: 'Mixed' })[eol] ?? 'LF';

/** 文件现在的状态（外部改动检测用）：旧版没有接口时返回 null */
export async function statFile(ops, path) {
  try {
    return await ops.get(`/fs/stat?path=${encodeURIComponent(path)}`);
  } catch (e) {
    if (noRoute(e)) return null;
    throw e;
  }
}
