// 新标签页里的文件查看页 /view?path=…（docs/FILE_VIEWER_LAYOUT_2026-11-27.md §4）：整页只有这个文件的预览，
// 能编辑保存、对比、← → 翻同一个文件夹里的文件；刷新、收藏后回到同一个文件。
// `id`：文件属于哪个会话（远程时按会话所在的电脑转发，地址里不用写电脑）；`device`：没有会话时文件在哪台电脑；
// `art=1`：Codex 产出的文件；`line` / `to`：标出的行；`diff=1`：打开就看改动。
import { ChevronLeft, ChevronRight } from 'lucide-react';
import { useEffect, useMemo, useState } from 'react';
import { useTranslation } from 'react-i18next';

import { deviceApi, locateTerminal, plainLinksWork } from '../shared/api.js';
import { query, useRouter } from '../shared/router.jsx';
import { endpoints } from '../pages/FileBrowser.jsx';
import { Preview } from './Preview.jsx';

const nameOf = (p) => p.replace(/[\\/]+$/, '').split(/[\\/]/).pop() || p;
const sepOf = (p) => (p.includes('\\') && !p.includes('/') ? '\\' : '/');
function dirOf(p) {
  const d = p.replace(/[\\/][^\\/]*$/, '');
  if (/^[A-Za-z]:$/.test(d)) return `${d}\\`;
  return d || '/';
}

export default function ViewPage() {
  const { t } = useTranslation();
  const { search, navigate } = useRouter();
  const path = query(search, 'path') ?? '';
  const id = query(search, 'id');
  const art = query(search, 'art') === '1';
  const deviceQ = query(search, 'device');
  const line = Number(query(search, 'line')) || 0;
  const to = Number(query(search, 'to')) || line;
  const diff = query(search, 'diff') === '1';
  // 会话所在的电脑：新标签页里一开始不知道，先问到再显示（不然 /fs/read 会发到顶栏选中的那台）
  const [device, setDevice] = useState(id ? undefined : deviceQ);
  useEffect(() => {
    if (!id) return undefined;
    let alive = true;
    locateTerminal(id).then(
      (d) => alive && setDevice(d),
      () => alive && setDevice(null),
    );
    return () => {
      alive = false;
    };
  }, [id]);
  const ep = useMemo(() => {
    if (art && id) {
      const u = (p, dl) => `/instances/${id}/chat/artifact?path=${encodeURIComponent(p)}${dl ? '&download=1' : ''}`;
      return { preview: (p) => u(p), download: (p) => u(p, true), plain: plainLinksWork() };
    }
    return endpoints(id ?? undefined);
  }, [id, art]);
  // 同一个文件夹里的文件（← → 翻）
  const [siblings, setSiblings] = useState(null);
  const dir = path ? dirOf(path) : '';
  useEffect(() => {
    if (!path || art || device === undefined) return undefined;
    let alive = true;
    deviceApi(device ?? undefined)
      .get(`${ep.list(dir, false)}&limit=2000`)
      .then((data) => {
        if (!alive) return;
        const sep = sepOf(path);
        const files = (data?.entries ?? []).filter((e) => !e.is_dir).map((e) => e.path ?? (dir.endsWith(sep) ? dir + e.name : dir + sep + e.name));
        files.sort((a, b) => nameOf(a).localeCompare(nameOf(b), undefined, { numeric: true, sensitivity: 'base' }));
        setSiblings(files);
      })
      .catch(() => alive && setSiblings(null));
    return () => {
      alive = false;
    };
  }, [dir, device, art, ep]);
  const at = siblings ? siblings.indexOf(path) : -1;
  const go = (d) => {
    const next = siblings?.[at + d];
    if (!next) return;
    const q = new URLSearchParams(search);
    q.set('path', next);
    q.delete('line');
    q.delete('to');
    q.delete('diff');
    navigate(`/view?${q}`, { replace: true });
  };
  useEffect(() => {
    document.title = path ? nameOf(path) : t('fv.view_title');
  }, [path]);
  useEffect(() => {
    const key = (e) => {
      if ((e.key !== 'ArrowLeft' && e.key !== 'ArrowRight') || e.altKey || e.ctrlKey || e.metaKey || e.shiftKey) return;
      if (e.target.closest?.('input, textarea, select, [contenteditable], .cm-editor')) return;
      if (!siblings || siblings.length < 2) return;
      e.preventDefault();
      go(e.key === 'ArrowLeft' ? -1 : 1);
    };
    window.addEventListener('keydown', key);
    return () => window.removeEventListener('keydown', key);
  });
  // 关掉：脚本打开的标签页能直接关；关不掉（直接输入的地址）就回到文件所在的文件夹
  const close = () => {
    window.close();
    setTimeout(() => navigate(!id && dir ? `/files?path=${encodeURIComponent(dir)}` : '/', { replace: true }), 150);
  };
  if (!path) {
    return (
      <main className="view-page view-page-empty">
        <p className="muted">{t('fv.view_none')}</p>
      </main>
    );
  }
  const nav = siblings && siblings.length > 1 && at >= 0 && (
    <span className="chat-fv-nav">
      <button type="button" className="btn btn-plain btn-icon" onClick={() => go(-1)} disabled={at <= 0} aria-label={t('chat.file_prev')} title={`${t('chat.file_prev')} (←)`}>
        <ChevronLeft size={17} />
      </button>
      <span className="chat-note">
        {at + 1}/{siblings.length}
      </span>
      <button type="button" className="btn btn-plain btn-icon" onClick={() => go(1)} disabled={at >= siblings.length - 1} aria-label={t('chat.file_next')} title={`${t('chat.file_next')} (→)`}>
        <ChevronRight size={17} />
      </button>
    </span>
  );
  return (
    <main className="view-page">
      {device !== undefined && (
        <Preview
          key={path}
          ep={ep}
          entry={{ name: nameOf(path), path }}
          sep={sepOf(path)}
          device={device}
          focus={line ? { from: line, to: Math.max(line, to) } : null}
          diff={diff}
          extra={nav}
          onClose={close}
          onAsk={null}
          linked={false}
        />
      )}
    </main>
  );
}
