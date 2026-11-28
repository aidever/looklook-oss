// 一个文件的预览（docs/FILES_REDO_DESIGN_2026-11-24.md）：“文件”页右边、对话里点开的文件（ChatView 的 FileViewer）都用它。
// 代码和文字（CodeMirror，CodeView.jsx：按原编码读写、分段看大文件、对比、草稿、外部改动）、图片（含 SVG）、音视频（Vidstack）、
// Markdown / CSV / 网页 / JSON / 笔记本的“好看的”看法、办公文件（OfficePreview.jsx）、压缩包、十六进制（files/views.jsx）。
import { BookOpen, Braces, ChevronDown, ChevronUp, CirclePlay, Code2, CopyPlus, Download, FileArchive, FileCode, FilePen, FileSpreadsheet, FileText, GitCompare, Globe, Hash, Image as ImageIcon, Maximize2, MessageSquarePlus, Save, ScrollText, Share2, Sparkles, TextCursorInput, WrapText, X } from 'lucide-react';
import { lazy, Suspense, useEffect, useMemo, useRef, useState } from 'react';
import { useTranslation } from 'react-i18next';

import { deviceApi, fetchRaw, isLocalDevice, rtcMode, saveViaFrame, selectedDevice, terminalDevice } from '../shared/api.js';
import { errorText } from '../shared/i18n.js';
import ImageViewer from '../shared/ImageViewer.jsx';
import { touchUi } from '../shared/layers.js';
import Markdown, { isMarkdown } from '../shared/Markdown.jsx';
import { OFFICE_MAX, officeKind, parseCsv, WHOLE } from '../shared/office.js';
import PopMenu from '../shared/PopMenu.jsx';
import { relativeTime } from '../shared/time.js';
import { Button, ErrorNote, Field, Modal, Seg, useConfirm, useToast } from '../shared/ui.jsx';
import { formatBytes } from '../shared/xfer.js';
import { DocxView, PptxView, renderOffice, SheetView } from '../pages/OfficePreview.jsx';
import { dropDraft, getDraft, putDraft } from './drafts.js';
import { EDIT_MAX, ENCODINGS, eolLabel, READ_LEN, readText, statFile } from './text.js';
import { ARCHIVE, ArchiveEntry, ArchiveView, EditBar, HexView, HTML, HtmlView, JsonTree, Notebook, PdfView, useBlobImages } from './views.jsx';

export const IMAGE = /\.(png|jpe?g|gif|webp|bmp|ico|avif|svg)$/i;
/** 像日志的文件：预览时提供“跟随” */
const LOGLIKE = /\.(log|out|err|jsonl|ndjson)$|(^|[._-])log$/i;
export const VIDEO = /\.(mp4|m4v|webm|mov|mkv|ogv)$/i;
export const AUDIO = /\.(mp3|m4a|aac|wav|flac|ogg|oga|opus)$/i;
/** 远程又不能用普通链接时（多台电脑、不经 WebRTC）音视频只能整个取回来再放：太大就不放了 */
const MEDIA_FETCH_MAX = 512 * 1024 * 1024;
const Player = lazy(() => import('../shared/MediaPlayer.jsx'));
const CodeView = lazy(() => import('../shared/CodeView.jsx'));

/**
 * 音视频的播放地址：能用普通链接时直接指向下载接口（`inline=1`，边下边播）；远程经 WebRTC 时同样，
 * 用 `__lldev` 指定电脑（Service Worker 转发，Range 照常）。都不行时返回 null（只能取回来再放）。
 */
export function mediaSrc(ep, path, device) {
  const href = `/api${ep.download(path)}&inline=1`;
  if (rtcMode) return device ? `${href}&__lldev=${encodeURIComponent(device)}` : href;
  return ep.plain ? href : null;
}

/** 下载：能用普通链接时直接打开；远程多台电脑时先取回再保存（普通链接带不了 X-LL-Device）。 */
function DownloadLink({ ep, entry, size = 15 }) {
  const { t } = useTranslation();
  const toast = useToast();
  const label = t('files.download');
  if (ep.plain) {
    return (
      <a className="btn btn-plain btn-icon" href={`/api${ep.download(entry.path)}`} download={entry.name} aria-label={label} title={label} onClick={() => toast(t('fb.dl_start_one', { name: entry.name }))}>
        <Download size={size} />
      </a>
    );
  }
  const save = async () => {
    toast(t('fb.dl_start_one', { name: entry.name }));
    // 远程（WebRTC）：经 iframe 边收边存，不整个取进内存
    if (rtcMode) return saveViaFrame(`/api${ep.download(entry.path)}`, selectedDevice());
    try {
      const blob = await (await fetchRaw(ep.download(entry.path))).blob();
      const url = URL.createObjectURL(blob);
      const a = Object.assign(document.createElement('a'), { href: url, download: entry.name });
      document.body.appendChild(a);
      a.click();
      a.remove();
      setTimeout(() => URL.revokeObjectURL(url), 30000);
    } catch (e) {
      toast(errorText(t, e), 'error');
    }
  };
  return (
    <button type="button" className="btn btn-plain btn-icon" onClick={save} aria-label={label} title={label}>
      <Download size={size} />
    </button>
  );
}

/** 旧格式转换的进度：准备转换器（电脑下载 / 取到浏览器）、转换中 */
function stageText(t, { stage, done, total }) {
  const pct = total ? Math.min(99, Math.floor((done / total) * 100)) : null;
  if (stage === 'convert') return t('fb.x2t_convert');
  if (stage === 'unpack') return t('fb.x2t_unpack');
  const key = stage === 'download' ? 'fb.x2t_download' : 'fb.x2t_fetch';
  return pct == null ? t(key, { progress: done ? formatBytes(done) : '' }) : t(key, { progress: `${pct}%` });
}

/** Markdown 里的相对图片路径 → 这台电脑上的绝对路径。 */
function resolve(dir, rel, sep) {
  if (/^([\\/]|[A-Za-z]:[\\/])/.test(rel)) return rel;
  const parts = dir.split(/[\\/]/);
  for (const seg of rel.split(/[\\/]/)) {
    if (seg === '..') parts.length > 1 && parts.pop();
    else if (seg && seg !== '.') parts.push(seg);
  }
  const out = parts.join(sep);
  return out || sep;
}

/** 一段文字有几行（分段看时算最后一行的行号） */
const lineCount = (s) => {
  let n = 1;
  for (let i = s.indexOf('\n'); i >= 0; i = s.indexOf('\n', i + 1)) n += 1;
  return s.endsWith('\n') ? n - 1 : n;
};

/** 跳到第几行：标题栏下面的一个小输入框（Ctrl/⌘+G） */
function GotoLine({ max, onGo, onClose }) {
  const { t } = useTranslation();
  const [v, setV] = useState('');
  return (
    <form
      className="fv-goto"
      onSubmit={(e) => {
        e.preventDefault();
        const n = parseInt(v, 10);
        if (n > 0) onGo(n);
      }}
    >
      <Hash size={14} aria-hidden="true" />
      <input
        autoFocus
        inputMode="numeric"
        value={v}
        onChange={(e) => setV(e.target.value.replace(/\D/g, ''))}
        onKeyDown={(e) => e.key === 'Escape' && (e.stopPropagation(), onClose())}
        placeholder={max ? t('fv.goto_ph_max', { max }) : t('fv.goto_ph')}
        aria-label={t('fv.goto')}
      />
      <button type="submit" className="btn btn-primary btn-sm">
        {t('fv.goto_go')}
      </button>
      <button type="button" className="btn btn-plain btn-icon" onClick={onClose} aria-label={t('action.close')}>
        <X size={15} />
      </button>
    </form>
  );
}

/** 另存为：输入完整路径（默认同一个文件夹里的“副本”） */
function SaveAs({ path, sep, onSave, onClose }) {
  const { t } = useTranslation();
  const dot = path.lastIndexOf('.');
  const cut = dot > path.lastIndexOf(sep) ? dot : path.length;
  const [v, setV] = useState(`${path.slice(0, cut)} ${t('fv.copy_suffix')}${path.slice(cut)}`);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState(null);
  return (
    <Modal open onClose={onClose} title={t('fv.save_as')}>
      <form
        onSubmit={async (e) => {
          e.preventDefault();
          setBusy(true);
          setError(null);
          try {
            await onSave(v.trim());
            onClose();
          } catch (err) {
            setError(err);
          }
          setBusy(false);
        }}
      >
        <Field label={t('fv.save_as_path')}>
          <input className="input" value={v} onChange={(e) => setV(e.target.value)} autoFocus spellCheck={false} />
        </Field>
        {error && <ErrorNote error={error} />}
        <div className="modal-actions">
          <Button variant="ghost" type="button" onClick={onClose}>
            {t('action.cancel')}
          </Button>
          <Button type="submit" busy={busy} disabled={!v.trim()}>
            {t('fb.save')}
          </Button>
        </div>
      </form>
    </Modal>
  );
}

/** 文字文件的几种“好看的”看法：Markdown 排版、CSV 表格、网页、JSON 树、笔记本 */
function richKind(name, office) {
  if (isMarkdown(name)) return 'md';
  if (office === 'csv') return 'csv';
  if (HTML.test(name)) return 'html';
  if (/\.ipynb$/i.test(name)) return 'ipynb';
  if (/\.(json|jsonl|ndjson|geojson)$/i.test(name)) return 'json';
  return null;
}

/**
 * 一个文件的预览（“文件”页右边；对话页里点到的文件也用它，见 ChatView 的 FileViewer）。
 * 对话页另外传：focus（{ from, to }：标出这几行并滚过去）、extra（标题栏关闭按钮前的按钮）。
 * `device`：文件在哪台电脑（远程播放音视频时用）；`active`：看不见时（切到别的标签、别的会话）把正在放的停下、不查外部改动。
 * `insertLabel`：填路径按钮的说明（对话会话里填进输入框，其他填进终端）。
 * `edit`：打开就进编辑（对话页 /memory 刚新建的空 CLAUDE.md）；`diff`：打开就看和会话开始前比改了什么。
 * `onOpenPath(path)`：另存为、解压之后打开新的文件 / 文件夹（“文件”页传）。
 * `linked`：false 时不提供填进对话 / 评论这几行（新标签页里的查看页，旁边没有对话）。
 * 文字（设计稿 §4）：按原编码读写（/fs/read、/fs/write），可以换编码；大文件分段看；没存的改动记成草稿；
 * 外面改了文件：只读时自动重读，编辑时提示（对比 / 用磁盘上的 / 保留我的）。
 */
export function Preview({ ep, entry, onClose, onInsert, insertLabel, sep, focus = null, extra = null, device = null, active = true, onAsk, edit = false, diff: diffInit = false, onOpenPath, onDirty, linked = true }) {
  const { t } = useTranslation();
  const toast = useToast();
  const confirm = useConfirm();
  const [state, setState] = useState({ loading: true });
  const dev = device ?? (ep.id ? terminalDevice(ep.id) : selectedDevice());
  const ops = deviceApi(dev ?? undefined);
  const rawOpts = dev ? { device: dev } : {};
  const devKey = dev ?? 'self';
  // 编辑（§9.4 J5）：草稿放在 ref 里（每个字都 setState 会让编辑器重建）；dirty 只用来显示“未保存”
  const [editing, setEditing] = useState(false);
  const [editBase, setEditBase] = useState('');
  const draft = useRef('');
  const [dirty, setDirty] = useState(false);
  useEffect(() => {
    onDirty?.(dirty);
  }, [dirty, onDirty]);
  const [saving, setSaving] = useState(false);
  const [sel, setSel] = useState(null);
  const [follow, setFollow] = useState(false);
  const shareFile = useRef(null);
  const cm = useRef(null);
  const keepScroll = useRef(null);
  const body = useRef(null);
  // 没传 onAsk：对话里点开的文件，“让 AI 改”填进这个会话的对话输入框；传 null 表示不提供
  const ask = onAsk === undefined ? (linked && ep.id ? (text) => {
    window.dispatchEvent(new CustomEvent(`ll-chat-insert:${ep.id}`, { detail: text }));
    if (touchUi) onClose?.();
  } : null) : onAsk;
  const [wrap, setWrap] = useState(true);
  const office = officeKind(entry.name);
  const rich = richKind(entry.name, office);
  // Markdown 看排版效果 / 源码，CSV 看表格 / 源码……；要标出某几行时先看源码；JSON 默认看源码
  const [rendered, setRendered] = useState(!focus?.from && rich !== 'json');
  const image = IMAGE.test(entry.name);
  const video = VIDEO.test(entry.name);
  const audio = !video && AUDIO.test(entry.name);
  const media = video || audio;
  const whole = WHOLE.includes(office);
  const archive = ARCHIVE.test(entry.name);
  const [zoom, setZoom] = useState(false);
  const [hex, setHex] = useState(false);
  const [arcEntry, setArcEntry] = useState(null);
  const [goto, setGoto] = useState(false);
  const [saveAs, setSaveAs] = useState(false);
  // 标出的行（对话里点进来的；跳到行时换成跳到的那一行）
  const [mark, setMark] = useState(focus);
  // 外部改动：flash 显示“已更新”；conflict：编辑中文件被改了 / 保存时发现被改了；gone：文件被删了
  const [flash, setFlash] = useState(false);
  const [conflict, setConflict] = useState(null);
  const [gone, setGone] = useState(false);
  const [draftHint, setDraftHint] = useState(null);
  // 对比（§4.4）：和会话开始前（first）/ 这一轮开始前（turn）的版本比
  const [diffOn, setDiffOn] = useState(Boolean(diffInit && ep.id));
  const [diffAt, setDiffAt] = useState('first');
  const [base, setBase] = useState(null);
  const [chunks, setChunks] = useState(null);
  const doc = state.doc ?? null;
  const partial = Boolean(doc && state.text != null && (doc.offset > 0 || doc.end < doc.size));
  const lineStart = doc?.line ?? 1;
  const lastLine = doc?.text != null ? lineStart + lineCount(doc.text) - 1 : 0;

  /** 读文字：opts = { offset, line, encoding }；keep：外面改了自动重读（停在原来的位置） */
  const loadText = async (opts = {}, { keep = false, quiet = false } = {}) => {
    if (!quiet) setState((st) => ({ ...st, loading: true }));
    const r = await readText(ops, ep, entry.path, { ...opts, fetchRaw: (p) => fetchRaw(p) });
    if (keep) keepScroll.current = body.current?.querySelector('.cm-scroller')?.scrollTop ?? null;
    setState({ doc: r, text: r.text });
    setGone(false);
    return r;
  };

  useEffect(() => {
    let alive = true;
    let url = null;
    if (media) {
      const src = mediaSrc(ep, entry.path, device);
      if (src) setState({ media: src });
      else if ((entry.size ?? 0) > MEDIA_FETCH_MAX) setState({ office: 'media_big' });
      else {
        setState({ loading: true });
        fetchRaw(ep.download(entry.path))
          .then((r) => r.blob())
          .then((b) => {
            url = URL.createObjectURL(b);
            if (alive) setState({ media: url });
          })
          .catch((error) => alive && setState({ error }));
      }
      return () => {
        alive = false;
        if (url) URL.revokeObjectURL(url);
      };
    }
    if (archive) {
      setState({ archive: true });
      return undefined;
    }
    // 浏览器不能在页面里显示 PDF（安卓上的 Chrome 等）：用 pdf.js 自己画
    const noPdf = office === 'pdf' && navigator.pdfViewerEnabled === false;
    if (office === 'other' || (whole && entry.size > OFFICE_MAX)) {
      setState({ office: office === 'other' ? 'other' : 'too_big' });
      return undefined;
    }
    setState({ loading: true });
    const ac = new AbortController();
    if (image || whole) {
      // 图片、PDF、Word、Excel、PowerPoint 取整个文件
      fetchRaw(ep.download(entry.path))
        .then(async (res) => {
          if (image) {
            const blob = await res.blob();
            // SVG 当图片看（<img> 里的 SVG 不会跑脚本）；类型不对时浏览器不显示，按扩展名补上
            url = URL.createObjectURL(/\.svg$/i.test(entry.name) ? new Blob([blob], { type: 'image/svg+xml' }) : blob);
            if (alive) setState({ url });
            return;
          }
          if (noPdf) {
            const blob = await res.blob();
            if (alive) setState({ pdf: blob });
            return;
          }
          // 旧格式要先转换（第一次还要准备转换器），把进度显示出来
          const onStage = (stage) => alive && setState({ loading: true, stage });
          let view = null;
          let code = 'failed';
          try {
            view = await renderOffice(office, await res.blob(), { ops, device: dev, onStage, signal: ac.signal });
          } catch (e) {
            code = { X2T_CLIENT_OLD: 'x2t_old', X2T_DOWNLOAD_FAILED: 'x2t_download', X2T_TOO_LARGE: 'too_big' }[e?.code] ?? 'failed';
          }
          url = view?.url ?? null;
          if (alive) setState(view ? { view } : { office: code });
        })
        .catch((error) => alive && setState({ error }));
    } else {
      (async () => {
        let r = await readText(ops, ep, entry.path, { fetchRaw: (p) => fetchRaw(p) });
        // 大文件：要标出的行不在第一段里，从那一行附近读
        if (alive && focus?.from && r.text != null && !r.legacy && r.end < r.size && focus.from > (r.line ?? 1) + lineCount(r.text) - 1) {
          r = await readText(ops, ep, entry.path, { line: focus.from });
        }
        if (alive) setState({ doc: r, text: r.text });
      })().catch((error) => alive && setState({ error }));
    }
    return () => {
      alive = false;
      ac.abort();
      if (url) URL.revokeObjectURL(url);
    };
  }, [entry.path]);
  const dir = entry.path.slice(0, Math.max(0, entry.path.lastIndexOf(sep))) || sep;
  const showRich = rich && rendered && state.text != null && !editing && !diffOn;
  const csvSheets = useMemo(() => (showRich && rich === 'csv' ? parseCsv(state.text, entry.name) : null), [showRich, rich, state.text, entry.name]);
  const insertText = insertLabel ?? t('fb.insert');
  // Markdown 里的相对图片：能用普通链接时直接指向下载接口，不能时（远程多台电脑）取回来再显示
  const mdBlob = useBlobImages(state.text ?? '', (p) => fetchRaw(ep.download(resolve(dir, p, sep))).then((r) => r.blob()), rich === 'md' && !ep.plain && showRich);
  const mdImage = ep.plain ? (src) => `/api${ep.download(resolve(dir, src, sep))}&inline=1` : mdBlob;

  // 跟随日志（§9.4 J4）：每 2 秒取新增的内容接在后面，滚到最后；看不见时停
  useEffect(() => {
    if (!follow || !active || state.text == null) return undefined;
    let alive = true;
    let from = doc && doc.end < doc.size ? 0 : doc?.size ?? 0;
    let timer = 0;
    const tick = () =>
      ops
        .get(`/fs/tail?path=${encodeURIComponent(entry.path)}&from=${from}`)
        .then((r) => {
          if (!alive) return;
          from = r.size;
          if (r.reset || r.text) setState((st) => ({ ...st, text: (r.reset ? r.text : st.text + r.text).slice(-2_000_000), doc: st.doc && { ...st.doc, size: r.size, end: r.size } }));
        })
        .catch((e) => {
          if (!alive) return;
          setFollow(false);
          toast(errorText(t, e.status === 404 ? { code: 'CLIENT_TOO_OLD' } : e), 'error');
        })
        .finally(() => {
          if (alive) timer = setTimeout(tick, 2000);
        });
    tick();
    return () => {
      alive = false;
      clearTimeout(timer);
    };
  }, [follow, active, entry.path]);

  // 外部改动（§4.3）：看得见时每 3 秒问一次文件的修改时间；旧版看看没有 /fs/stat 时不查
  const seen = useRef(null);
  seen.current = { mtime: doc?.mtime_ms, size: doc?.size, editing, conflict };
  useEffect(() => {
    if (!active || !doc || doc.legacy || follow || doc.text == null) return undefined;
    let alive = true;
    let timer = 0;
    let off = false;
    const tick = async () => {
      if (document.visibilityState === 'visible') {
        try {
          const r = await statFile(ops, entry.path);
          if (!alive) return;
          if (r === null) off = true;
          else if (!r.exists) setGone(true);
          else {
            setGone(false);
            const s = seen.current;
            if (r.mtime_ms !== s.mtime || r.size !== s.size) {
              if (s.editing) {
                if (!s.conflict || s.conflict.mtime !== r.mtime_ms) setConflict({ kind: 'external', mtime: r.mtime_ms });
              } else {
                await loadText({ offset: partial ? doc.offset : undefined, encoding: doc.encoding ?? undefined }, { keep: true, quiet: true });
                if (!alive) return;
                setFlash(true);
                setTimeout(() => alive && setFlash(false), 2000);
              }
            }
          }
        } catch {
          /* 一时问不到：下次再问 */
        }
      }
      if (alive && !off) timer = setTimeout(tick, 3000);
    };
    timer = setTimeout(tick, 3000);
    return () => {
      alive = false;
      clearTimeout(timer);
    };
  }, [active, entry.path, doc?.legacy, follow, doc?.text == null, doc?.offset]);

  // 对比的基准：会话的检查点（没有时 HEAD），按需取
  useEffect(() => {
    if (!diffOn || !ep.id) return undefined;
    let alive = true;
    setBase(null);
    setChunks(null);
    ops
      .get(`/instances/${ep.id}/checkpoints/base?path=${encodeURIComponent(entry.path)}${diffAt === 'turn' ? '&at=turn' : ''}`)
      .then((r) => alive && setBase(r))
      .catch((error) => alive && setBase({ error }));
    return () => {
      alive = false;
    };
  }, [diffOn, diffAt, entry.path]);

  const plainText = state.text != null && !image && !media && !state.view && !state.office && !state.archive;
  const size = doc?.size ?? entry.size ?? 0;
  const canEdit = plainText && !partial && size <= (doc?.legacy ? 1024 * 1024 : EDIT_MAX) && !follow;
  const canFollow = plainText && !editing && (LOGLIKE.test(entry.name) || (entry.mtime_ms && Date.now() - entry.mtime_ms < 10 * 60_000));
  const canDiff = Boolean(ep.id) && plainText && !partial && !follow;

  // 草稿：读到内容后看有没有上次没存的
  useEffect(() => {
    if (!doc || doc.text == null || partial || editing) return undefined;
    let alive = true;
    getDraft(devKey, entry.path).then((d) => alive && setDraftHint(d && d.text !== doc.text ? d : null));
    return () => {
      alive = false;
    };
  }, [doc?.mtime_ms, entry.path]);
  const draftTimer = useRef(0);
  useEffect(() => () => clearTimeout(draftTimer.current), []);

  const startEdit = (text = state.text, wasDirty = false) => {
    draft.current = text;
    setEditBase(text);
    setDirty(wasDirty);
    setRendered(false);
    setDraftHint(null);
    setEditing(true);
  };
  // 要求一打开就编辑：内容读到、能编辑时进一次
  const autoEdit = useRef(edit);
  useEffect(() => {
    if (autoEdit.current && !state.loading && canEdit && !editing) {
      autoEdit.current = false;
      startEdit();
    }
  }, [state.loading, canEdit]);
  const endEdit = () => {
    clearTimeout(draftTimer.current);
    dropDraft(devKey, entry.path);
    setEditing(false);
    setDirty(false);
    setConflict(null);
  };
  const stopEdit = async () => {
    if (dirty && !(await confirm({ title: t('fb.edit_discard_title'), body: t('fb.edit_discard_body'), confirm: t('fb.edit_discard'), danger: true }))) return false;
    endEdit();
    return true;
  };
  const onChange = (v) => {
    draft.current = v;
    if (!dirty) setDirty(true);
    clearTimeout(draftTimer.current);
    draftTimer.current = setTimeout(() => putDraft(devKey, entry.path, draft.current, doc?.mtime_ms), 500);
  };
  // 保存时用的换行：混合换行先问一次统一成哪种
  const eolChoice = useRef(null);
  const writeOpts = async () => {
    if (!doc || doc.legacy) return {};
    let eol = doc.eol === 'mixed' ? eolChoice.current : doc.eol;
    if (!eol) {
      const major = doc.eol_major ?? 'lf';
      if (!(await confirm({ title: t('fv.eol_mixed_title'), body: t('fv.eol_mixed_body', { eol: eolLabel(major) }), confirm: t('fv.eol_unify', { eol: eolLabel(major) }) }))) return null;
      eol = major;
      eolChoice.current = major;
    }
    return { encoding: doc.encoding ?? undefined, eol, bom: doc.bom };
  };
  const save = async (force = false, override = null) => {
    if (saving) return undefined;
    const opts = override ?? (await writeOpts());
    if (!opts) return undefined;
    setSaving(true);
    try {
      const r = await ops.post('/fs/write', { path: entry.path, content: draft.current, if_mtime: force ? undefined : doc?.mtime_ms || undefined, ...opts });
      const text = draft.current;
      setState((st) => ({ ...st, text, doc: { ...st.doc, text, mtime_ms: r.mtime_ms, ...(() => { const n = r.size ?? new TextEncoder().encode(text).length; return { size: n, end: n, offset: 0 }; })(), encoding: opts.encoding ?? st.doc?.encoding, bom: opts.bom ?? st.doc?.bom, eol: opts.eol ?? st.doc?.eol } }));
      endEdit();
      toast(t('fb.saved', { name: entry.name }));
    } catch (e) {
      setSaving(false);
      if (e.code === 'FILE_CHANGED') {
        setConflict({ kind: 'save', mtime: e.params?.mtime_ms });
        return undefined;
      }
      if (e.code === 'ENCODING_UNMAPPABLE') {
        if (await confirm({ title: t('fv.unmappable_title'), body: t('fv.unmappable_body', { char: e.params?.char ?? '?', encoding: e.params?.encoding ?? doc?.encoding }), confirm: t('fv.save_utf8') })) {
          return save(force, { ...opts, encoding: 'UTF-8', bom: false });
        }
        return undefined;
      }
      toast(errorText(t, e.status === 404 && !e.params?.path ? { code: 'CLIENT_TOO_OLD' } : e), 'error');
      return undefined;
    }
    setSaving(false);
    return undefined;
  };
  // 冲突：对比（我的 vs 磁盘上的）/ 用磁盘上的 / 保留我的（覆盖）
  const [diskText, setDiskText] = useState(null);
  const conflictDiff = async () => {
    try {
      const r = await readText(ops, ep, entry.path, { encoding: doc?.encoding ?? undefined });
      setDiskText(r.text ?? '');
    } catch (e) {
      toast(errorText(t, e), 'error');
    }
  };
  const useDisk = async () => {
    endEdit();
    setDiskText(null);
    await loadText({ encoding: doc?.encoding ?? undefined }).catch((error) => setState({ error }));
  };
  const keepMine = () => {
    setDiskText(null);
    setConflict(null);
    save(true);
  };
  const saveAsPath = async (to) => {
    const opts = (await writeOpts()) ?? {};
    await ops.post('/fs/write', { path: to, content: editing ? draft.current : state.text, create: true, ...opts });
    toast(t('fb.saved', { name: to.split(/[\\/]/).pop() }));
    onOpenPath?.(to);
  };
  const close = async () => {
    if (editing && !(await stopEdit())) return;
    onClose?.();
  };
  const lines = sel ? (sel.from === sel.to ? `${sel.from + lineStart - 1}` : `${sel.from + lineStart - 1}-${sel.to + lineStart - 1}`) : '';
  const askAi = () => {
    if (!ask || !sel) return;
    const code = sel.text.length > 6000 ? `${sel.text.slice(0, 6000)}\n…` : sel.text;
    ask(t('fb.ask_ai_text', { path: entry.path, lines, code }));
  };
  // 评论这几行：作为引用放进对话输入框（和划词评论一样，发出去时带上）
  const comment = () => {
    if (!linked || !ep.id || !sel) return;
    const code = sel.text.length > 4000 ? `${sel.text.slice(0, 4000)}\n…` : sel.text;
    window.dispatchEvent(new CustomEvent(`ll-chat-quote:${ep.id}`, { detail: { quote: `${entry.path}:${lines}\n${code}`, note: '', focus: 'note' } }));
    if (touchUi) onClose?.();
  };
  const canShare = touchUi && typeof navigator.canShare === 'function' && !media;
  const share = async () => {
    try {
      if (!shareFile.current) {
        const blob = await (await fetchRaw(ep.download(entry.path))).blob();
        shareFile.current = new File([blob], entry.name, { type: blob.type || 'application/octet-stream' });
      }
      const data = { files: [shareFile.current], title: entry.name };
      if (!navigator.canShare(data)) {
        toast(t('fb.share_unsupported'), 'error');
        return;
      }
      await navigator.share(data);
    } catch (e) {
      if (e?.name === 'AbortError') return;
      // 取文件花了太久，浏览器不认这次点击了：文件已经在手里，再点一次就能分享
      if (e?.name === 'NotAllowedError' && shareFile.current) toast(t('fb.share_again'));
      else toast(errorText(t, e), 'error');
    }
  };
  // 跳到行：在这一段里就直接跳，不在就从那一行附近重读
  const gotoLine = async (n) => {
    setGoto(false);
    if (!partial || (n >= lineStart && n <= lastLine)) {
      if (cm.current) cm.current.gotoLine(n);
      else setMark({ from: n, to: n });
      return;
    }
    try {
      await loadText({ line: n, encoding: doc?.encoding ?? undefined });
      setMark({ from: n, to: n });
    } catch (e) {
      toast(errorText(t, e), 'error');
    }
  };
  const segment = (offset) => loadText({ offset, encoding: doc?.encoding ?? undefined }).catch((e) => toast(errorText(t, e), 'error'));
  const reencode = (encoding) => loadText({ encoding, offset: partial ? doc.offset : undefined }).catch((e) => toast(errorText(t, e), 'error'));
  // 键盘：Ctrl/⌘+G 跳到行；对比时 [ ] 上一处 / 下一处改动
  const onKeyDown = (e) => {
    if ((e.ctrlKey || e.metaKey) && !e.shiftKey && !e.altKey && e.key.toLowerCase() === 'g' && plainText) {
      e.preventDefault();
      setGoto(true);
      return;
    }
    if (diffOn && !editing && !e.ctrlKey && !e.metaKey && !e.altKey && (e.key === '[' || e.key === ']')) {
      e.preventDefault();
      if (e.key === '[') cm.current?.prevChunk();
      else cm.current?.nextChunk();
    }
  };
  const original = diskText != null ? diskText : diffOn && base && !base.error && base.source !== 'none' && base.text != null ? base.text : null;
  const relMark = mark?.from ? { from: Math.max(1, mark.from - lineStart + 1), to: Math.max(1, (mark.to ?? mark.from) - lineStart + 1) } : null;
  const showCode = plainText && !showRich && !hex && (state.text || editing);
  const moreItems = plainText
    ? [
        !showRich && { icon: WrapText, label: t('fb.wrap'), checked: wrap, onClick: () => setWrap(!wrap) },
        { icon: Hash, label: `${t('fv.goto')} (Ctrl+G)`, onClick: () => setGoto(true) },
        !doc?.legacy && { icon: FileCode, label: t('fv.hex'), checked: hex, onClick: () => setHex(!hex) },
        !doc?.legacy && { icon: CopyPlus, label: t('fv.save_as'), onClick: () => setSaveAs(true) },
        !doc?.legacy && !editing && doc?.encoding && { head: t('fv.encoding') },
        ...(!doc?.legacy && !editing && doc?.encoding ? ENCODINGS.map((name) => ({ label: name, checked: doc.encoding.toLowerCase() === name.toLowerCase(), onClick: () => reencode(name) })) : []),
      ]
    : state.text === null && !state.loading && !state.office ? [{ icon: FileCode, label: t('fv.hex'), checked: hex, onClick: () => setHex(!hex) }] : null;
  const encLabel = doc?.encoding && !doc.legacy ? `${doc.encoding}${doc.bom ? ' BOM' : ''} · ${eolLabel(doc.eol)}` : null;
  return (
    <div className="fb-preview" role="region" aria-label={entry.name} onKeyDown={onKeyDown}>
      <div className="fb-preview-head">
        {media ? <CirclePlay size={16} aria-hidden="true" /> : image ? <ImageIcon size={16} aria-hidden="true" /> : office === 'xlsx' || office === 'csv' ? <FileSpreadsheet size={16} aria-hidden="true" /> : archive ? <FileArchive size={16} aria-hidden="true" /> : <FileText size={16} aria-hidden="true" />}
        <b className="ellipsis grow" title={entry.path}>{entry.name}</b>
        {flash && <span className="fv-flash small">{t('fv.updated')}</span>}
        {size > 0 && <span className="muted small fv-size">{formatBytes(size)}</span>}
        {encLabel && (
          <span className="fv-enc">
            <PopMenu
              className="btn btn-plain btn-sm fv-enc-btn"
              icon={null}
              text={encLabel}
              label={t('fv.encoding')}
              disabled={editing}
              items={[{ head: t('fv.encoding_reopen') }, ...ENCODINGS.map((name) => ({ label: name, checked: doc.encoding.toLowerCase() === name.toLowerCase(), onClick: () => reencode(name) }))]}
            />
          </span>
        )}
        {rich && state.text != null && !editing && !diffOn && (
          // 看效果 / 看源码：两个按钮并排，现在是哪种一眼看得出（以前一个图标显示的是“另一种”）
          <span className="seg seg-xs fv-viewseg" role="group" aria-label={t('fv.view_mode')}>
            <button type="button" aria-pressed={rendered} onClick={() => setRendered(true)} title={t(`fv.rich_${rich}`)}>
              {rich === 'csv' ? <FileSpreadsheet size={14} aria-hidden="true" /> : rich === 'html' ? <Globe size={14} aria-hidden="true" /> : rich === 'json' ? <Braces size={14} aria-hidden="true" /> : <BookOpen size={14} aria-hidden="true" />}
              <span className="fv-viewseg-text">{t(`fv.rich_${rich}`)}</span>
            </button>
            <button type="button" aria-pressed={!rendered} onClick={() => setRendered(false)} title={t('fv.source')}>
              <Code2 size={14} aria-hidden="true" />
              <span className="fv-viewseg-text">{t('fv.source')}</span>
            </button>
          </span>
        )}
        {image && state.url && (
          <button type="button" className="btn btn-plain btn-icon" onClick={() => setZoom(true)} aria-label={t('fb.zoom')} title={t('fb.zoom')}>
            <Maximize2 size={16} />
          </button>
        )}
        {canDiff && (
          <button type="button" className={`btn btn-plain btn-icon ${diffOn ? 'on' : ''}`} onClick={() => setDiffOn(!diffOn)} aria-pressed={diffOn} aria-label={t('fv.diff')} title={t('fv.diff_hint')}>
            <GitCompare size={16} />
          </button>
        )}
        {editing ? (
          <>
            {dirty && <span className="muted small fv-unsaved">{t('fb.unsaved')}</span>}
            {linked && ep.id && sel && (
              <button type="button" className="btn btn-plain btn-icon" onClick={comment} aria-label={t('fv.comment')} title={t('fv.comment_hint')}>
                <MessageSquarePlus size={16} />
              </button>
            )}
            <button type="button" className="btn btn-plain btn-sm" onClick={stopEdit} disabled={saving}>
              {t('fb.edit_cancel')}
            </button>
            <button type="button" className="btn btn-primary btn-sm" onClick={() => save()} disabled={saving || !dirty} title={t('fb.save_hint')}>
              <Save size={15} aria-hidden="true" />
              {t(saving ? 'fb.saving' : 'fb.save')}
            </button>
          </>
        ) : (
          <>
            {ask && sel && (
              <button type="button" className="btn btn-plain btn-sm fb-ask" onClick={askAi} title={t('fb.ask_ai_hint')}>
                <Sparkles size={15} aria-hidden="true" />
                {t('fb.ask_ai')}
              </button>
            )}
            {linked && ep.id && sel && (
              <button type="button" className="btn btn-plain btn-icon" onClick={comment} aria-label={t('fv.comment')} title={t('fv.comment_hint')}>
                <MessageSquarePlus size={16} />
              </button>
            )}
            {canFollow && (
              <button type="button" className={`btn btn-plain btn-icon ${follow ? 'on' : ''}`} onClick={() => setFollow(!follow)} aria-pressed={follow} aria-label={t('fb.follow')} title={t('fb.follow_hint')}>
                <ScrollText size={16} />
              </button>
            )}
            {canEdit && (
              <button type="button" className="btn btn-plain btn-icon" onClick={() => startEdit()} aria-label={t('fb.edit')} title={t('fb.edit')}>
                <FilePen size={16} />
              </button>
            )}
            {onInsert && (
              <button type="button" className="btn btn-plain btn-icon" onClick={() => onInsert(entry.path)} aria-label={insertText} title={insertText}>
                <TextCursorInput size={16} />
              </button>
            )}
            {extra}
            {canShare && (
              <button type="button" className="btn btn-plain btn-icon" onClick={share} aria-label={t('fb.share')} title={t('fb.share')}>
                <Share2 size={16} />
              </button>
            )}
            <DownloadLink ep={ep} entry={entry} size={16} />
          </>
        )}
        {moreItems && <PopMenu className="btn btn-plain btn-icon" items={moreItems} label={t('fv.more')} />}
        <button type="button" className="btn btn-plain btn-icon" onClick={close} aria-label={t('fb.close_preview')} title={t('fb.close_preview')}>
          <X size={17} />
        </button>
      </div>
      {goto && <GotoLine max={partial ? null : lastLine} onGo={gotoLine} onClose={() => setGoto(false)} />}
      {gone && <p className="fv-bar fv-bar-warn small">{t('fv.gone')}</p>}
      {draftHint && !editing && (
        <div className="fv-bar small">
          <span className="grow">{t(draftHint.mtime && doc?.mtime_ms && draftHint.mtime !== doc.mtime_ms ? 'fv.draft_changed' : 'fv.draft', { time: relativeTime(t, draftHint.at) })}</span>
          <button type="button" className="btn btn-primary btn-sm" onClick={() => startEdit(draftHint.text, true)} disabled={!canEdit}>
            {t('fv.draft_restore')}
          </button>
          <button
            type="button"
            className="btn btn-plain btn-sm"
            onClick={() => {
              dropDraft(devKey, entry.path);
              setDraftHint(null);
            }}
          >
            {t('fv.draft_drop')}
          </button>
        </div>
      )}
      {conflict && editing && (
        <div className="fv-bar fv-bar-warn small">
          <span className="grow">{t(conflict.kind === 'save' ? 'fv.conflict_save' : 'fv.conflict_external', { name: entry.name })}</span>
          {diskText == null ? (
            <button type="button" className="btn btn-plain btn-sm" onClick={conflictDiff}>
              {t('fv.conflict_diff')}
            </button>
          ) : (
            <button type="button" className="btn btn-plain btn-sm" onClick={() => setDiskText(null)}>
              {t('fv.conflict_nodiff')}
            </button>
          )}
          <button type="button" className="btn btn-plain btn-sm" onClick={useDisk}>
            {t('fv.conflict_disk')}
          </button>
          <button type="button" className="btn btn-primary btn-sm" onClick={keepMine}>
            {t('fv.conflict_mine')}
          </button>
        </div>
      )}
      {diffOn && canDiff && diskText == null && (
        <div className="fv-bar small">
          <Seg
            value={diffAt}
            onChange={setDiffAt}
            label={t('fv.diff')}
            options={[
              ['first', t('fv.diff_first')],
              ['turn', t('fv.diff_turn')],
            ]}
          />
          <span className="grow muted">
            {!base ? t('fb.loading') : base.error ? errorText(t, base.error.status === 404 && !base.error.params?.path ? { code: 'CLIENT_TOO_OLD' } : base.error) : base.source === 'none' ? t('fv.diff_none') : base.text == null ? t('fv.diff_binary') : (
              <>
                {base.absent ? t('fv.diff_new') : base.source === 'head' ? t('fv.diff_head') : base.label ? t('fv.diff_cp', { label: base.label }) : ''}
                {chunks && (
                  <>
                    {' '}
                    <b className="fv-add">+{chunks.add}</b> <b className="fv-del">−{chunks.del}</b>
                  </>
                )}
              </>
            )}
          </span>
          {chunks?.n > 0 && (
            <>
              <button type="button" className="btn btn-plain btn-icon" onClick={() => cm.current?.prevChunk()} aria-label={t('fv.diff_prev')} title={`${t('fv.diff_prev')} ([)`}>
                <ChevronUp size={16} />
              </button>
              <button type="button" className="btn btn-plain btn-icon" onClick={() => cm.current?.nextChunk()} aria-label={t('fv.diff_next')} title={`${t('fv.diff_next')} (])`}>
                <ChevronDown size={16} />
              </button>
            </>
          )}
        </div>
      )}
      {partial && !follow && (
        <div className="fv-bar small">
          <span className="grow muted">
            {doc.line ? t('fv.seg_lines', { from: lineStart, to: lastLine }) : ''} {t('fv.seg_of', { size: formatBytes(doc.size) })}
          </span>
          <button type="button" className="btn btn-plain btn-sm" onClick={() => segment(0)} disabled={doc.offset === 0}>
            {t('fv.seg_start')}
          </button>
          <button type="button" className="btn btn-plain btn-icon" onClick={() => segment(Math.max(0, doc.offset - READ_LEN))} disabled={doc.offset === 0} aria-label={t('fv.seg_prev')} title={t('fv.seg_prev')}>
            <ChevronUp size={16} />
          </button>
          <button type="button" className="btn btn-plain btn-icon" onClick={() => segment(doc.end)} disabled={doc.end >= doc.size} aria-label={t('fv.seg_next')} title={t('fv.seg_next')}>
            <ChevronDown size={16} />
          </button>
          <button type="button" className="btn btn-plain btn-sm" onClick={() => segment(Math.max(0, doc.size - READ_LEN))} disabled={doc.end >= doc.size}>
            {t('fv.seg_end')}
          </button>
        </div>
      )}
      <div className="fb-preview-body" ref={body} data-split={editing && rich === 'md' && !touchUi ? '' : undefined}>
        {state.loading ? (
          <p className="muted small fb-pad">{state.stage ? stageText(t, state.stage) : t('fb.loading')}</p>
        ) : state.error ? (
          <p className="fb-pad small" style={{ color: 'var(--danger)' }}>{t(`files.err.${state.error.code}`, { defaultValue: errorText(t, state.error) })}</p>
        ) : hex ? (
          <HexView fetchHead={() => fetchRaw(`/fs/head?path=${encodeURIComponent(entry.path)}`, rawOpts)} />
        ) : state.archive ? (
          arcEntry ? (
            <ArchiveEntry key={arcEntry.name} name={arcEntry.name} fetchEntry={() => fetchRaw(`/fs/archive/read?path=${encodeURIComponent(entry.path)}&entry=${encodeURIComponent(arcEntry.name)}`, rawOpts)} onBack={() => setArcEntry(null)} />
          ) : (
            <ArchiveView ops={ops} path={entry.path} onExtracted={(p) => (onOpenPath ? onOpenPath(p, true) : toast(t('fv.arc_done', { path: p })))} readEntry={setArcEntry} />
          )
        ) : state.media ? (
          <Suspense fallback={<p className="muted small fb-pad">{t('fb.loading')}</p>}>
            {active ? <Player key={state.media} src={state.media} name={entry.name} audio={audio} onError={() => setState({ office: 'media_failed' })} /> : <p className="muted small fb-pad">{t('fb.media_paused')}</p>}
          </Suspense>
        ) : image ? (
          <>
            <button type="button" className="fb-img" onClick={() => setZoom(true)} aria-label={t('fb.zoom')}>
              <img src={state.url} alt={entry.name} />
            </button>
            {zoom && <ImageViewer images={[{ url: state.url, name: entry.name }]} onClose={() => setZoom(false)} />}
          </>
        ) : state.pdf ? (
          <PdfView blob={state.pdf} />
        ) : state.office ? (
          <p className="muted small fb-pad">{t(`fb.office_${state.office}`, { size: formatBytes(OFFICE_MAX) })}</p>
        ) : state.view?.url ? (
          <iframe className="fb-pdf" src={state.view.url} title={entry.name} />
        ) : state.view?.docx ? (
          <DocxView buf={state.view.docx} onError={() => setState({ office: 'failed' })} />
        ) : state.view?.pptx ? (
          <PptxView buf={state.view.pptx} onError={() => setState({ office: 'failed' })} />
        ) : state.view?.sheets ? (
          <SheetView sheets={state.view.sheets} />
        ) : state.text == null ? (
          <div className="fb-pad">
            <p className="muted small">{t('fb.binary')}</p>
            {!doc?.legacy && (
              <button type="button" className="btn btn-ghost btn-sm" onClick={() => setHex(true)}>
                <FileCode size={14} aria-hidden="true" /> {t('fv.hex')}
              </button>
            )}
          </div>
        ) : showRich ? (
          rich === 'csv' ? (
            <SheetView sheets={csvSheets} />
          ) : rich === 'html' ? (
            <HtmlView ops={ops} path={entry.path} name={entry.name} text={state.text} local={isLocalDevice(dev) && !rtcMode && !doc?.legacy && !partial} />
          ) : rich === 'json' ? (
            <JsonTree text={state.text} jsonl={!/\.(json|geojson)$/i.test(entry.name)} />
          ) : rich === 'ipynb' ? (
            <Notebook text={state.text} />
          ) : (
            <Markdown text={state.text} imageUrl={mdImage} />
          )
        ) : !showCode ? (
          <pre className="fb-text">{t('fb.empty_file')}</pre>
        ) : (
          <>
            {doc?.legacy && doc.end < doc.size && <p className="muted small fb-pad fb-note">{t('fb.truncated', { size: formatBytes(READ_LEN) })}</p>}
            <Suspense fallback={<pre className={`fb-text ${wrap ? 'fb-wrap' : ''}`}>{state.text}</pre>}>
              <CodeView
                text={editing ? editBase : state.text}
                name={entry.name}
                wrap={wrap}
                focus={follow || editing ? null : relMark}
                editable={editing}
                atEnd={follow}
                onChange={onChange}
                onSave={() => save()}
                onSelect={setSel}
                original={original}
                onChunks={setChunks}
                lineStart={lineStart}
                api={cm}
                keepScroll={keepScroll}
              />
            </Suspense>
            {editing && rich === 'md' && !touchUi && <MdLive draft={draft} dirty={dirty} imageUrl={mdImage} />}
          </>
        )}
      </div>
      {editing && touchUi && <EditBar api={cm} />}
      {saveAs && <SaveAs path={entry.path} sep={sep} onSave={saveAsPath} onClose={() => setSaveAs(false)} />}
    </div>
  );
}

/** 电脑上编辑 Markdown：右边跟着显示排版效果（停手 0.3 秒刷新一次） */
function MdLive({ draft, dirty, imageUrl }) {
  const [text, setText] = useState(draft.current);
  useEffect(() => {
    const timer = setInterval(() => setText((x) => (x === draft.current ? x : draft.current)), 300);
    return () => clearInterval(timer);
  }, [draft]);
  void dirty;
  return (
    <div className="fv-md-live">
      <Markdown text={text} imageUrl={imageUrl} />
    </div>
  );
}
