// 文件预览里的几种专门的看法（设计稿 §6）：JSON / JSONL 树、Jupyter 笔记本、压缩包目录、十六进制、HTML 网页、
// 浏览器自己显示不了 PDF 时用 pdf.js 画、Markdown 里的相对图片（远程时取回来再显示）、手机上编辑时的符号工具条。
import { ChevronRight, CornerDownLeft, FileArchive, Folder, Redo2, Undo2 } from 'lucide-react';
import { memo, useEffect, useMemo, useRef, useState } from 'react';
import { useTranslation } from 'react-i18next';

import { errorText } from '../shared/i18n.js';
import Markdown from '../shared/Markdown.jsx';
import { formatBytes } from '../shared/xfer.js';

// ---------------- JSON / JSONL 树 ----------------

/** 解析 JSON（JSONL：每行一个，最多 5000 行）；解析不了返回 { error } */
export function parseJson(text, jsonl) {
  try {
    if (!jsonl) return { value: JSON.parse(text) };
    const out = [];
    const lines = text.split(/\r?\n/);
    for (let i = 0; i < lines.length && out.length < 5000; i += 1) {
      const l = lines[i].trim();
      if (l) out.push(JSON.parse(l));
    }
    return { value: out, lines: true };
  } catch (e) {
    return { error: String(e?.message ?? e) };
  }
}

const kindOf = (v) => (v === null ? 'null' : Array.isArray(v) ? 'array' : typeof v);

function Leaf({ v }) {
  const k = kindOf(v);
  const text = k === 'string' ? JSON.stringify(v) : String(v);
  return <span className={`jt-${k}`}>{text.length > 2000 ? `${text.slice(0, 2000)}…` : text}</span>;
}

const Node = memo(function Node({ name, v, depth, open: initOpen }) {
  const k = kindOf(v);
  const [open, setOpen] = useState(initOpen);
  const [limit, setLimit] = useState(200);
  if (k !== 'object' && k !== 'array') {
    return (
      <div className="jt-row" style={{ paddingLeft: depth * 14 + 18 }}>
        {name != null && <span className="jt-key">{name}: </span>}
        <Leaf v={v} />
      </div>
    );
  }
  const entries = k === 'array' ? v.map((x, i) => [i, x]) : Object.entries(v);
  const sum = k === 'array' ? `[${entries.length}]` : `{${entries.length}}`;
  return (
    <>
      <button type="button" className="jt-row jt-toggle" style={{ paddingLeft: depth * 14 + 4 }} onClick={() => setOpen(!open)} aria-expanded={open}>
        <ChevronRight size={12} className="jt-caret" aria-hidden="true" />
        {name != null && <span className="jt-key">{name}: </span>}
        <span className="jt-sum">{sum}</span>
      </button>
      {open && entries.slice(0, limit).map(([key, x]) => <Node key={key} name={key} v={x} depth={depth + 1} open={false} />)}
      {open && entries.length > limit && (
        <button type="button" className="jt-row jt-more" style={{ paddingLeft: (depth + 1) * 14 + 18 }} onClick={() => setLimit((n) => n + 500)}>
          … {entries.length - limit}
        </button>
      )}
    </>
  );
});

export function JsonTree({ text, jsonl }) {
  const { t } = useTranslation();
  const parsed = useMemo(() => parseJson(text, jsonl), [text, jsonl]);
  if (parsed.error) return <p className="muted small fb-pad">{t('fv.json_bad', { error: parsed.error })}</p>;
  return (
    <div className="jt" role="tree">
      {parsed.lines ? parsed.value.map((x, i) => <Node key={i} name={i + 1} v={x} depth={0} open={parsed.value.length <= 20} />) : <Node v={parsed.value} depth={0} open />}
    </div>
  );
}

// ---------------- Jupyter 笔记本 ----------------

const joinSrc = (s) => (Array.isArray(s) ? s.join('') : s ?? '');

function NbOutput({ o }) {
  const data = o.data ?? {};
  if (o.output_type === 'stream') return <pre className="nb-out">{joinSrc(o.text)}</pre>;
  if (o.output_type === 'error') return <pre className="nb-out nb-err">{(o.traceback ?? []).join('\n').replace(/\x1b\[[0-9;]*m/g, '')}</pre>;
  for (const mime of ['image/png', 'image/jpeg', 'image/gif']) {
    if (data[mime]) return <img className="nb-img" alt="" src={`data:${mime};base64,${joinSrc(data[mime]).replace(/\s/g, '')}`} />;
  }
  if (data['image/svg+xml']) return <img className="nb-img" alt="" src={`data:image/svg+xml;charset=utf-8,${encodeURIComponent(joinSrc(data['image/svg+xml']))}`} />;
  if (data['text/markdown']) return <Markdown text={joinSrc(data['text/markdown'])} />;
  if (data['text/plain']) return <pre className="nb-out">{joinSrc(data['text/plain'])}</pre>;
  return null;
}

export function Notebook({ text }) {
  const { t } = useTranslation();
  const nb = useMemo(() => {
    try {
      return JSON.parse(text);
    } catch {
      return null;
    }
  }, [text]);
  if (!nb?.cells) return <p className="muted small fb-pad">{t('fv.nb_bad')}</p>;
  const lang = nb.metadata?.language_info?.name ?? nb.metadata?.kernelspec?.language ?? '';
  return (
    <div className="nb">
      {nb.cells.map((c, i) => (
        <section key={i} className={`nb-cell nb-${c.cell_type}`}>
          {c.cell_type === 'markdown' ? (
            <Markdown text={joinSrc(c.source)} />
          ) : (
            <>
              <div className="nb-in">
                <span className="nb-n">{c.cell_type === 'code' ? `[${c.execution_count ?? ' '}]` : ''}</span>
                <pre data-lang={lang}>{joinSrc(c.source)}</pre>
              </div>
              {(c.outputs ?? []).map((o, j) => (
                <NbOutput key={j} o={o} />
              ))}
            </>
          )}
        </section>
      ))}
    </div>
  );
}

// ---------------- 压缩包 ----------------

export const ARCHIVE = /\.(zip|jar|apk|whl|tar|tgz|tar\.gz)$/i;

/** 压缩包里有什么（/fs/archive），点文字文件看内容，能解压到旁边的新文件夹 */
export function ArchiveView({ ops, path, onExtracted, readEntry }) {
  const { t } = useTranslation();
  const [st, setSt] = useState({ loading: true });
  const [busy, setBusy] = useState(false);
  useEffect(() => {
    let alive = true;
    ops
      .get(`/fs/archive?path=${encodeURIComponent(path)}`)
      .then((r) => alive && setSt(r))
      .catch((error) => alive && setSt({ error }));
    return () => {
      alive = false;
    };
  }, [path]);
  const extract = async () => {
    setBusy(true);
    try {
      const r = await ops.post('/fs/archive/extract', { path });
      onExtracted?.(r.path);
    } catch (error) {
      setSt((s) => ({ ...s, extractError: error }));
    }
    setBusy(false);
  };
  if (st.loading) return <p className="muted small fb-pad">{t('fb.loading')}</p>;
  if (st.error) return <p className="small fb-pad" style={{ color: 'var(--danger)' }}>{errorText(t, st.error.status === 404 && !st.error.params?.path ? { code: 'CLIENT_TOO_OLD' } : st.error)}</p>;
  const files = st.entries.filter((e) => !e.dir);
  const total = files.reduce((n, e) => n + (e.size || 0), 0);
  return (
    <div className="arc">
      <div className="arc-head">
        <FileArchive size={15} aria-hidden="true" />
        <span className="grow small">{t('fv.arc_sum', { count: st.total, size: formatBytes(total) })}</span>
        <button type="button" className="btn btn-ghost btn-sm" onClick={extract} disabled={busy}>
          {t(busy ? 'fv.arc_extracting' : 'fv.arc_extract')}
        </button>
      </div>
      {st.extractError && <p className="small fb-pad" style={{ color: 'var(--danger)' }}>{errorText(t, st.extractError)}</p>}
      {st.truncated && <p className="muted small fb-pad">{t('fv.arc_truncated', { count: st.entries.length })}</p>}
      <ul className="arc-list">
        {st.entries.map((e) => (
          <li key={e.name}>
            {e.dir ? (
              <span className="arc-row muted">
                <Folder size={14} aria-hidden="true" />
                <span className="ellipsis grow">{e.name}</span>
              </span>
            ) : (
              <button type="button" className="arc-row" onClick={() => readEntry?.(e)} title={e.name}>
                <span className="ellipsis grow">{e.name}</span>
                <span className="muted small">{formatBytes(e.size)}</span>
              </button>
            )}
          </li>
        ))}
      </ul>
    </div>
  );
}

// ---------------- 十六进制 ----------------

/** 文件开头 64 KiB 的十六进制（/fs/head），二进制文件、认不出的格式用 */
export function HexView({ fetchHead }) {
  const { t } = useTranslation();
  const [st, setSt] = useState({ loading: true });
  useEffect(() => {
    let alive = true;
    fetchHead()
      .then(async (res) => {
        const buf = new Uint8Array(await res.arrayBuffer());
        if (alive) setSt({ buf, size: Number(res.headers.get('x-looklook-size') ?? buf.length), kind: res.headers.get('x-looklook-kind') ?? '' });
      })
      .catch((error) => alive && setSt({ error }));
    return () => {
      alive = false;
    };
  }, []);
  const text = useMemo(() => {
    if (!st.buf) return '';
    const rows = [];
    const b = st.buf;
    for (let i = 0; i < b.length; i += 16) {
      let hex = '';
      let asc = '';
      for (let j = 0; j < 16; j += 1) {
        const v = b[i + j];
        hex += v == null ? '   ' : `${v.toString(16).padStart(2, '0')} `;
        if (j === 7) hex += ' ';
        asc += v == null ? '' : v >= 32 && v < 127 ? String.fromCharCode(v) : '.';
      }
      rows.push(`${i.toString(16).padStart(8, '0')}  ${hex} ${asc}`);
    }
    return rows.join('\n');
  }, [st.buf]);
  if (st.loading) return <p className="muted small fb-pad">{t('fb.loading')}</p>;
  if (st.error) return <p className="small fb-pad" style={{ color: 'var(--danger)' }}>{errorText(t, st.error.status === 404 && !st.error.params?.path ? { code: 'CLIENT_TOO_OLD' } : st.error)}</p>;
  return (
    <>
      <p className="muted small fb-pad fb-note">
        {st.kind ? `${st.kind} · ` : ''}
        {st.size > st.buf.length ? t('fv.hex_head', { size: formatBytes(st.buf.length), total: formatBytes(st.size) }) : formatBytes(st.size)}
      </p>
      <pre className="fb-text hex">{text}</pre>
    </>
  );
}

// ---------------- HTML 网页 ----------------

export const HTML = /\.html?$/i;

/**
 * 网页预览：本机打开时用 /fs/site 把它所在的文件夹当成一个小站点（相对路径的 CSS、JS、图片都能加载），
 * 在沙盒 iframe 里跑（allow-scripts，不给 allow-same-origin：拿不到看看的 Cookie、接口）。
 * 远程、多台电脑时只能把这一页的内容放进 srcdoc，相对路径的资源显示不了。
 */
export function HtmlView({ ops, path, name, text, local }) {
  const { t } = useTranslation();
  const [src, setSrc] = useState(null);
  const [fallback, setFallback] = useState(!local);
  useEffect(() => {
    if (!local) return undefined;
    let alive = true;
    ops
      .post('/fs/site', { path })
      .then((r) => alive && setSrc(`/api/fs/site/${r.token}/${encodeURIComponent(r.name)}`))
      .catch(() => alive && setFallback(true));
    return () => {
      alive = false;
    };
  }, [path, local]);
  const sandbox = 'allow-scripts allow-popups allow-modals allow-forms';
  if (fallback) {
    return (
      <div className="fb-html">
        <p className="muted small fb-pad fb-note">{t('fv.html_srcdoc')}</p>
        <iframe title={name} sandbox={sandbox} srcDoc={text ?? ''} />
      </div>
    );
  }
  if (!src) return <p className="muted small fb-pad">{t('fb.loading')}</p>;
  return (
    <div className="fb-html">
      <iframe title={name} sandbox={sandbox} src={src} />
    </div>
  );
}

// ---------------- PDF（pdf.js） ----------------

/** 浏览器自己显示不了 PDF（安卓 Chrome 等）：用 pdf.js 一页页画出来，滚到才画 */
export function PdfView({ blob }) {
  const { t } = useTranslation();
  const box = useRef(null);
  const [st, setSt] = useState({ loading: true });
  useEffect(() => {
    let alive = true;
    let doc = null;
    let io = null;
    (async () => {
      const pdfjs = await import('pdfjs-dist');
      const worker = (await import('pdfjs-dist/build/pdf.worker.min.mjs?url')).default;
      pdfjs.GlobalWorkerOptions.workerSrc = worker;
      doc = await pdfjs.getDocument({ data: new Uint8Array(await blob.arrayBuffer()), isEvalSupported: false }).promise;
      if (!alive) return;
      setSt({ pages: doc.numPages });
      const host = box.current;
      const width = Math.max(200, host.clientWidth - 16);
      const dpr = Math.min(2, window.devicePixelRatio || 1);
      io = new IntersectionObserver(
        (ents) => {
          for (const e of ents) {
            if (!e.isIntersecting || e.target.dataset.done) continue;
            e.target.dataset.done = '1';
            const n = Number(e.target.dataset.n);
            doc.getPage(n).then((page) => {
              const vp0 = page.getViewport({ scale: 1 });
              const vp = page.getViewport({ scale: (width / vp0.width) * dpr });
              const c = document.createElement('canvas');
              c.width = vp.width;
              c.height = vp.height;
              c.style.width = `${width}px`;
              e.target.style.height = '';
              e.target.appendChild(c);
              return page.render({ canvas: c, canvasContext: c.getContext('2d'), viewport: vp }).promise;
            }).catch(() => {});
          }
        },
        { root: host, rootMargin: '600px 0px' },
      );
      const first = await doc.getPage(1);
      const ratio = first.getViewport({ scale: 1 });
      for (let n = 1; n <= doc.numPages; n += 1) {
        const d = document.createElement('div');
        d.className = 'pdf-page';
        d.dataset.n = String(n);
        d.style.height = `${(width * ratio.height) / ratio.width}px`;
        host.appendChild(d);
        io.observe(d);
      }
    })().catch(() => alive && setSt({ error: true }));
    return () => {
      alive = false;
      io?.disconnect();
      doc?.destroy();
    };
  }, [blob]);
  return (
    <div className="pdf-box" ref={box}>
      {st.loading && <p className="muted small fb-pad">{t('fb.loading')}</p>}
      {st.error && <p className="muted small fb-pad">{t('fb.office_failed')}</p>}
    </div>
  );
}

// ---------------- Markdown 里的相对图片 ----------------

/** Markdown 里的图片地址（相对的、本机绝对路径） */
export function mdImages(text) {
  const out = new Set();
  const re = /!\[[^\]]*\]\(\s*<?([^)\s>]+)>?(?:\s+"[^"]*")?\s*\)|<img[^>]+src=["']([^"']+)["']/gi;
  for (let m = re.exec(text); m; m = re.exec(text)) {
    const s = m[1] ?? m[2];
    if (s && !/^[a-z][a-z0-9+.-]*:/i.test(s) && !s.startsWith('//')) out.add(s);
  }
  return [...out].slice(0, 60);
}

/**
 * 远程（普通链接带不了 X-LL-Device）时 Markdown 里的相对图片：先一张张取回来变成 blob 地址再显示。
 * load(相对路径) → Promise<Blob>；返回 imageUrl(相对路径) 给 Markdown。
 */
export function useBlobImages(text, load, on) {
  const [map, setMap] = useState({});
  const list = useMemo(() => (on ? mdImages(text) : []), [text, on]);
  useEffect(() => {
    if (!list.length) return undefined;
    let alive = true;
    const urls = [];
    (async () => {
      let i = 0;
      const next = async () => {
        while (alive && i < list.length) {
          const src = list[i++];
          try {
            const url = URL.createObjectURL(await load(decodeURI(src.split(/[?#]/)[0])));
            urls.push(url);
            if (alive) setMap((m) => ({ ...m, [src]: url }));
          } catch {
            /* 取不到就不显示 */
          }
        }
      };
      await Promise.all([next(), next(), next()]);
    })();
    return () => {
      alive = false;
      for (const u of urls) URL.revokeObjectURL(u);
      setMap({});
    };
  }, [list]);
  return useMemo(() => (on ? (src) => map[src] ?? map[encodeURI(src)] ?? null : null), [map, on]);
}

// ---------------- 手机编辑工具条 ----------------

const KEYS = [
  ['Tab', '\t'],
  ['(', '(', ')'],
  ['[', '[', ']'],
  ['{', '{', '}'],
  ['"', '"', '"'],
  ["'", "'", "'"],
  ['`', '`', '`'],
  ['<', '<'],
  ['>', '>'],
  ['=', '='],
  [':', ':'],
  [';', ';'],
  ['/', '/'],
  ['\\', '\\'],
  ['|', '|'],
  ['-', '-'],
  ['_', '_'],
  ['#', '#'],
  ['$', '$'],
  ['*', '*'],
  ['&', '&'],
];

/**
 * 手机上编辑时贴在键盘上面的一排符号（输入法里难找的）：点了插到光标处，成对的符号有选中时包起来。
 * 按住不抢编辑器的焦点（pointerdown preventDefault），键盘不会收起。跟着 visualViewport 走，键盘弹出时不被挡。
 */
export function EditBar({ api }) {
  const { t } = useTranslation();
  const [bottom, setBottom] = useState(0);
  useEffect(() => {
    const vv = window.visualViewport;
    if (!vv) return undefined;
    const on = () => setBottom(Math.max(0, window.innerHeight - vv.height - vv.offsetTop));
    on();
    vv.addEventListener('resize', on);
    vv.addEventListener('scroll', on);
    return () => {
      vv.removeEventListener('resize', on);
      vv.removeEventListener('scroll', on);
    };
  }, []);
  const keep = (e) => e.preventDefault();
  return (
    <div className="fv-editbar" style={bottom ? { transform: `translateY(-${bottom}px)` } : undefined} onPointerDown={keep} onMouseDown={keep} role="toolbar" aria-label={t('fv.editbar')}>
      <button type="button" onClick={() => api.current?.undo()} aria-label={t('fv.undo')}>
        <Undo2 size={16} />
      </button>
      <button type="button" onClick={() => api.current?.redo()} aria-label={t('fv.redo')}>
        <Redo2 size={16} />
      </button>
      {KEYS.map(([label, a, b]) => (
        <button key={label} type="button" onClick={() => api.current?.insert(a, b ? { wrap: b } : {})} aria-label={label === 'Tab' ? 'Tab' : undefined}>
          {label === 'Tab' ? <CornerDownLeft size={14} style={{ transform: 'scaleX(-1) rotate(90deg)' }} /> : label}
        </button>
      ))}
    </div>
  );
}

// ---------------- 压缩包里的一个文件 ----------------

const ARC_IMAGE = /\.(png|jpe?g|gif|webp|bmp|ico|avif)$/i;

/** 压缩包里的一个文件：图片显示出来，文字显示内容，其他说看不了 */
export function ArchiveEntry({ name, fetchEntry, onBack }) {
  const { t } = useTranslation();
  const [st, setSt] = useState({ loading: true });
  useEffect(() => {
    let alive = true;
    let url = null;
    fetchEntry()
      .then(async (res) => {
        const buf = await res.arrayBuffer();
        if (ARC_IMAGE.test(name)) {
          url = URL.createObjectURL(new Blob([buf], { type: res.headers.get('content-type') ?? '' }));
          if (alive) setSt({ url });
          return;
        }
        const bytes = new Uint8Array(buf);
        let text = null;
        if (!bytes.subarray(0, 8192).includes(0)) {
          try {
            text = new TextDecoder('utf-8', { fatal: true }).decode(bytes.subarray(0, 2_000_000));
          } catch {
            text = null;
          }
        }
        if (alive) setSt({ text });
      })
      .catch((error) => alive && setSt({ error }));
    return () => {
      alive = false;
      if (url) URL.revokeObjectURL(url);
    };
  }, []);
  return (
    <div className="arc">
      <div className="arc-head">
        <button type="button" className="btn btn-plain btn-sm" onClick={onBack}>
          ← {t('fv.arc_back')}
        </button>
        <span className="ellipsis grow small" title={name}>{name}</span>
      </div>
      {st.loading ? (
        <p className="muted small fb-pad">{t('fb.loading')}</p>
      ) : st.error ? (
        <p className="small fb-pad" style={{ color: 'var(--danger)' }}>{errorText(t, st.error)}</p>
      ) : st.url ? (
        <img src={st.url} alt={name} />
      ) : st.text == null ? (
        <p className="muted small fb-pad">{t('fb.binary')}</p>
      ) : (
        <pre className="fb-text fb-wrap">{st.text}</pre>
      )}
    </div>
  );
}
