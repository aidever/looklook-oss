// 对话回复里的文件路径变成能点开的链接（设计稿 §5.1）：正文和行内代码里像路径的字（`src/a.rs`、`a.rs:12`、`:12:5`、
// `#L12-L20`、`~/x`、`C:\x`），先一批问电脑哪些真的存在（/fs/exists，相对路径按会话目录算），存在的文件才换成链接。
// 工作目录外的也给链接，标“工作目录外”。代码块、已经是链接的不动。

// 前面不能紧挨着字母、斜杠、冒号、点（排除网址、版本号的中间）；后面不能紧跟字母或斜杠
const NAME = "[\\w@+.\\-\\u3400-\\u9fff]";
const PATH_RE = new RegExp(
  `(?<![\\w/\\\\:.\\-])((?:[A-Za-z]:[\\\\/]|~[\\\\/]|\\.{1,2}[\\\\/]|[\\\\/])?(?:${NAME}+[\\\\/])*${NAME}+)(?::(\\d+)(?::\\d+)?(?:-(\\d+))?|#L(\\d+)(?:-L?(\\d+))?)?(?![\\w/\\\\])`,
  'g',
);
const EXT = /\.[A-Za-z0-9]{1,10}$/;
const MAX_PER_BLOCK = 120;

/** 一个词是不是值得去问的路径：带分隔符，或者像 `name.ext`；排除纯数字、版本号、网址、太长的 */
export function pathLike(p) {
  if (!p || p.length > 300 || p.length < 3) return false;
  if (/^[\d.]+$/.test(p) || /^v?\d+(\.\d+)+/.test(p)) return false;
  if (/^[\\/]+$/.test(p) || p.startsWith('//')) return false;
  const sep = /[\\/]/.test(p);
  if (!sep) return EXT.test(p) && !/^\.[A-Za-z0-9]+$/.test(p) ? !/^(e\.g|i\.e|etc)\.?$/i.test(p) : false;
  return true;
}

/** 文字里的候选：[{ index, length, path, from, to }]（from / to：行号，没有为 0） */
export function scanPaths(text) {
  const out = [];
  PATH_RE.lastIndex = 0;
  for (let m = PATH_RE.exec(text); m; m = PATH_RE.exec(text)) {
    // 句末的点不算路径的一部分（“看 src/a.rs.”）
    let path = m[1];
    let len = m[0].length;
    if (!m[2] && !m[4]) {
      const trimmed = path.replace(/[.\-]+$/, '');
      len -= path.length - trimmed.length;
      path = trimmed;
    }
    if (!pathLike(path)) continue;
    const from = Number(m[2] ?? m[4] ?? 0);
    const to = Number(m[3] ?? m[5] ?? 0) || from;
    out.push({ index: m.index, length: len, path, from, to: Math.max(from, to) });
  }
  return out;
}

/** 回复的 DOM 里要找的文字节点：不在链接、代码块里 */
function textNodes(root) {
  const out = [];
  const walker = document.createTreeWalker(root, NodeFilter.SHOW_TEXT);
  for (let n = walker.nextNode(); n; n = walker.nextNode()) {
    const p = n.parentElement;
    if (!p || p.closest('a, pre, .md-code, .md-mermaid, svg')) continue;
    if (/[\\/.]/.test(n.data)) out.push(n);
  }
  return out;
}

/**
 * 把 root 里的路径换成链接。resolve(paths) → Promise<{ [写法]: { path, dir } }>（存在的才有）；
 * href(绝对路径, from, to) → 地址；outside(绝对路径) → 不在工作目录里时的标注文字（在里面返回空）。
 * 返回 false 表示这次没换（被 alive() 叫停或者没有候选）。
 */
export async function linkPaths(root, { resolve, href, outside, alive }) {
  const nodes = textNodes(root);
  const found = [];
  for (const node of nodes) {
    for (const c of scanPaths(node.data)) {
      found.push({ node, ...c });
      if (found.length >= MAX_PER_BLOCK) break;
    }
    if (found.length >= MAX_PER_BLOCK) break;
  }
  if (!found.length) return false;
  const hits = await resolve([...new Set(found.map((f) => f.path))]);
  if (!alive() || !hits) return false;
  const byNode = new Map();
  for (const f of found) {
    const hit = hits[f.path];
    if (!hit || hit.dir) continue;
    if (!byNode.has(f.node)) byNode.set(f.node, []);
    byNode.get(f.node).push({ ...f, abs: hit.path });
  }
  for (const [node, list] of byNode) {
    if (!node.isConnected) continue;
    const frag = document.createDocumentFragment();
    let at = 0;
    for (const f of list) {
      if (f.index < at) continue;
      if (f.index > at) frag.append(node.data.slice(at, f.index));
      const a = document.createElement('a');
      a.className = 'md-file md-path';
      a.setAttribute('href', href(f.abs, f.from, f.to));
      a.setAttribute('title', f.abs);
      const out = outside(f.abs);
      if (out) a.dataset.out = out;
      a.textContent = node.data.slice(f.index, f.index + f.length);
      frag.append(a);
      at = f.index + f.length;
    }
    if (at < node.data.length) frag.append(node.data.slice(at));
    node.replaceWith(frag);
  }
  return true;
}

/**
 * 一批批问电脑哪些路径存在：同一刻的请求合在一起发，结果按会话记住（不存在的 30 秒后再问，AI 可能刚建好）。
 * ask(paths) → Promise<items>（/fs/exists 的 items）；旧版看看没有这个接口时 ask 抛出 404，以后不再问。
 */
export function pathResolver(ask) {
  const cache = new Map();
  let queue = new Map();
  let timer = 0;
  let off = false;
  const flush = async () => {
    timer = 0;
    const batch = queue;
    queue = new Map();
    const paths = [...batch.keys()];
    let items = [];
    try {
      for (let i = 0; i < paths.length; i += 200) items = items.concat((await ask(paths.slice(i, i + 200))) ?? []);
    } catch (e) {
      if (e?.status === 404) off = true;
    }
    const got = new Map(items.map((x) => [x.input, x]));
    for (const [p, done] of batch) {
      const hit = got.get(p) ?? null;
      cache.set(p, { hit, at: Date.now() });
      done(hit);
    }
  };
  const one = (p) => {
    const c = cache.get(p);
    if (c && (c.hit || Date.now() - c.at < 30_000)) return Promise.resolve(c.hit);
    if (off) return Promise.resolve(null);
    return new Promise((done) => {
      const prev = queue.get(p);
      queue.set(p, prev ? (h) => (prev(h), done(h)) : done);
      if (!timer) timer = setTimeout(flush, 40);
    });
  };
  return async (paths) => {
    const res = await Promise.all(paths.map(one));
    const out = {};
    paths.forEach((p, i) => {
      if (res[i]) out[p] = res[i];
    });
    return out;
  };
}
